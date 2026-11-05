//! SQL 改写（M3）：token 级 PL 变量替换为命名占位符 `$v_<名>`。
//! 基底是 SqlStatement 自带的原始 sql_text（Token::StringLiteral 是独立
//! token，字符串内不会被误替换）；方言函数映射自 M3.5 接入本管线。
//!
//! 规模拆分（AGENTS §3，单文件 ≤500 行）：`tokens`（变量替换 + `[functions]`
//! 重命名）、`scan`（调用点扫描）、`split`（顶层切分/引号）、`ast`（解析级变换）。

mod ast;
mod scan;
mod split;
mod tokens;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod transform_tests;

pub use ast::{apply_ast_transforms, oracle_format_to_strftime, strip_update_alias};
pub(crate) use split::split_top_level_commas;
pub use tokens::{rewrite_tokens, rewrite_tokens_with_rules, SqlBindSrc, SqlUnit, VarScope};
