// 解释器测试模块（自 interp.rs 拆分，行为不变，AGENTS §3）。
// writebuf/batch_commit 块独立成子模块（§3 规模拆分，第四轮复核应修 ④）。
mod writebuf;
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
