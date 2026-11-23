//! 反可重入证明（trap 分派边）的精度守护：处理器体前向汇合到
//! `G_pure` **后继**（非受保护语句的祖先）是安全的——该汇合点只有在
//! 受保护语句原生执行时才可达，而原生执行该语句意味着 Worker 已在
//! 该点之前交权，汇合点本身尚未被 Worker 执行，不存在重复执行。
//! 仅当处理器可达 `G_pure` 中受保护语句的**祖先**（Worker 必先执行
//! 再遇交权点）时才是真重入（`gate_tests` 的 Jump(body_start) 用例）。

use super::analyze;
use super::PurePrefixPlan;
use crate::catalog::RoutineCatalog;
use crate::ir::{AssignTarget, CursorSource, IrExpr, IrProg, IrStmt, TrapEntry, VarName};
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;

const CURSOR_SQL: &str = "SELECT a FROM src";
const CACHEABLE_SQL: &str =
    "SELECT kind_id FROM swh_all_kind WHERE operation_kind = 'COMMISSION_SWITCH'";

fn vn(s: &str) -> VarName {
    VarName(s.into())
}

fn e_int(n: i64) -> IrExpr {
    IrExpr::Const(SpinValue::Int(n))
}

fn assign_var(target: &str, expr: IrExpr) -> IrStmt {
    IrStmt::Assign {
        target: AssignTarget::Var(vn(target)),
        expr,
    }
}

fn cacheable_query(into: &str) -> IrStmt {
    IrStmt::SqlQuery {
        sql: CACHEABLE_SQL.into(),
        binds: Vec::new(),
        into: vec![AssignTarget::Var(vn(into))],
        fn_sites: Vec::new(),
    }
}

/// purchase 形态骨架（两处理器，jump-over 布局同 `trap_prog`）：
/// `0 Open / 1 Fetch / 2 cond / 3 query / 4-7 assigns /
///  8 PushTrap→h1 / 9 query(受保护) / 10 PopTrap /
///  11 PushTrap→h2 / 12-14 assigns / 15 PopTrap /
///  16 Jump(21 跳处理器) / 17-18 h1: assign + Jump(11 汇合区头) /
///  19-20 h2: assign + Jump(21) / 21 Jump(fetch) / 22 Close / 23 exit`
/// h1 前向汇合到受保护查询(pc9)的**后继**汇合区(pc11)；h2 仅前向。
fn trap_merge_prog() -> IrProg {
    let cursor = vn("c");
    let mut code = vec![
        IrStmt::OpenCursor {
            cursor: cursor.clone(),
            source: CursorSource::Static {
                sql: CURSOR_SQL.into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
        },
        IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(vn("rec"))],
            record: true,
        },
        IrStmt::Nop, // cond 占位，稍后回填
        cacheable_query("v_k"),
    ];
    for i in 0..4 {
        code.push(assign_var(&format!("v_a{i}"), e_int(i as i64)));
    }
    code.push(IrStmt::PushTrap {
        entries: vec![TrapEntry {
            conds: vec!["others".into()],
            body_start: 17,
            body_end: 19,
        }],
    });
    code.push(cacheable_query("v_prot"));
    code.push(IrStmt::PopTrap);
    code.push(IrStmt::PushTrap {
        entries: vec![TrapEntry {
            conds: vec!["others".into()],
            body_start: 19,
            body_end: 21,
        }],
    });
    for i in 0..3 {
        code.push(assign_var(&format!("v_m{i}"), e_int(10 + i as i64)));
    }
    code.push(IrStmt::PopTrap);
    code.push(IrStmt::Jump(0)); // pc16 jump-over 占位 -> tail_pc
    code.push(assign_var("v_h1", e_int(0))); // pc17 h1
    code.push(IrStmt::Jump(11)); // pc18 h1 尾 -> 汇合区 PushTrap
    code.push(assign_var("v_h2", e_int(0))); // pc19 h2
    code.push(IrStmt::Jump(0)); // pc20 h2 尾占位 -> tail_pc
    code.push(IrStmt::Jump(1)); // pc21 tail -> fetch_pc
    let close_pc = code.len();
    code.push(IrStmt::CloseCursor {
        cursor: cursor.clone(),
    });
    code[2] = IrStmt::JumpIf {
        cond: IrExpr::CursorAttr {
            cursor,
            kind: CursorAttrKind::NotFound,
        },
        to: close_pc + 1,
    };
    code[16] = IrStmt::Jump(21);
    code[20] = IrStmt::Jump(21);
    IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    }
}

#[test]
fn should_admit_handler_merge_into_g_pure_descendant() {
    // h1 尾跳汇合区 PushTrap(pc11)：受保护查询(pc9)的后继而非祖先。
    // 原生到达 pc9 ⟹ Worker 已在 pc9 处交权 ⟹ pc11..15 尚未被 Worker
    // 执行 ⟹ 无重复执行 → 应准入（purchase pc50→handler54→pc56 形态）。
    let prog = trap_merge_prog();
    let plan = analyze_ok(&prog);
    assert!(plan.g_pure.contains(&11));
    assert!(plan.g_pure.contains(&9));
}

fn analyze_ok(prog: &IrProg) -> PurePrefixPlan {
    analyze(prog, &RoutineCatalog::new(), false).expect("fixture 应通过静态门禁")
}

/// IF 分支汇合形态（SO route-IF 的 LP 对应面）：
/// `3 query / 4 JumpIfNot(cond→7) / 5 assign(THEN) / 6 Jump(9) /
///  7 SqlExec(ELSE 交权) / 8 assign(ELSE 尾) / 9 assign(汇合,纯) /
///  10 Jump(fetch) / 11 Close`。
/// ELSE 臂在 pc7 交权，其续跑路径经 pc8 到达汇合点 pc9——pc9 同时
/// 被 THEN 臂 `Jump(9)` 纳入 `G_pure`，但它是交权源 pc4 的**后代**
/// 而非祖先：Worker 走 ELSE 臂时根本没执行 pc9，主线程原生续跑
/// 恰执行一次 → 应准入（旧「任一 G_pure 可达即拒」会误拒）。
fn yield_merge_prog() -> IrProg {
    let cursor = vn("c");
    let mut code = vec![
        IrStmt::OpenCursor {
            cursor: cursor.clone(),
            source: CursorSource::Static {
                sql: CURSOR_SQL.into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
        },
        IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(vn("rec"))],
            record: true,
        },
        IrStmt::Nop, // pc2 cond 占位
        cacheable_query("v_k"),
        IrStmt::JumpIfNot {
            cond: IrExpr::Const(SpinValue::Bool(true)),
            to: 7,
        },
        assign_var("v_x", e_int(1)),
        IrStmt::Jump(9),
        IrStmt::SqlExec {
            sql: "INSERT INTO dst VALUES (1)".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
            buffer_policy: crate::ir::BufferPolicy::Immediate,
        },
        assign_var("v_y", e_int(2)),
        assign_var("v_z", e_int(3)),
        IrStmt::Jump(1),
    ];
    let close_pc = code.len();
    code.push(IrStmt::CloseCursor {
        cursor: cursor.clone(),
    });
    code[2] = IrStmt::JumpIf {
        cond: IrExpr::CursorAttr {
            cursor,
            kind: CursorAttrKind::NotFound,
        },
        to: close_pc + 1,
    };
    IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    }
}

#[test]
fn should_admit_yield_merge_into_g_pure_descendant() {
    let prog = yield_merge_prog();
    let plan = analyze_ok(&prog);
    // 汇合点 pc9/10 经 THEN 臂进入 G_pure；ELSE 交权边 (4→7) 的续跑
    // 可达集与 pc4 的祖先集不相交 → 准入。
    assert!(plan.g_pure.contains(&9));
    assert!(!plan.g_pure.contains(&7));
}
