//! flush 既有语义锁定：组内首行失败上抛后，若错误被 PL 处理器捕获，
//! 剩余组必须留在缓冲并在例程出口刷盘——不得随错误丢弃（数据丢失）。

use super::*;
use crate::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use std::cell::RefCell;

/// 对指定 `insert into <表>` 前缀一律失败的执行器，记录全部下发文本。
struct FailOnTable {
    fail_prefix: &'static str,
    calls: RefCell<Vec<String>>,
}

impl SqlExecutor for FailOnTable {
    fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        self.calls.borrow_mut().push(sql.to_string());
        Ok(RowStream::new(Vec::new(), Vec::new()))
    }
    fn execute(&self, sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.calls.borrow_mut().push(sql.to_string());
        if sql.to_ascii_lowercase().starts_with(self.fail_prefix) {
            return Err(EngineError::kernel(10, "table is broken"));
        }
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

fn sql_stmt(text: &str) -> PlStatement {
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    PlStatement::SqlStatement {
        span: None,
        sql_text: text.to_string(),
        statement: Box::new(
            Parser::new(Tokenizer::new(text).tokenize().unwrap())
                .parse()
                .remove(0),
        ),
    }
}

fn exc_block(body: Vec<PlStatement>, handler: Vec<PlStatement>) -> PlStatement {
    use ogsql_parser::ast::plpgsql::{PlBlock, PlExceptionBlock, PlExceptionHandler};
    use ogsql_parser::ast::Spanned;
    PlStatement::Block(Spanned::new(
        PlBlock {
            label: None,
            declarations: vec![],
            body,
            exception_block: Some(PlExceptionBlock {
                handlers: vec![PlExceptionHandler {
                    conditions: vec!["others".to_string()],
                    statements: handler,
                }],
            }),
            end_label: None,
        },
        None,
    ))
}

fn compact_calls(ex: &FailOnTable) -> Vec<String> {
    ex.calls
        .borrow()
        .iter()
        .map(|s| {
            s.split_whitespace()
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .collect()
}

#[test]
fn should_preserve_seven_group_order_across_middle_failure() {
    let ex = FailOnTable {
        fail_prefix: "insert into bad",
        calls: RefCell::new(Vec::new()),
    };
    let block = exc_block(
        vec![
            insert_stmt("a", "'1'"),
            insert_stmt("b", "'2'"),
            insert_stmt("a", "'3'"),
            insert_stmt("bad", "'4'"),
            insert_stmt("a", "'5'"),
            insert_stmt("b", "'6'"),
            insert_stmt("a", "'7'"),
            sql_stmt("DELETE FROM bad"),
        ],
        vec![assign(var("v_caught"), lit_str("yes"))],
    );
    let prog = compile_stmts(&[block]).unwrap();
    let mut frame = Frame::new();
    let result = run_with_catalog(
        &prog,
        &mut frame,
        &ex,
        &mut crate::exec::TraceNoopSink,
        &crate::catalog::RoutineCatalog::new(),
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    )
    .unwrap();
    assert_eq!(result, RunOutcome::Completed);
    assert_eq!(
        frame.get(&VarName("v_caught".into())),
        Some(&SpinValue::Str("yes".into()))
    );
    assert_eq!(
        compact_calls(&ex),
        vec![
            "insertintoa(a)values('1')",
            "insertintob(a)values('2')",
            "insertintoa(a)values('3')",
            "insertintobad(a)values('4')",
            "insertintobad(a)values('4')",
            "rollback",
            "begintransaction",
            "insertintoa(a)values('5')",
            "insertintob(a)values('6')",
            "insertintoa(a)values('7')",
        ]
    );
}

#[test]
fn should_keep_remaining_groups_when_flush_error_is_caught() {
    let ex = FailOnTable {
        fail_prefix: "insert into a",
        calls: RefCell::new(Vec::new()),
    };
    // a、b 两组：DELETE FROM a 触发刷盘 → a 组首行失败上抛 → OTHERS 捕获；
    // b 组从未下发，必须在出口刷盘存活。
    let block = exc_block(
        vec![
            insert_stmt("a", "'1'"),
            insert_stmt("b", "'2'"),
            sql_stmt("DELETE FROM a WHERE x = '0'"),
        ],
        vec![assign(var("v_caught"), lit_str("yes"))],
    );
    let prog = compile_stmts(&[block]).expect("compile");
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
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
    assert_eq!(
        f.get(&VarName("v_caught".into())),
        Some(&SpinValue::Str("yes".into())),
        "刷盘错误应被 OTHERS 处理器捕获"
    );
    let calls = ex.calls.borrow();
    let b_inserts = calls
        .iter()
        .filter(|s| s.to_ascii_lowercase().starts_with("insert into b"))
        .count();
    assert_eq!(
        b_inserts, 1,
        "捕获错误后剩余组必须在出口刷盘且不重复，实际下发: {calls:?}"
    );
}

#[test]
fn should_keep_groups_after_middle_failure_and_not_replay_succeeded_group() {
    // a、b、c 三组：DELETE FROM b 触发刷盘 → a 组成功下发、b 组首行失败上抛
    // → OTHERS 捕获。索引消费须 drain(..=1) 出账 a、b 两组，**c 组留存**；
    // 已成功的 a 组不得在出口重复下发（remove(0)→drain 重构的行为锁定）。
    let ex = FailOnTable {
        fail_prefix: "insert into b",
        calls: RefCell::new(Vec::new()),
    };
    let block = exc_block(
        vec![
            insert_stmt("a", "'1'"),
            insert_stmt("b", "'2'"),
            insert_stmt("c", "'3'"),
            sql_stmt("DELETE FROM b WHERE x = '0'"),
        ],
        vec![assign(var("v_caught"), lit_str("yes"))],
    );
    let prog = compile_stmts(&[block]).expect("compile");
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
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
    assert_eq!(
        f.get(&VarName("v_caught".into())),
        Some(&SpinValue::Str("yes".into())),
        "中间组刷盘错误应被 OTHERS 处理器捕获"
    );
    let calls = ex.calls.borrow();
    let count = |tbl: &str| {
        calls
            .iter()
            .filter(|s| {
                s.to_ascii_lowercase()
                    .starts_with(&format!("insert into {tbl}"))
            })
            .count()
    };
    assert_eq!(count("a"), 1, "已成功组不得重复下发，实际: {calls:?}");
    assert_eq!(
        count("c"),
        1,
        "失败组之后的 c 组必须留存并出口刷盘，实际: {calls:?}"
    );
}
