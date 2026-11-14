//! IR 语句埋点事件与空 trace 汇（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::exec::TraceEvent;
use crate::exec::TraceSink;
use crate::ir::IrStmt;

pub(super) fn spinduck_trace_event(st: &IrStmt) -> TraceEvent {
    TraceEvent::IrStmt {
        variant: variant_name(st),
    }
}

fn variant_name(st: &IrStmt) -> &'static str {
    match st {
        IrStmt::Nop => "Nop",
        IrStmt::SqlExecDynamic { .. } => "SqlExecDynamic",
        IrStmt::Assign { .. } => "Assign",
        IrStmt::Jump(_) => "Jump",
        IrStmt::JumpIfNot { .. } => "JumpIfNot",
        IrStmt::JumpIf { .. } => "JumpIf",
        IrStmt::Raise { .. } => "Raise",
        IrStmt::Return { .. } => "Return",
        IrStmt::TxCommit => "TxCommit",
        IrStmt::TxRollback => "TxRollback",
        IrStmt::SqlQuery { .. } | IrStmt::FoldedQuery { .. } => "SqlQuery",
        IrStmt::SqlExec { .. } => "SqlExec",
        IrStmt::Perform { .. } => "Perform",
        IrStmt::OpenCursor { .. } => "OpenCursor",
        IrStmt::Fetch { .. } => "Fetch",
        IrStmt::CloseCursor { .. } => "CloseCursor",
        IrStmt::CallRoutine { .. } => "CallRoutine",
        IrStmt::PushTrap { .. } => "PushTrap",
        IrStmt::PopTrap => "PopTrap",
    }
}

pub(crate) struct TraceNoop;
impl TraceSink for TraceNoop {
    #[inline]
    fn is_enabled(&self) -> bool {
        false
    }
    fn on_event(&mut self, _event: TraceEvent) {}
}
