// 解释器测试模块（自 interp.rs 拆分，行为不变，AGENTS §3）。
use super::*;
use crate::exec::FakeExecutor;
use crate::ir::VarName;
use crate::ir::{compile_block, compile_stmts};

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

fn bare_raise() -> PlStatement {
    use ogsql_parser::ast::plpgsql::PlRaiseStmt;
    use ogsql_parser::ast::Spanned;
    PlStatement::Raise(Spanned::new(
        PlRaiseStmt {
            level: None,
            message: None,
            params: vec![],
            options: vec![],
            condname: None,
            sqlstate: None,
        },
        None,
    ))
}

#[test]
fn should_re_raise_current_error_when_handler_ends_with_bare_raise() {
    // 语料主流错误行形态（10 处）：EXCEPTION WHEN OTHERS THEN pack_log.log(…); RAISE;
    // 裸 RAISE 必须重抛**当前**错误——错误码与故障域与未被捕获时完全一致，
    // 不得虚构消息（旧实现把 RAISE; 编成常量 "EXCEPTION"，上层链路判据全失）。
    let ex = FakeExecutor::default(); // 0 行 → NO_DATA_FOUND
    let pre = assign(var("p_i_date"), lit_str("20240105"));
    // 对照组：同一 body 无处理器 → 原始错误
    let (_, base) = run_pl_with(vec![pre.clone(), select_into("v_inner", "p_i_date")], &ex);
    let base_err = base.unwrap_err();

    let inner = exc_block(
        vec![select_into("v_inner", "p_i_date")],
        vec![(
            vec!["others"],
            vec![assign(var("v_logged"), lit_str("logged")), bare_raise()],
        )],
    );
    let (f, out) = run_pl_with(vec![pre, inner], &ex);
    // 处理器体已执行（日志已写），随后原样上抛
    assert_eq!(
        f.get(&VarName("v_logged".into())),
        Some(&SpinValue::Str("logged".into()))
    );
    let e = out.unwrap_err();
    assert_eq!(e.code, base_err.code, "重抛必须保留错误码");
    assert_eq!(e.domain, base_err.domain, "重抛必须保留故障域");
    assert_eq!(e.message, base_err.message, "重抛必须保留原始消息");
}

#[test]
fn should_report_no_active_exception_for_bare_raise_outside_handler() {
    // 处理器外的裸 RAISE 无当前错误可抛：显式报错，不得静默成功
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_with(vec![bare_raise()], &ex);
    let e = out.unwrap_err();
    assert_eq!(e.domain, crate::error::FaultDomain::Business);
    assert!(
        e.message.contains("no active exception"),
        "消息须说明无当前异常，实际: {}",
        e.message
    );
}

#[test]
fn should_reject_raise_named_condition_as_uncovered_form() {
    // 错误码 BIZ-0202 探针：`RAISE <条件名>` 语料零命中，必须显式拒绝
    // （旧实现编造常量消息，把未覆盖面伪装成已支持）
    use ogsql_parser::ast::plpgsql::PlRaiseStmt;
    use ogsql_parser::ast::Spanned;
    let stmt = PlStatement::Raise(Spanned::new(
        PlRaiseStmt {
            level: None,
            message: None,
            params: vec![],
            options: vec![],
            condname: Some("e_bad".into()),
            sqlstate: None,
        },
        None,
    ));
    let e = compile_stmts(&[stmt]).unwrap_err();
    assert_eq!(e.code.to_string(), "BIZ-0202");
    assert!(
        e.message.contains("e_bad"),
        "消息须点出具名条件: {}",
        e.message
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

/// 例程级 EXCEPTION 段（语料主流形态：`IS BEGIN … EXCEPTION WHEN OTHERS … END;`）
/// 与块级同样编译为 trap；此前 PlBlock.exception_block 被静默丢弃。
fn routine_block(body: Vec<PlStmtT>, handlers: Vec<(Vec<&str>, Vec<PlStmtT>)>) -> PlBlock {
    use ogsql_parser::ast::plpgsql::{PlExceptionBlock, PlExceptionHandler};
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
    }
}

#[test]
fn should_run_routine_level_exception_handler() {
    let ex = FakeExecutor::default(); // 0 行 → NO_DATA_FOUND
    let prog = compile_block(&routine_block(
        vec![
            assign(var("p_i_date"), lit_str("20240105")),
            select_into("v_flag", "p_i_date"),
        ],
        vec![(
            vec!["others"],
            vec![assign(var("v_flag"), lit_str("caught"))],
        )],
    ))
    .expect("compile");
    let mut f = Frame::new();
    let out = run(&prog, &mut f, &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_flag".into())),
        Some(&SpinValue::Str("caught".into())),
        "例程级 WHEN OTHERS 必须执行处理器体"
    );
}

#[test]
fn should_run_routine_handler_then_finish_without_body_tail() {
    let ex = FakeExecutor::default();
    let prog = compile_block(&routine_block(
        vec![
            assign(var("p_i_date"), lit_str("20240105")),
            select_into("v_flag", "p_i_date"),
            assign(var("v_tail"), lit_str("executed")),
        ],
        vec![(
            vec!["no_data_found"],
            vec![assign(var("v_caught"), lit_int(1))],
        )],
    ))
    .expect("compile");
    let mut f = Frame::new();
    let out = run(&prog, &mut f, &ex);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(
        f.get(&VarName("v_caught".into())),
        Some(&SpinValue::Int(1)),
        "具名条件同样要匹配例程级处理器"
    );
    assert_eq!(
        f.get(&VarName("v_tail".into())),
        None,
        "处理器接管后不再执行 body 尾部（GaussDB 语义：处理器体后直接结束块）"
    );
}
