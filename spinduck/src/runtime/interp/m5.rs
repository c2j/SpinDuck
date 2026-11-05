// 解释器测试模块（自 interp.rs 拆分，行为不变，AGENTS §3）。
use super::*;
use crate::exec::FakeExecutor;
use crate::ir::compile_stmts;
use crate::ir::VarName;

use crate::runtime::{Frame, RunOutcome};

use ogsql_parser::ast::plpgsql::PlStatement;
use ogsql_parser::ast::plpgsql::{PlBlock, PlStatement as PlStmtT};
fn ident(name: &str) -> ogsql_parser::ast::Ident {
    ogsql_parser::ast::Ident {
        value: name.to_string(),
        quote_style: None,
    }
}
fn var(name: &str) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::PlVariable(vec![ident(name)])
}
fn lit_str(s: &str) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::String(s.to_string()))
}
fn lit_int(i: i64) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(i))
}
fn assign(target: ogsql_parser::ast::Expr, e: ogsql_parser::ast::Expr) -> PlStatement {
    PlStatement::Assignment {
        target,
        expression: e,
    }
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
fn exc_block(body: Vec<PlStmtT>, handlers: Vec<(Vec<&str>, Vec<PlStmtT>)>) -> PlStmtT {
    use ogsql_parser::ast::plpgsql::{PlExceptionBlock, PlExceptionHandler};
    use ogsql_parser::ast::Spanned;
    PlStmtT::Block(Spanned::new(
        PlBlock {
            label: None,
            declarations: vec![],
            body,
            exception_block: Some(PlExceptionBlock {
                handlers: handlers
                    .into_iter()
                    .map(|(conds, stmts)| PlExceptionHandler {
                        conditions: conds.iter().map(|c| c.to_string()).collect(),
                        statements: stmts,
                    })
                    .collect(),
            }),
            end_label: None,
        },
        None,
    ))
}

fn select_into(var_name: &str, cond: &str) -> PlStatement {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = format!("SELECT t.flag INTO {var_name} FROM swh_routing WHERE t.k = {cond}");
    let parsed = Parser::new(Tokenizer::new(&sql).tokenize().unwrap())
        .parse()
        .remove(0);
    PlStatement::SqlStatement {
        span: None,
        sql_text: sql,
        statement: Box::new(parsed),
    }
}

#[test]
fn should_catch_no_data_found_with_named_handler() {
    let ex = FakeExecutor::default(); // 0 行 → NO_DATA_FOUND
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    let block = exc_block(
        vec![select_into("v_flag", "p_i_date")],
        vec![(
            vec!["no_data_found"],
            vec![assign(var("v_flag"), lit_str("fallback"))],
        )],
    );
    let after = assign(var("v_after"), lit_str("yes"));
    let (f, out) = run_pl_with(vec![pre, block, after], &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_flag".into())),
        Some(&SpinValue::Str("fallback".into()))
    );
    // 处理器完成后继续块外语句
    assert_eq!(
        f.get(&VarName("v_after".into())),
        Some(&SpinValue::Str("yes".into()))
    );
}

#[test]
fn should_propagate_unmatched_error_to_outer_others() {
    let ex = FakeExecutor::default();
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    // 内层：只声明 too_many_rows；实际是 no_data_found → 不匹配 → 上抛
    let inner = exc_block(
        vec![select_into("v_inner", "p_i_date")],
        vec![(
            vec!["too_many_rows"],
            vec![assign(var("v_inner"), lit_str("x"))],
        )],
    );
    // 外层：WHEN OTHERS 兜底
    let outer = exc_block(
        vec![inner],
        vec![(vec!["others"], vec![assign(var("v_handled"), lit_int(1))])],
    );
    let (f, out) = run_pl_with(vec![pre, outer], &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_handled".into())),
        Some(&SpinValue::Int(1))
    );
}

#[test]
fn should_re_raise_from_handler_to_outer_trap() {
    let ex = FakeExecutor::default();
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    // 内层处理器捕获后再次 RAISE → 向外层传播
    let inner = exc_block(
        vec![select_into("v_inner", "p_i_date")],
        vec![(
            vec!["no_data_found"],
            vec![PlStatement::Raise(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlRaiseStmt {
                    level: None,
                    message: Some("re-raised".into()),
                    params: vec![],
                    options: vec![],
                    condname: None,
                    sqlstate: None,
                },
                None,
            ))],
        )],
    );
    let outer = exc_block(
        vec![inner],
        vec![(
            vec!["others"],
            vec![assign(var("v_outer"), lit_str("outer_caught"))],
        )],
    );
    let (f, out) = run_pl_with(vec![pre, outer], &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_outer".into())),
        Some(&SpinValue::Str("outer_caught".into()))
    );
}

#[test]
fn should_stop_propagation_when_no_handler_anywhere() {
    let ex = FakeExecutor::default();
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    let (_, out) = run_pl_with(vec![pre, select_into("v_flag", "p_i_date")], &ex);
    let err = out.unwrap_err();
    assert_eq!(err.code.to_string(), "BIZ-0100");
}
