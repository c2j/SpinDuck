//! SQL 层：改写管线（M3）+ DuckDB 原生执行（M4）。
pub mod rewrite;

#[cfg(feature = "native")]
pub mod exec_native;

pub use rewrite::{
    apply_ast_transforms, rewrite_tokens, rewrite_tokens_with_rules, SqlUnit, VarScope,
};

#[cfg(feature = "native")]
pub use exec_native::DuckExecutor;
