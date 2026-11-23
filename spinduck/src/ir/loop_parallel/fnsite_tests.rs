//! 门禁 7 静态面：游标 fn_site 物化的准入判定（V8-E5）。
//!
//! 准入条件：① 物化发生在 `OPEN`（两种模式同点同结果）；② `distinct_sql`
//! 绑定不含行相关源（`Field`/`Expr`——`sd_cur` 引用）；③ 物化表
//! `sd_fn*` 不被循环区域写（否则冻结快照与串行语义分叉）。

use super::analyze;
use super::{ParallelSkipReason, PurePrefixPlan};
use crate::catalog::RoutineCatalog;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{AssignTarget, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use crate::runtime::cursor::CursorAttrKind;
use crate::sql::FnSite;
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

fn sql_exec(sql: &str) -> IrStmt {
    IrStmt::SqlExec {
        sql: sql.into(),
        binds: Vec::new(),
        fn_sites: Vec::new(),
        buffer_policy: crate::ir::BufferPolicy::Immediate,
    }
}

fn fn_site(table: &str, binds: Vec<(String, BindSrc)>) -> FnSite {
    FnSite {
        table: table.into(),
        name: "fnc_get_fund_data_mode".into(),
        args: vec!["p_i_date".into()],
        distinct_sql: "SELECT DISTINCT p_i_date FROM dual".into(),
        binds,
    }
}

/// 最小游标循环；`fn_sites` 注入游标源（其余同 gate_tests 骨架）。
fn cursor_prog_with_sites(fn_sites: Vec<FnSite>, body: Vec<IrStmt>, suffix: Vec<IrStmt>) -> IrProg {
    let cursor = vn("c");
    let mut code = Vec::new();
    let fetch_pc = code.len() + 1;
    let cond_pc = code.len() + 2;
    code.extend([
        IrStmt::OpenCursor {
            cursor: cursor.clone(),
            source: CursorSource::Static {
                sql: CURSOR_SQL.into(),
                binds: Vec::new(),
                fn_sites,
            },
        },
        IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(vn("rec"))],
            record: true,
        },
        IrStmt::Nop,
    ]);
    code.extend(body);
    code.push(IrStmt::Jump(fetch_pc));
    let close_pc = code.len();
    code.push(IrStmt::CloseCursor {
        cursor: cursor.clone(),
    });
    code[cond_pc] = IrStmt::JumpIf {
        cond: IrExpr::CursorAttr {
            cursor,
            kind: CursorAttrKind::NotFound,
        },
        to: close_pc + 1,
    };
    code.extend(suffix);
    IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    }
}

fn region_of(prog: &IrProg) -> CursorRegion {
    crate::ir::set_oriented::region::extract(prog).expect("fixture 应可抽取区域")
}

fn analyze_ok(prog: &IrProg) -> PurePrefixPlan {
    analyze(prog, &RoutineCatalog::new(), false).expect("fixture 应通过静态门禁")
}

fn analyze_err(prog: &IrProg) -> ParallelSkipReason {
    analyze(prog, &RoutineCatalog::new(), false)
        .err()
        .expect("fixture 应被门禁拒绝")
}

/// 四条纯赋值 + 一条可预热查询（满足最小工作量）。
fn ample_body() -> Vec<IrStmt> {
    vec![
        cacheable_query("v_k"),
        assign_var("v_a", e_int(1)),
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
        assign_var("v_d", e_int(4)),
    ]
}

// ---------------------------------------------------------------------------
// V8-E5：fn_site 物化快照化准入
// ---------------------------------------------------------------------------

#[test]
fn should_admit_loop_entry_fn_site_with_var_binds() {
    // byfund 形态：distinct_sql 仅绑循环不变入参（Var），物化在 OPEN
    // 一次完成、内容对 Worker 只读——准入。
    let sites = vec![fn_site(
        "sd_fn0",
        vec![("p_i_date".into(), BindSrc::Var(vn("p_i_date")))],
    )];
    let prog = cursor_prog_with_sites(sites, ample_body(), vec![IrStmt::Return { value: None }]);
    let plan = analyze_ok(&prog);
    assert_eq!(plan.queries.len(), 1);
    let _ = region_of(&prog);
}

#[test]
fn should_reject_fn_site_with_row_dependent_binds() {
    // Field 绑定 = 逐行求值 → 物化时记录未绑定，两种模式同点失败；
    // 保守拒绝（E5：行相关 fn_site 不在快照化范围）。
    let sites = vec![fn_site(
        "sd_fn0",
        vec![(
            "p_x".into(),
            BindSrc::Field {
                rec: vn("rec"),
                field: "a".into(),
            },
        )],
    )];
    let prog = cursor_prog_with_sites(sites, ample_body(), vec![IrStmt::Return { value: None }]);
    assert_eq!(
        analyze_err(&prog),
        ParallelSkipReason::MaterializedFnSitePresent
    );
}

#[test]
fn should_reject_fn_site_table_written_in_loop() {
    // 循环内写 sd_fn0 → 串行下物化表逐行变化，Worker 冻结快照分叉 → 拒。
    let sites = vec![fn_site(
        "sd_fn0",
        vec![("p_i_date".into(), BindSrc::Var(vn("p_i_date")))],
    )];
    let mut body = ample_body();
    body.push(sql_exec("INSERT INTO sd_fn0 VALUES (1)"));
    let prog = cursor_prog_with_sites(sites, body, vec![IrStmt::Return { value: None }]);
    assert_eq!(
        analyze_err(&prog),
        ParallelSkipReason::MaterializedFnSitePresent
    );
}

#[test]
fn should_admit_fn_site_when_loop_writes_other_table() {
    // 对照：循环写别的表不影响 sd_fn0 快照 → 仍准入。
    let sites = vec![fn_site(
        "sd_fn0",
        vec![("p_i_date".into(), BindSrc::Var(vn("p_i_date")))],
    )];
    let mut body = ample_body();
    body.push(sql_exec("INSERT INTO dat_other VALUES (1)"));
    let prog = cursor_prog_with_sites(sites, body, vec![IrStmt::Return { value: None }]);
    analyze_ok(&prog);
}
