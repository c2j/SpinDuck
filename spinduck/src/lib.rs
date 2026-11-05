//! spinduck-core：GaussDB 存储过程引擎（DuckDB 内核）。
//!
//! 分层纪律（AGENTS.md §1）：解释器/编译器只依赖 [`exec::SqlExecutor`] 与
//! [`exec::TraceSink`] 两个接缝；DuckDB API 只允许出现在 `sql/exec.rs` 的
//! 原生实现中。

pub mod catalog;
pub mod cli;
pub mod engine;
pub mod error;
pub mod exec;
pub mod io;
pub mod ir;
pub mod rules;
pub mod runtime;
pub mod sql;
pub mod types;
pub mod value;

pub use ir::{compile_block, compile_stmts, IrExpr, IrProg, IrStmt};

pub use catalog::{
    CallResult, CompiledRoutine, ParamMode, RoutineCatalog, RoutineKind, RoutineParam,
};
pub use engine::Engine;
pub use error::{EngineError, ErrorCode, FaultDomain};
pub use exec::{BindMap, ExecOutcome, FakeExecutor, Row, RowStream, SqlExecutor, TraceSink};
pub use rules::RuleRegistry;
#[cfg(feature = "native")]
pub use sql::DuckExecutor;
pub use types::SpinType;
pub use value::{coerce, SpinValue};

/// 引擎版本号。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn should_expose_version_from_cargo_metadata() {
        assert!(!crate::version().is_empty());
    }
}
