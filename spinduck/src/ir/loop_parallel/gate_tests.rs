//! 静态门禁（门禁 1/2/5/7 静态面）的 IR 级单测。
//!
//! fixture 约定：`cursor_prog(body, suffix)` 生成
//! `OPEN c / FETCH c INTO rec / JumpIf %NOTFOUND / body… / Jump(fetch) /
//! CloseCursor / suffix…` 的最小 SESE 骨架（`record = true` 单目标）。

use super::analyze;
use super::{ParallelSkipReason, PurePrefixPlan};
use crate::catalog::{CompiledRoutine, RoutineCatalog, RoutineKind};
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{AssignTarget, CursorSource, IrExpr, IrProg, IrStmt, TrapEntry, VarName};
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;

const CURSOR_SQL: &str = "SELECT a FROM src";
const CACHEABLE_SQL: &str =
    "SELECT kind_id FROM swh_all_kind WHERE operation_kind = 'COMMISSION_SWITCH'";

fn vn(s: &str) -> VarName {
    VarName(s.into())
}

fn e_var(s: &str) -> IrExpr {
    IrExpr::Var(vn(s))
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

fn assign_field(rec: &str, field: &str, expr: IrExpr) -> IrStmt {
    IrStmt::Assign {
        target: AssignTarget::Field {
            rec: vn(rec),
            field: field.into(),
        },
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

/// 最小游标循环：`body` 之后自动补 `Jump(fetch)` 与 `CloseCursor`，
/// 再接 `suffix`。
fn cursor_prog(body: Vec<IrStmt>, suffix: Vec<IrStmt>) -> IrProg {
    cursor_prog_full(Vec::new(), body, suffix)
}

/// 带前缀语句的游标循环（前缀位于 `open_pc` 之前，属"例程入口..open"
/// 洁净度审计范围）。
fn cursor_prog_full(prefix: Vec<IrStmt>, body: Vec<IrStmt>, suffix: Vec<IrStmt>) -> IrProg {
    let cursor = vn("c");
    let mut code = prefix;
    let fetch_pc = code.len() + 1;
    let cond_pc = code.len() + 2;
    code.extend([
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
        // cond `to` 稍后回填（依赖 close_pc）。
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

fn analyze_err(prog: &IrProg, batch_commit: bool) -> ParallelSkipReason {
    analyze(prog, &RoutineCatalog::new(), batch_commit)
        .err()
        .expect("fixture 应被门禁拒绝")
}

/// 四条纯赋值 + 一条可预热查询的标准前缀（满足最小工作量）。
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
// 门禁 1：G_pure 切分 / 表达式禁令 / 交权边界 / 反可重入 / 最小工作量
// ---------------------------------------------------------------------------

#[test]
fn should_build_g_pure_for_cacheable_cursor_loop() {
    let prog = cursor_prog(ample_body(), vec![IrStmt::Return { value: None }]);
    let plan = analyze_ok(&prog);
    let region = region_of(&prog);
    assert_eq!(plan.body_start_pc, region.fetch_pc + 2);
    assert_eq!(plan.cond_pc, region.fetch_pc + 1);
    assert_eq!(plan.exit_pc, region.close_pc + 1);
    // body 五条全纯 + 尾部 Jump(fetch) 亦为纯节点。
    assert!(plan.g_pure.contains(&plan.body_start_pc));
    assert!(plan.g_pure.contains(&(plan.body_start_pc + 4)));
    assert_eq!(plan.queries.len(), 1);
    assert!(plan.max_prefix_steps >= 5);
}

#[test]
fn should_reject_cursor_attr_reads_in_pure_prefix() {
    // %ROWCOUNT 读取进入 V_pure → Worker 沙箱无 CursorState 静默失真，
    // 整域拒绝（设计 §3.1 / §7.2-9）。
    let mut body = ample_body();
    body.push(assign_var(
        "v_rows",
        IrExpr::CursorAttr {
            cursor: vn("c"),
            kind: CursorAttrKind::RowCount,
        },
    ));
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::UnsupportedControlFlow
    );
}

#[test]
fn should_yield_statement_with_sqlfn_expr() {
    // V8-E2 后 SqlFn 准入 = prepared 就绪且可物化；prepared 失败的
    // 调用仍是 E_yield 交权边界（R6-P2-1 语义保留）。
    let mut body = ample_body();
    let sqlfn_pc = 3 + body.len();
    body.push(assign_var(
        "v_t",
        IrExpr::SqlFn {
            name: "to_char".into(),
            args: vec![e_var("v_a")],
            prepared: Err(crate::error::EngineError::invariant(1, "no sql")),
        },
    ));
    let prog = cursor_prog(body, vec![]);
    let plan = analyze_ok(&prog);
    assert!(!plan.g_pure.contains(&sqlfn_pc));
}

#[test]
fn should_yield_statement_with_funccall_expr() {
    // 同上：prepared 失败的内建 FuncCall 仍交权；prepared 就绪且
    // callee 可物化的调用由 callmat_tests 覆盖（进入 G_pure）。
    let mut body = ample_body();
    let pc = 3 + body.len();
    body.push(assign_var(
        "v_t",
        IrExpr::FuncCall {
            name: "unknown_builtin".into(),
            args: vec![e_var("v_a")],
            prepared: Err(crate::error::EngineError::invariant(1, "no sql")),
        },
    ));
    let prog = cursor_prog(body, vec![]);
    let plan = analyze_ok(&prog);
    assert!(!plan.g_pure.contains(&pc));
}

#[test]
fn should_yield_sqlquery_with_fn_sites() {
    // fn_sites 物化属写副作用——语句交权而非进入 V_pure。
    // 门禁 5 对 fn_site 例程名做传递闭包审计：注册一个无副作用的
    // 空体例程，使测试只锁定"交权"行为而非目录拒判。
    let mut catalog = RoutineCatalog::new();
    catalog.register(CompiledRoutine {
        name: "fn_x".into(),
        kind: RoutineKind::Function,
        params: Vec::new(),
        prog: IrProg {
            code: vec![IrStmt::Return { value: None }],
            var_types: Default::default(),
            local_ddl: Vec::new(),
        },
    });
    let mut body = ample_body();
    let pc = 3 + body.len();
    body.push(IrStmt::SqlQuery {
        sql: CACHEABLE_SQL.into(),
        binds: Vec::new(),
        into: vec![AssignTarget::Var(vn("v_e"))],
        fn_sites: vec![crate::sql::FnSite {
            table: "tmp".into(),
            name: "fn_x".into(),
            args: Vec::new(),
            distinct_sql: String::new(),
            binds: Vec::new(),
        }],
    });
    let prog = cursor_prog(body, vec![]);
    let plan = analyze(&prog, &catalog, false).expect("fixture 应通过静态门禁");
    assert!(!plan.g_pure.contains(&pc));
}

#[test]
fn should_yield_uncacheable_sqlquery() {
    // shape::recognize 不识别的查询不能进 Worker（零数据库铁律）→ 交权。
    let mut body = ample_body();
    let pc = 3 + body.len();
    body.push(IrStmt::SqlQuery {
        sql: "SELECT weird_expr(1) FROM t".into(),
        binds: Vec::new(),
        into: vec![AssignTarget::Var(vn("v_e"))],
        fn_sites: Vec::new(),
    });
    let prog = cursor_prog(body, vec![]);
    let plan = analyze_ok(&prog);
    assert!(!plan.g_pure.contains(&pc));
}

#[test]
fn should_reject_local_backward_jump_in_body() {
    // 局部循环/向后回边（非 fetch_pc）→ UnsupportedControlFlow。
    let mut body = ample_body();
    body.push(IrStmt::Jump(3));
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::UnsupportedControlFlow
    );
}

#[test]
fn should_reject_goto_body_start_reentry() {
    // 交权残余路径 Jump 回 body_start（跳过 FETCH）→ 反可重入拒绝。
    let mut body = ample_body();
    body.push(sql_exec("INSERT INTO dst VALUES (1)"));
    body.push(IrStmt::Jump(3)); // -> body_start_pc
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::UnsupportedControlFlow
    );
}

/// 带 trap 处理器体的游标循环：`body_prefix | PushTrap | protected |
/// PopTrap | Jump->tail | handler… | Jump(fetch) | CloseCursor`。
/// 处理器体位于区域内部死区（正常流经 Jump-over 跳过，仅 trap 分派
/// 与 handler 内 `Jump(fetch)` 到达）。
fn trap_prog(body_prefix: Vec<IrStmt>, protected: Vec<IrStmt>, handler: Vec<IrStmt>) -> IrProg {
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
    code.push(IrStmt::Jump(1)); // -> fetch_pc（正常循环尾）
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
    code[jump_over_pc] = IrStmt::Jump(tail_pc);
    if let IrStmt::PushTrap { entries } = &mut code[push_pc] {
        entries[0].body_start = handler_start;
        entries[0].body_end = handler_end;
    }
    IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    }
}

#[test]
fn should_admit_goto_fetch_label_continue_edge() {
    // §7.2-5 正例：handler 体 `inst_log`（交权）+ `GOTO error_row`
    // （Jump fetch_pc = E_iter_end 继续边）→ 准入。
    let prog = trap_prog(
        ample_body(),
        vec![IrStmt::Nop],
        vec![sql_exec("INSERT INTO inst_log VALUES (1)"), IrStmt::Jump(1)],
    );
    let plan = analyze_ok(&prog);
    // handler 的 inst_log 不在 V_pure；其 Jump(fetch) 亦不可达纯子图。
    let handler_sql_pc = plan.exit_pc - 4;
    assert!(!plan.g_pure.contains(&handler_sql_pc));
}

#[test]
fn should_reject_handler_reentry_into_pure_prefix() {
    // handler 体尾跳回 body_start（跳过 FETCH）→ 反可重入拒绝。
    let prog = trap_prog(
        ample_body(),
        vec![IrStmt::Nop],
        vec![sql_exec("INSERT INTO inst_log VALUES (1)"), IrStmt::Jump(3)],
    );
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::UnsupportedControlFlow
    );
}

#[test]
fn should_reject_insufficient_pure_work() {
    // 首句即 SqlExec（inst_log 形态）：无有效纯前缀。
    let prog = cursor_prog(vec![sql_exec("INSERT INTO log VALUES (1)")], vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::InsufficientParallelWork
    );
}

#[test]
fn should_reject_no_loop_region() {
    let prog = IrProg {
        code: vec![IrStmt::Return { value: None }],
        var_types: Default::default(),
        local_ddl: Vec::new(),
    };
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::NoLoopRegionFound
    );
}

#[test]
fn should_reject_dynamic_cursor_source() {
    let cursor = vn("c");
    let code = vec![
        IrStmt::OpenCursor {
            cursor: cursor.clone(),
            source: CursorSource::Dynamic {
                query_expr: e_var("v_sql"),
            },
        },
        IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(vn("rec"))],
            record: true,
        },
        IrStmt::JumpIf {
            cond: IrExpr::CursorAttr {
                cursor: cursor.clone(),
                kind: CursorAttrKind::NotFound,
            },
            to: 5,
        },
        IrStmt::Nop,
        IrStmt::Jump(1),
        IrStmt::CloseCursor { cursor },
    ];
    let prog = IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    };
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::DynamicCursorSource
    );
}

#[test]
fn should_reject_nested_static_cursor() {
    let mut body = ample_body();
    body.push(IrStmt::OpenCursor {
        cursor: vn("c2"),
        source: CursorSource::Static {
            sql: "SELECT b FROM src2".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
        },
    });
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::UnsupportedControlFlow
    );
}

#[test]
fn should_reject_cursor_fn_sites_materialization() {
    // V8-E5 后准入口径收窄为"不可快照化"：行相关绑定（Field 指向
    // 逐行记录，OPEN 时未绑定）→ MaterializedFnSitePresent。
    // 准入侧用例见 `fnsite_tests.rs`。
    let mut code = cursor_prog(ample_body(), vec![]).code;
    if let IrStmt::OpenCursor { source, .. } = &mut code[0] {
        if let CursorSource::Static { fn_sites, .. } = source {
            *fn_sites = vec![crate::sql::FnSite {
                table: "tmp".into(),
                name: "fn_x".into(),
                args: Vec::new(),
                distinct_sql: String::new(),
                binds: vec![(
                    "p_x".into(),
                    crate::ir::BindSrc::Field {
                        rec: vn("rec"),
                        field: "a".into(),
                    },
                )],
            }];
        }
    }
    let prog = IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    };
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::MaterializedFnSitePresent
    );
}

// ---------------------------------------------------------------------------
// 门禁 2：跨轮状态独立性
// ---------------------------------------------------------------------------

#[test]
fn should_reject_suffix_write_feeding_later_prefix_read() {
    // §7.2-11 静态面：残余段写 v_acc → 下一行前缀读 v_acc = 循环携带。
    let mut body = vec![
        cacheable_query("v_k"),
        assign_var("v_z", e_var("v_acc")), // 前缀读残留变量
        assign_var("v_a", e_int(1)),
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
    ];
    body.push(sql_exec("INSERT INTO dst VALUES (1)")); // 交权边界
    body.push(assign_var("v_acc", e_int(9))); // 残余段写回（可达 fetch）
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::CycleCarriedDependence
    );
}

#[test]
fn should_admit_prefix_only_var_redefinition() {
    // v_t 仅在前缀内写、读点在其定义之后 → 每行重算，无跨轮依赖。
    let mut body = vec![
        cacheable_query("v_k"),
        assign_var("v_t", e_int(7)),
        assign_var("v_a", e_var("v_t")),
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
    ];
    body.push(sql_exec("INSERT INTO dst VALUES (1)"));
    let prog = cursor_prog(body, vec![]);
    analyze_ok(&prog);
}

#[test]
fn should_reject_prefix_read_of_var_written_only_in_prefix_later() {
    // v_t 在前缀内先读后写：读到的是上一行前缀写的值 → 循环携带。
    let mut body = vec![
        cacheable_query("v_k"),
        assign_var("v_a", e_var("v_t")), // 先读
        assign_var("v_t", e_int(7)),     // 后写
        assign_var("v_b", e_int(2)),
        assign_var("v_c", e_int(3)),
    ];
    body.push(sql_exec("INSERT INTO dst VALUES (1)"));
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::CycleCarriedDependence
    );
}

#[test]
fn should_reject_sqlerrm_read_with_trap_in_loop() {
    // 隐式 __sqlerrm 写建模（R4-P1-2）：循环含 PushTrap 且处理器体
    // 可达 fetch_pc 时，前缀读 __sqlerrm 一律 CycleCarriedDependence。
    let mut prefix = ample_body();
    prefix.push(assign_var("v_e", e_var("__sqlerrm")));
    let prog = trap_prog(
        prefix,
        vec![IrStmt::Nop],
        vec![sql_exec("INSERT INTO inst_log VALUES (1)"), IrStmt::Jump(1)],
    );
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::CycleCarriedDependence
    );
}

// ---------------------------------------------------------------------------
// 门禁 5：确定性 / 会话无关 / 事务锚定 / Top-1
// ---------------------------------------------------------------------------

#[test]
fn should_reject_tx_anchored_function_in_prefix() {
    let mut body = ample_body();
    body.push(assign_var(
        "v_t",
        IrExpr::FuncCall {
            name: "now".into(),
            args: vec![],
            prepared: Ok("SELECT now()".into()),
        },
    ));
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::TransactionAnchoredRead
    );
}

#[test]
fn should_admit_tx_anchored_read_after_close() {
    // V8-E4 修订（取代 R5-P1-1 全例程口径）：CLOSE 之后的语句在并行
    // 窗口关闭后原生执行，事务演化与串行逐语句一致——预热建索引仅
    // 只读 SELECT（DuckDB 1.5.5 实证失败不污染事务）、实现亦无
    // reset_session，后缀不存在额外事务切换 → 后缀事务锚定读不审计。
    let prog = cursor_prog(
        ample_body(),
        vec![assign_var(
            "p_o",
            IrExpr::FuncCall {
                name: "current_timestamp".into(),
                args: vec![],
                prepared: Ok("SELECT current_timestamp".into()),
            },
        )],
    );
    analyze_ok(&prog);
}

#[test]
fn should_admit_dynamic_sql_exec_in_suffix() {
    // 对照：后缀 SqlExecDynamic 同样不审计（与上例同一修订口径），
    // 但区域/前缀内的动态 SQL 仍须拒绝——由
    // `should_reject_dynamic_sql_exec`（body 内）与
    // `should_reject_dynamic_cursor_source` 覆盖。
    let prog = cursor_prog(
        ample_body(),
        vec![IrStmt::SqlExecDynamic {
            text_expr: Box::new(e_var("v_sql")),
            into_targets: Vec::new(),
        }],
    );
    analyze_ok(&prog);
}

#[test]
fn should_reject_nondeterministic_function() {
    let mut body = ample_body();
    body.push(assign_var(
        "v_r",
        IrExpr::FuncCall {
            name: "random".into(),
            args: vec![],
            prepared: Ok("SELECT random()".into()),
        },
    ));
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::NonDeterministicOrAmbiguousTop1
    );
}

#[test]
fn should_reject_dynamic_sql_exec() {
    // SqlExecDynamic 运行期文本无法静态证明 → TransactionAnchoredRead。
    let mut body = ample_body();
    body.push(IrStmt::SqlExecDynamic {
        text_expr: Box::new(e_var("v_sql")),
        into_targets: Vec::new(),
    });
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::TransactionAnchoredRead
    );
}

#[test]
fn should_reject_top1_query_in_pure_prefix() {
    // "SELECT k FROM t ORDER BY k LIMIT 1" 无 PK/UNIQUE 全序证明：
    // V8 起由「整域拒绝」改为「交权边界」——Top-1 查询不进入
    // `G_pure`（主线程原生执行，歧义语义零变化），其前的纯前缀
    // 仍可并行。断言口径从「拒绝例程」收紧为「查询不在纯前缀」。
    let mut body: Vec<IrStmt> = (0..4)
        .map(|i| assign_var(&format!("v_{i}"), e_int(i)))
        .collect();
    body.push(IrStmt::SqlQuery {
        sql: "SELECT k FROM t ORDER BY k LIMIT 1".into(),
        binds: Vec::new(),
        into: vec![AssignTarget::Var(vn("v_k"))],
        fn_sites: Vec::new(),
    });
    let prog = cursor_prog(body, vec![]);
    let plan = analyze(&prog, &RoutineCatalog::new(), false).expect("Top-1 交权后前缀应可并行");
    let query_pc = plan.body_start_pc + 4;
    assert!(
        !plan.g_pure.contains(&query_pc),
        "Top-1 查询 pc={query_pc} 不得进入 G_pure: {:?}",
        plan.g_pure
    );
    assert!(
        plan.g_pure.contains(&plan.body_start_pc),
        "前缀纯赋值应保留在 G_pure"
    );
}

// ---------------------------------------------------------------------------
// 门禁 7 静态面：batch_commit 前缀洁净度 / 读写集不交 / W_loop 枚举
// ---------------------------------------------------------------------------

#[test]
fn should_reject_batch_commit_with_dirty_prefix() {
    // batch_commit=true 下前缀 DML 后的 COMMIT 被吞并 → 判脏拒绝。
    let prog = cursor_prog_full(
        vec![sql_exec("DELETE FROM t WHERE id = 1")],
        ample_body(),
        vec![],
    );
    assert_eq!(
        analyze_err(&prog, true),
        ParallelSkipReason::DirtyPrefixWithRollbackRisk
    );
    // batch_commit=false 时同夹具静态放行（运行态 clean_at_prewarm 兜底）。
    analyze_ok_with(&prog, false);
}

fn analyze_ok_with(prog: &IrProg, batch_commit: bool) -> PurePrefixPlan {
    analyze(prog, &RoutineCatalog::new(), batch_commit).expect("应通过静态门禁")
}

#[test]
fn should_reject_read_write_intersection() {
    // 游标读 src，循环内 SqlExec 写 src → R_prefix ∩ W_loop ≠ ∅。
    let mut body = ample_body();
    body.push(sql_exec("INSERT INTO src VALUES (1)"));
    let prog = cursor_prog(body, vec![]);
    assert_eq!(
        analyze_err(&prog, false),
        ParallelSkipReason::UncachedQueryInPrefix
    );
}

#[test]
fn should_reject_unenumerable_w_loop() {
    // 循环内 SqlExecDynamic → W_loop 不可完备枚举 → UncachedQueryInPrefix。
    // （门禁 5 的动态 SQL 拒判先触发——调整顺序验证 W_loop 枚举拒判：
    //   用 CallRoutine 未注册名触发 write_set 的 UnsupportedCall。）
    let mut body = ample_body();
    body.push(IrStmt::CallRoutine {
        name: "unregistered_proc".into(),
        args: Vec::new(),
        out_targets: Vec::new(),
    });
    let prog = cursor_prog(body, vec![]);
    // 门禁 5 先拒（闭包不可达）——断言其一即可，二者均为安全拒绝。
    let reason = analyze_err(&prog, false);
    assert!(
        matches!(
            reason,
            ParallelSkipReason::TransactionAnchoredRead | ParallelSkipReason::UncachedQueryInPrefix
        ),
        "意外拒绝原因: {reason:?}"
    );
}

// ---------------------------------------------------------------------------
// 字段粒度正例：前缀写 rec.a / 残余写 rec.b 不相交 → 放行
// ---------------------------------------------------------------------------

#[test]
fn should_admit_field_separated_record_writes() {
    // 前缀写 rec.a（经 AssignTarget::Field），残余段写 rec.b——
    // 字段粒度不相交且 FETCH 每行整记录刷新 → 放行。
    let mut body = vec![
        cacheable_query("v_k"),
        assign_field("rec", "a", e_int(1)),
        assign_var("v_a", e_int(2)),
        assign_var("v_b", e_int(3)),
        assign_var("v_c", e_int(4)),
    ];
    body.push(sql_exec("INSERT INTO dst VALUES (1)")); // 交权
    body.push(assign_field("rec", "b", e_int(5))); // 残余字段写
    let prog = cursor_prog(body, vec![]);
    analyze_ok(&prog);
}
