//! `engine` 的支撑件：注册护栏的文本/AST 判定、SQL 规范化重放、
//! 以及供外部（测试/未来 M6+ 扩展点）使用的求值上下文与名字构造工具。
//!
//! 从 `engine.rs` 拆出以守单文件 ≤700 行上限（AGENTS §3）；`engine` 根
//! 模块对外 re-export，调用路径不变。

use crate::catalog::RoutineCatalog;
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::ir::{AssignTarget, VarName};
use crate::runtime::Frame;
use ogsql_parser::ast::Statement;
use ogsql_parser::token::tokenizer::Tokenizer;

/// 文本是否声明例程（`CREATE … PROCEDURE|FUNCTION|PACKAGE`）。
/// 粗粒度文本判定：只用于"零登记"护栏，宁可多报也不放过静默丢弃。
pub(crate) fn declares_routine(sql_text: &str) -> bool {
    let up = sql_text.to_ascii_uppercase();
    up.contains("CREATE")
        && ["PROCEDURE", "FUNCTION", "PACKAGE"]
            .iter()
            .any(|k| up.contains(k))
}

/// 解析结果里是否存在包规格（spec 合法地不产出例程名，只有包体才编译例程）。
pub(crate) fn has_package_spec(stmts: &[Statement]) -> bool {
    stmts
        .iter()
        .any(|st| matches!(st, Statement::CreatePackage(_)))
}

/// SQL 文本 → 各语句的规范化文本（目录表重放用，依赖 ogsql formatter）。
pub(crate) fn format_statements(sql_text: &str) -> Result<Vec<String>, EngineError> {
    let tokens = Tokenizer::new(sql_text)
        .tokenize()
        .map_err(|e| EngineError::invariant(63, format!("tokenizer: {e}")))?;
    let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
    Ok(stmts
        .iter()
        .map(|st| ogsql_parser::formatter::SqlFormatter::new().format_statement(st))
        .collect())
}

/// 表达式求值环境（PL 上下文函数直调需要 catalog/ex）。
pub struct EvalCtx<'a> {
    pub frame: &'a Frame,
    pub ex: &'a dyn SqlExecutor,
    pub cat: &'a RoutineCatalog,
    pub depth: usize,
}

/// AssignTarget 的便捷构造（CallRoutine OUT 写回）。
pub fn var_target(name: &str) -> AssignTarget {
    AssignTarget::Var(VarName(name.to_ascii_lowercase()))
}

/// 供 ir 测试与 engine 使用：SQL 文本 → 参数名 → 帧变量集（M6 扩展点落地）。
pub fn param_names_to_vars(params: &[String]) -> Vec<VarName> {
    params
        .iter()
        .map(|p| VarName(p.to_ascii_lowercase()))
        .collect()
}
