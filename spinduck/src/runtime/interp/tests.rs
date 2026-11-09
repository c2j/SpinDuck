// 解释器测试模块（自 interp.rs 拆分，行为不变，AGENTS §3）。
use super::*;
use crate::exec::{BindMap, FakeExecutor, SqlExecutor};
use crate::ir::compile_stmts;
use crate::ir::VarName;
use crate::value::SpinValue;
use ogsql_parser::ast::plpgsql::PlStatement;

fn run_pl(stmts: Vec<PlStatement>) -> (Frame, Result<RunOutcome, EngineError>) {
    run_pl_with(stmts, &FakeExecutor::default())
}

fn run_pl_with(
    stmts: Vec<PlStatement>,
    ex: &FakeExecutor,
) -> (Frame, Result<RunOutcome, EngineError>) {
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let out = run(&prog, &mut f, ex);
    (f, out)
}

fn var(name: &str) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::PlVariable(vec![ident(name)])
}

fn ident(name: &str) -> ogsql_parser::ast::Ident {
    ogsql_parser::ast::Ident {
        value: name.to_string(),
        quote_style: None,
    }
}

fn lit_str(s: &str) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::String(s.to_string()))
}

fn lit_int(i: i64) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(i))
}

fn bin(
    l: ogsql_parser::ast::Expr,
    op: &str,
    r: ogsql_parser::ast::Expr,
) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::BinaryOp {
        left: Box::new(l),
        op: op.to_string(),
        right: Box::new(r),
    }
}

fn assign(target: ogsql_parser::ast::Expr, e: ogsql_parser::ast::Expr) -> PlStatement {
    PlStatement::Assignment {
        target,
        expression: e,
    }
}

#[test]
fn should_assign_variables_with_loose_string_coercion() {
    let (f, out) = run_pl(vec![
        assign(var("v_name"), lit_str("prc_trd_hz_byfund")),
        assign(var("v_step"), lit_int(1)),
    ]);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_name".into())),
        Some(&SpinValue::Str("prc_trd_hz_byfund".into()))
    );
    assert_eq!(f.get(&VarName("v_step".into())), Some(&SpinValue::Int(1)));
}

#[test]
fn should_concat_strings_for_log_message_pattern() {
    let (f, out) = run_pl(vec![assign(
        var("v_msg"),
        bin(lit_str("删除出错!"), "||", lit_str("SQLERRM")),
    )]);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_msg".into())),
        Some(&SpinValue::Str("删除出错!SQLERRM".into()))
    );
}

#[test]
fn should_run_if_else_branches() {
    use ogsql_parser::ast::plpgsql::{PlElsif, PlIfStmt};
    let stmt = PlStatement::If(ogsql_parser::ast::Spanned::new(
        PlIfStmt {
            condition: bin(var("v_cnt"), "=", lit_int(0)),
            then_stmts: vec![assign(var("v_result"), lit_str("zero"))],
            elsifs: vec![PlElsif {
                condition: bin(var("v_cnt"), ">", lit_int(0)),
                stmts: vec![assign(var("v_result"), lit_str("pos"))],
            }],
            else_stmts: vec![assign(var("v_result"), lit_str("neg"))],
        },
        None,
    ));
    let (f, out) = run_pl(vec![assign(var("v_cnt"), lit_int(5)), stmt]);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_result".into())),
        Some(&SpinValue::Str("pos".into()))
    );
}

#[test]
fn should_exit_loop_on_notfound_pattern() {
    // 模拟游标循环骨架：v_i 计数，EXIT WHEN v_i >= 3（三值逻辑外的确定性路径）
    let body = vec![
        assign(var("v_i"), bin(var("v_i"), "+", lit_int(1))),
        PlStatement::Exit {
            label: None,
            condition: Some(bin(var("v_i"), ">=", lit_int(3))),
        },
    ];
    let stmt = PlStatement::Loop(ogsql_parser::ast::Spanned::new(
        ogsql_parser::ast::plpgsql::PlLoopStmt {
            label: None,
            body,
            end_label: None,
        },
        None,
    ));
    let (f, out) = run_pl(vec![assign(var("v_i"), lit_int(0)), stmt]);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(f.get(&VarName("v_i".into())), Some(&SpinValue::Int(3)));
}

#[test]
fn should_stop_infinite_loop_at_step_budget() {
    let stmt = PlStatement::Loop(ogsql_parser::ast::Spanned::new(
        ogsql_parser::ast::plpgsql::PlLoopStmt {
            label: None,
            body: vec![],
            end_label: None,
        },
        None,
    ));
    let (_, out) = run_pl(vec![assign(var("v_i"), lit_int(0)), stmt]);
    let err = out.unwrap_err();
    assert_eq!(err.code.to_string(), "ENG-0032");
}

fn run_pl_with_budget(
    stmts: Vec<PlStatement>,
    budget: u64,
) -> (Frame, Result<RunOutcome, EngineError>) {
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &FakeExecutor::default(),
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget,
            batch_commit: false,
        },
    );
    (f, out)
}

#[test]
fn should_stop_loop_at_custom_step_budget() {
    let stmt = PlStatement::Loop(ogsql_parser::ast::Spanned::new(
        ogsql_parser::ast::plpgsql::PlLoopStmt {
            label: None,
            body: vec![],
            end_label: None,
        },
        None,
    ));
    let (_, out) = run_pl_with_budget(vec![assign(var("v_i"), lit_int(0)), stmt], 100);
    let err = out.unwrap_err();
    assert_eq!(err.code.to_string(), "ENG-0032");
    assert!(err.message.contains("(100)"), "实际消息: {}", err.message);
}

#[test]
fn should_complete_loop_within_custom_step_budget() {
    let body = vec![
        assign(var("v_i"), bin(var("v_i"), "+", lit_int(1))),
        PlStatement::Exit {
            label: None,
            condition: Some(bin(var("v_i"), ">=", lit_int(50))),
        },
    ];
    let stmt = PlStatement::Loop(ogsql_parser::ast::Spanned::new(
        ogsql_parser::ast::plpgsql::PlLoopStmt {
            label: None,
            body,
            end_label: None,
        },
        None,
    ));
    let (f, out) = run_pl_with_budget(vec![assign(var("v_i"), lit_int(0)), stmt], 1000);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(f.get(&VarName("v_i".into())), Some(&SpinValue::Int(50)));
}

#[test]
fn should_raise_business_error_with_message() {
    use ogsql_parser::ast::plpgsql::PlRaiseStmt;
    use ogsql_parser::ast::Spanned;
    let stmt = PlStatement::Raise(Spanned::new(
        PlRaiseStmt {
            level: None,
            message: Some("清除dat_trd_repurchase数据出错".into()),
            sqlstate: None,
            params: vec![],
            options: vec![],
            condname: None,
        },
        None,
    ));
    let (_, out) = run_pl(vec![stmt]);
    let err = out.unwrap_err();
    assert_eq!(err.domain, crate::error::FaultDomain::Business);
    assert!(err.message.contains("清除dat_trd_repurchase"));
}

#[test]
fn should_return_value() {
    let (_, out) = run_pl(vec![PlStatement::Return {
        expression: Some(lit_str("done")),
    }]);
    assert_eq!(
        out.unwrap(),
        RunOutcome::Returned(Some(SpinValue::Str("done".into())))
    );
}

#[test]
fn should_select_into_single_row_assigns_positionally() {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = "SELECT t.fund_code, t.cjsl INTO v_fund_code, v_cjsl FROM dat WHERE bcrq = p_i_date";
    let tokens = Tokenizer::new(sql).tokenize().unwrap();
    let parsed = Parser::new(tokens).parse().remove(0);
    let stmt = PlStatement::SqlStatement {
        span: None,
        sql_text: sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::with_rows(
        vec!["fund_code".to_string(), "cjsl".to_string()],
        vec![vec![SpinValue::Str("F0001".into()), SpinValue::Int(100)]],
    );
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    let (f, out) = run_pl_with(vec![pre, stmt], &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_fund_code".into())),
        Some(&SpinValue::Str("F0001".into()))
    );
    assert_eq!(f.get(&VarName("v_cjsl".into())), Some(&SpinValue::Int(100)));
    // 改写产物：INTO 摘除 + 变量占位符化
    let (sql, binds) = &ex.calls()[0];
    assert!(sql.contains("$v_p_i_date"), "sql: {sql}");
    assert!(!sql.to_uppercase().contains(" INTO "));
    assert_eq!(binds["$v_p_i_date"], SpinValue::Str("20240105".into()));
}

#[test]
fn should_raise_no_data_found_on_zero_rows() {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = "SELECT t.flag INTO v_flag FROM swh_routing WHERE t.k = p_i_date";
    let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let stmt = PlStatement::SqlStatement {
        span: None,
        sql_text: sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::default(); // 无脚本 → 空结果
    let (_, out) = run_pl_with(vec![stmt], &ex);
    let err = out.unwrap_err();
    assert_eq!(err.code.to_string(), "BIZ-0100");
}

#[test]
fn should_raise_too_many_rows_on_multi_rows() {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = "SELECT t.flag INTO v_flag FROM swh_routing WHERE t.k = p_i_date";
    let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let stmt = PlStatement::SqlStatement {
        span: None,
        sql_text: sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::with_rows(
        vec!["flag".to_string()],
        vec![
            vec![SpinValue::Str("0".into())],
            vec![SpinValue::Str("1".into())],
        ],
    );
    let (_, out) = run_pl_with(vec![stmt], &ex);
    assert_eq!(out.unwrap_err().code.to_string(), "BIZ-0101");
}

#[test]
fn should_execute_dml_with_named_binds() {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = "DELETE FROM dat_fund_cjqs WHERE bcrq = p_i_date AND scdm = '001'";
    let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let stmt = PlStatement::SqlStatement {
        span: None,
        sql_text: sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::default();
    // p_i_date 为过程参数：M6 CALL 注入帧；此处以赋值前置等价模拟（参数即帧变量）
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    let (_, out) = run_pl_with(vec![pre, stmt], &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let (sql, binds) = &ex.calls()[0];
    assert!(sql.starts_with("DELETE FROM"));
    assert_eq!(binds["$v_p_i_date"], SpinValue::Str("20240105".into()));
}

#[test]
fn should_reject_sql_statements_at_m2_scope() {
    // M2 范围外：SQL 类语句**编译期**显式拒绝（M3 纳入）
    let err = compile_stmts(&[PlStatement::Sql("SELECT 1".into())]).unwrap_err();
    assert_eq!(err.domain, crate::error::FaultDomain::Engine);
    assert!(err.message.contains("not yet supported"));
}

fn run_pl_batch(
    stmts: Vec<PlStatement>,
    ex: &FakeExecutor,
    batch: bool,
) -> (Frame, Result<RunOutcome, EngineError>) {
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: batch,
        },
    );
    (f, out)
}

fn insert_stmt(table: &str, val: &str) -> PlStatement {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = format!("INSERT INTO {table} (a) VALUES ({val})");
    let parsed = Parser::new(Tokenizer::new(sql.as_str()).tokenize().unwrap())
        .parse()
        .remove(0);
    PlStatement::SqlStatement {
        span: None,
        sql_text: sql,
        statement: Box::new(parsed),
    }
}

fn commit_stmt() -> PlStatement {
    PlStatement::Commit { and_chain: false }
}

fn count_loop_body() -> Vec<PlStatement> {
    vec![
        assign(var("v_i"), bin(var("v_i"), "+", lit_int(1))),
        insert_stmt("t", "v_i"),
        commit_stmt(),
        PlStatement::Exit {
            label: None,
            condition: Some(bin(var("v_i"), ">=", lit_int(3))),
        },
    ]
}

#[test]
fn should_coalesce_inserts_and_commits_when_batch_commit_on() {
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(
        vec![
            assign(var("v_i"), lit_int(0)),
            PlStatement::Loop(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlLoopStmt {
                    label: None,
                    body: count_loop_body(),
                    end_label: None,
                },
                None,
            )),
        ],
        &ex,
        true,
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let commits = ex.calls().iter().filter(|(s, _)| s == "COMMIT").count();
    let merged = ex
        .calls()
        .iter()
        .find(|(s, _)| s.starts_with("INSERT"))
        .map(|(sql, _)| sql.clone());
    assert_eq!(commits, 0, "开关 ON 时过程内 COMMIT 必须被吞并");
    let insert_count = ex
        .calls()
        .iter()
        .filter(|(s, _)| s.starts_with("INSERT"))
        .count();
    assert_eq!(insert_count, 1, "同表 INSERT 应合并为一条多值语句");
    let tuples = merged
        .as_deref()
        .map(|s| s.matches('(').count())
        .unwrap_or(0);
    // 3 个行元组 + 1 个列清单括号
    assert_eq!(tuples, 4, "多值语句应含 3 行元组 + 列清单: {tuples}");
}

#[test]
fn should_commit_immediately_when_batch_commit_off() {
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(
        vec![
            assign(var("v_i"), lit_int(0)),
            PlStatement::Loop(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlLoopStmt {
                    label: None,
                    body: count_loop_body(),
                    end_label: None,
                },
                None,
            )),
        ],
        &ex,
        false,
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let commits = ex.calls().iter().filter(|(s, _)| s == "COMMIT").count();
    let inserts = ex
        .calls()
        .iter()
        .filter(|(s, _)| s.starts_with("INSERT"))
        .count();
    assert_eq!(commits, 3, "开关 OFF（默认）逐行 COMMIT 语义不变");
    assert_eq!(inserts, 3, "开关 OFF 逐行下发");
}

#[test]
fn should_flush_buffer_before_touching_table_when_batch_commit_on() {
    // 读自己写：INSERT t 后 DELETE t（触及缓冲表）→ 必须先刷盘再下发 DELETE
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let del_sql = "DELETE FROM t WHERE a = 'x'";
    let parsed = Parser::new(Tokenizer::new(del_sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let del = PlStatement::SqlStatement {
        span: None,
        sql_text: del_sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::default();
    let stmts = vec![insert_stmt("t", "'x'"), del];
    let (_, out) = run_pl_batch(stmts, &ex, true);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    assert!(calls[0].0.starts_with("INSERT"), "缓冲 INSERT 先刷盘");
    assert!(calls[1].0.starts_with("DELETE"), "DELETE 后于刷盘下发");
    assert_eq!(calls.len(), 2);
}

#[test]
fn should_flush_buffer_before_touching_table_when_insert_has_no_space_before_cols() {
    // 复核应修 3：`INSERT INTO t(a) VALUES …`（表名与列清单之间无空格）的
    // tables() 曾取到 "t(a," → 永不命中 → 读自己写漏刷（golden case_split_agg
    // 的 routine.sql 即此写法）。表名 token 必须按 '(' 截断。
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let ins_sql = "INSERT INTO t(a) VALUES ('x')";
    let ins = PlStatement::SqlStatement {
        span: None,
        sql_text: ins_sql.to_string(),
        statement: Box::new(
            Parser::new(Tokenizer::new(ins_sql).tokenize().unwrap())
                .parse()
                .remove(0),
        ),
    };
    let del_sql = "DELETE FROM t WHERE a = 'x'";
    let del = PlStatement::SqlStatement {
        span: None,
        sql_text: del_sql.to_string(),
        statement: Box::new(
            Parser::new(Tokenizer::new(del_sql).tokenize().unwrap())
                .parse()
                .remove(0),
        ),
    };
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(vec![ins, del], &ex, true);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    assert!(
        calls[0].0.starts_with("INSERT"),
        "无空格形态也必须命中缓冲表并先刷盘，实际下发顺序: {calls:?}"
    );
    assert!(calls[1].0.starts_with("DELETE"), "DELETE 后于刷盘下发");
}

struct FailOnPattern {
    pattern: &'static str,
    ok_execs: std::cell::Cell<u32>,
}
impl SqlExecutor for FailOnPattern {
    fn query(
        &self,
        _sql: &str,
        _binds: &BindMap,
    ) -> Result<crate::exec::RowStream, crate::error::EngineError> {
        Ok(crate::exec::RowStream::new(vec![], vec![]))
    }
    fn execute(
        &self,
        sql: &str,
        _binds: &BindMap,
    ) -> Result<crate::exec::ExecOutcome, EngineError> {
        if sql.contains(self.pattern) {
            return Err(crate::error::EngineError::kernel(
                10,
                "constraint violation".to_string(),
            ));
        }
        self.ok_execs.set(self.ok_execs.get() + 1);
        Ok(crate::exec::ExecOutcome { rows_affected: 1 })
    }
}

#[test]
fn should_replay_batch_and_skip_failing_row_when_batch_commit_on() {
    // 批内含坏行：合并语句失败 → 回退逐行 → 坏行跳过、好行落库、不误伤调用结果
    let ex = std::rc::Rc::new(FailOnPattern {
        pattern: "'bad'",
        ok_execs: std::cell::Cell::new(0),
    });
    let stmts = vec![
        insert_stmt("t", "'good1'"),
        insert_stmt("t", "'bad'"),
        insert_stmt("t", "'good2'"),
    ];
    let rc: std::rc::Rc<dyn SqlExecutor> = ex.clone();
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &*rc,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(ex.ok_execs.get(), 2, "合并失败后逐行重放：两个好行落库");
}

#[test]
fn should_fall_back_to_immediate_exec_for_insert_select_when_batch_commit_on() {
    // 评审阻塞项 2：INSERT … SELECT 是形态覆盖门禁已断言的合法形态，
    // 开关 ON 时不可缓冲 → 回落即时下发（不得 ENG-0033 上抛）
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = "INSERT INTO t2 (a) SELECT a FROM t1";
    let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let ins_sel = PlStatement::SqlStatement {
        span: None,
        sql_text: sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(vec![insert_stmt("t1", "'x'"), ins_sel], &ex, true);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    // 复核建议 5：回落分支的「先刷盘」也要有断言保护——预置的 t1 缓冲行
    // 必须先于 INSERT…SELECT 下发（删掉该分支的 flush 会在此变红）
    assert_eq!(
        calls.len(),
        2,
        "恰好两条下发：t1 缓冲合并 INSERT 先刷盘 + INSERT…SELECT 即时下发，实际: {calls:?}"
    );
    assert!(
        calls[0].0.starts_with("INSERT INTO t1"),
        "缓冲的 t1 行先刷盘，实际: {calls:?}"
    );
    assert!(
        calls[1].0.starts_with("INSERT INTO t2"),
        "回落路径必须真实下发 INSERT…SELECT"
    );
}

#[test]
fn should_fall_back_to_immediate_exec_for_insert_with_fn_sites_when_batch_commit_on() {
    // 复核阻塞项：带 sqlfn 下沉（fn_sites 非空）的 INSERT 不可攒批——
    // 攒批路径跳过 sqlstmts::exec，materialize_fn_sites 不执行，
    // sd_fnN 缓存表要么缺失（KERNEL-0010）要么滞留旧元组（静默写错值）。
    // 准入条件：fn_sites 为空才可攒，非空必须回落即时下发。
    let site = crate::sql::FnSite {
        table: "sd_fn0".into(),
        name: "fnc_x".into(),
        args: vec!["v_i".into()],
        distinct_sql: "SELECT DISTINCT 1 AS sd_v, 1 AS sd_a0".into(),
        binds: vec![],
    };
    let fn_insert = IrStmt::SqlExec {
        sql: "INSERT INTO t (a) VALUES (1)".into(),
        binds: vec![],
        fn_sites: vec![site],
    };
    let ex = FakeExecutor::default();
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    // 复核建议 5：预置同表缓冲行使回落分支的 flush_if_touches 真实执行，
    // 并断言顺序（先刷盘 → 再物化 → 再下发 INSERT）
    let prog = IrProg {
        code: vec![
            IrStmt::SqlExec {
                sql: "INSERT INTO t (a) VALUES ('x')".into(),
                binds: vec![],
                fn_sites: vec![],
            },
            fn_insert,
        ],
    };
    let out = run_with_catalog(
        &prog,
        &mut f,
        &ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    let ins_pos = calls
        .iter()
        .position(|(s, _)| s.starts_with("INSERT INTO t"))
        .expect("带下沉的 INSERT 必须原样即时下发，不得进入缓冲");
    let mat_pos = calls
        .iter()
        .position(|(s, _)| s.contains("CREATE OR REPLACE TEMP TABLE sd_fn0"))
        .expect("fn_sites 必须先物化（temp table 建立）");
    assert!(
        ins_pos < mat_pos,
        "缓冲的 t 行必须先刷盘（calls[0]），再物化 sd_fn0；实际: {calls:?}"
    );
    assert!(
        calls[0].0.starts_with("INSERT INTO t"),
        "首条下发必须是刷盘的合并 INSERT，实际: {calls:?}"
    );
}

/// 事件收集 sink（writebuf 刷盘事件断言用）：SqlExec 记 SQL 文本、
/// BatchSkip 记「跳过行数|首错文本」。
struct VecSink {
    events: std::cell::RefCell<Vec<String>>,
}

impl VecSink {
    fn new() -> Self {
        Self {
            events: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn snapshot(&self) -> Vec<String> {
        self.events.borrow().clone()
    }
}

impl crate::exec::TraceSink for VecSink {
    fn on_event(&mut self, event: crate::exec::TraceEvent) {
        match event {
            crate::exec::TraceEvent::SqlExec { sql, .. } => self.events.borrow_mut().push(sql),
            crate::exec::TraceEvent::BatchSkip { rows, first_error } => self
                .events
                .borrow_mut()
                .push(format!("SKIP {rows}|{first_error}")),
            _ => {}
        }
    }
}

#[test]
fn should_emit_sql_exec_event_for_merged_insert_at_flush_point() {
    // 复核应修 B：被合并的 INSERT 在 OFF 路径每条语句发一次 SqlExec 事件，
    // ON 路径刷盘只调 ex.execute——JSONL 面上这些写入完全消失，排障盲区。
    // 刷盘点必须补发与 OFF 同构的事件。
    let ex = FakeExecutor::default();
    let prog = compile_stmts(&[insert_stmt("t", "'x'"), commit_stmt()]).unwrap();
    let mut f = Frame::new();
    let mut sink = VecSink::new();
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let events = sink.snapshot();
    assert!(
        events.iter().any(|s| s.starts_with("INSERT INTO t")),
        "刷盘点必须补发合并 INSERT 的 SqlExec 事件，实际: {events:?}"
    );
}

#[test]
fn should_emit_batch_skip_event_when_replay_drops_rows() {
    // 复核应修 2：回放期被跳过的坏行原样吞掉——JSONL 面上看不出 ON 模式
    // 少了一批数据。补发 BatchSkip{rows, first_error} 事件使丢行可见，
    // 并能把「业务异常行跳过」与引擎缺陷区分开。
    let ex = FailOnPattern {
        pattern: "'bad'",
        ok_execs: std::cell::Cell::new(0),
    };
    let stmts = vec![
        insert_stmt("t", "'good1'"),
        insert_stmt("t", "'bad'"),
        insert_stmt("t", "'good2'"),
    ];
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let mut sink = VecSink::new();
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let events = sink.snapshot();
    assert!(
        events
            .iter()
            .any(|s| s.starts_with("SKIP 1|") && s.contains("constraint violation")),
        "回放跳行必须发 BatchSkip 事件（行数 + 首错文本），实际: {events:?}"
    );
}
