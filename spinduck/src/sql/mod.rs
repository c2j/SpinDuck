//! SQL 层：改写管线（M3）+ DuckDB 原生执行（M4）。
pub mod patterns;
pub mod render;
pub mod rewrite;

#[cfg(feature = "native")]
pub mod exec_native;

pub use render::expr_to_sql;
pub mod sqlfn;
pub use sqlfn::{plan_sites, FnSite};

pub use rewrite::{
    apply_ast_transforms, oracle_format_to_strftime, rewrite_concat, rewrite_tokens,
    rewrite_tokens_with_rules, strip_update_alias, SqlUnit, VarScope,
};

pub use patterns::apply_text_patterns;

#[cfg(feature = "native")]
pub use exec_native::DuckExecutor;
