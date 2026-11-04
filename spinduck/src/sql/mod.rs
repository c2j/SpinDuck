//! SQL 层：改写管线（M3）+ DuckDB 原生执行（M4 起在 exec 原生实现中）。
pub mod rewrite;

pub use rewrite::{
    apply_ast_transforms, rewrite_tokens, rewrite_tokens_with_rules, SqlUnit, VarScope,
};
