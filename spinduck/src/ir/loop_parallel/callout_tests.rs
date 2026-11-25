//! CallRoutine out_targets 精度门禁测试（V8-E1 前置修复）。
//!
//! `ir/stmt.rs` 给每个可写实参都填 `out_targets` 语法位（编译期不知
//! callee 签名），但运行时 `interp/routine.rs` 只对 `ParamMode::Out`/
//! `InOut` 形参写回。`exprs::stmt_defs` 若把全部语法位计为写，会把
//! `inst_log(p_i_date, …)` 这类 IN 实参误判为循环写，污染
//! `query_cacheable` 的 range_key 循环不变性判定（purchase 实测根因）。

use super::analyze;
use super::PurePrefixPlan;
use crate::catalog::{CompiledRoutine, ParamMode, RoutineCatalog, RoutineKind, RoutineParam};
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{AssignTarget, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, TrapEntry, VarName};
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;

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

/// purchase Q1 同构：range bind `$v_p_i_date`（Var）+ 行键 bind
/// `$v_r_get_purchase__security_id`（Field）。range_binds 只含前者。
const ROW_KEY_SQL: &str = "SELECT coalesce(t.purchase_days, 1) FROM par_sys_purchase AS t \
     WHERE $v_p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date \
     AND t.security_id = $v_r_get_purchase__security_id";

fn row_key_query(into: &str) -> IrStmt {
    IrStmt::SqlQuery {
        sql: ROW_KEY_SQL.into(),
        binds: vec![
            ("$v_p_i_date".into(), BindSrc::Var(vn("p_i_date"))),
            (
                "$v_r_get_purchase__security_id".into(),
                BindSrc::Field {
                    rec: vn("r_get_purchase"),
                    field: "security_id".into(),
                },
            ),
        ],
        into: vec![AssignTarget::Var(vn(into))],
        fn_sites: Vec::new(),
    }
}

/// `inst_log` 同构：8 实参全部可写语法位；callee 形参模式由
/// `inst_log_catalog_mode0` 控制（p0 可切 Out/In，p7 恒 InOut）。
fn inst_log_call() -> IrStmt {
    IrStmt::CallRoutine {
        name: "pkg_split_trade_log.inst_log".into(),
        args: (0..8).map(|_| e_int(0)).collect(),
        out_targets: (0..8)
            .map(|i| {
                if i == 0 {
                    Some(AssignTarget::Var(vn("p_i_date")))
                } else {
                    None
                }
            })
            .collect(),
    }
}

fn inst_log_catalog_mode0(mode0: ParamMode) -> RoutineCatalog {
    let mut catalog = RoutineCatalog::new();
    catalog.register(CompiledRoutine {
        name: "pkg_split_trade_log.inst_log".into(),
        kind: RoutineKind::Procedure,
        params: (0..8)
            .map(|i| RoutineParam {
                name: format!("p{i}"),
                mode: if i == 0 {
                    mode0
                } else if i == 7 {
                    ParamMode::InOut
                } else {
                    ParamMode::In
                },
                default: None,
                ty: None,
            })
            .collect(),
        prog: IrProg {
            code: vec![IrStmt::Return { value: None }],
            var_types: Default::default(),
            local_ddl: Vec::new(),
        },
    });
    catalog
}

/// 带 trap 处理器体的游标循环（与 gate_tests::trap_prog 同构）：
/// `body_prefix | PushTrap | protected | PopTrap | Jump-over |
/// handler… | Jump(fetch) | CloseCursor`。
fn trap_prog(body_prefix: Vec<IrStmt>, protected: Vec<IrStmt>, handler: Vec<IrStmt>) -> IrProg {
    let cursor = vn("c");
    let mut code = vec![
        IrStmt::OpenCursor {
            cursor: cursor.clone(),
            source: CursorSource::Static {
                sql: "SELECT a FROM src".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
        },
        IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(vn("r_get_purchase"))],
            record: true,
        },
        IrStmt::Nop,
    ];
    code.extend(body_prefix);
    let push_pc = code.len();
    code.push(IrStmt::PushTrap {
        entries: vec![TrapEntry {
            conds: vec!["others".into()],
            body_start: 0,
            body_end: 0,
        }],
    });
    code.extend(protected);
    code.push(IrStmt::PopTrap);
    let jump_over_pc = code.len();
    code.push(IrStmt::Jump(0));
    let handler_start = code.len();
    code.extend(handler);
    let handler_end = code.len();
    let tail_pc = code.len();
    code.push(IrStmt::Jump(1)); // -> fetch_pc
    let close_pc = code.len();
    code.push(IrStmt::CloseCursor { cursor });
    code[2] = IrStmt::JumpIf {
        cond: IrExpr::CursorAttr {
            cursor: vn("c"),
            kind: CursorAttrKind::NotFound,
        },
        to: close_pc + 1,
    };
    code[jump_over_pc] = IrStmt::Jump(tail_pc);
    if let IrStmt::PushTrap { entries } = &mut code[push_pc] {
        entries[0].body_start = handler_start;
        entries[0].body_end = handler_end;
    }
    let mut prog = IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    };
    // p_i_date 槽位类型：供 call_is_noop 的 OUT 实参槽位校验。
    prog.var_types
        .insert("p_i_date".into(), crate::types::SpinType::Int);
    prog
}

fn region_of(prog: &IrProg) -> CursorRegion {
    crate::ir::set_oriented::region::extract(prog).expect("fixture 应可抽取区域")
}

/// IN 形参实参不得计入 loop_writes：`p_i_date` 是行键查询的 range
/// bind，只有真实 OUT/INOUT 写回才破坏其循环不变性。
#[test]
fn should_not_count_in_param_call_args_as_loop_writes() {
    let body_prefix = vec![
        row_key_query("v_days"),
        assign_var("v_a", e_int(1)),
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
        assign_var("v_d", e_int(4)),
    ];
    let prog = trap_prog(
        body_prefix,
        vec![IrStmt::Nop],
        vec![inst_log_call(), IrStmt::Jump(1)],
    );
    let _ = region_of(&prog);
    let catalog = inst_log_catalog_mode0(ParamMode::In);
    let plan: PurePrefixPlan = analyze(&prog, &catalog, false).expect("应通过静态门禁");
    assert_eq!(plan.queries.len(), 1, "行键查询应进入预热计划");
}

/// 对照：callee 首形参声明 `Out` 时 `p_i_date` 真实写回——range
/// bind 非循环不变，行键查询必须交权（queries 为空）。
#[test]
fn should_count_out_param_call_args_as_loop_writes() {
    let body_prefix = vec![
        assign_var("v_a", e_int(1)),
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
        assign_var("v_d", e_int(4)),
        row_key_query("v_days"),
    ];
    let prog = trap_prog(
        body_prefix,
        vec![IrStmt::Nop],
        vec![inst_log_call(), IrStmt::Jump(1)],
    );
    let catalog = inst_log_catalog_mode0(ParamMode::Out);
    let plan = analyze(&prog, &catalog, false).expect("门禁不拒绝，仅交权");
    assert!(
        plan.queries.is_empty(),
        "OUT 形参实参计写 → p_i_date 非不变 → 行键查询交权"
    );
}
