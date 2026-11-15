//! 在调用事务内选择独立集合化产物，关闭时保持原始执行路径。

use super::Engine;
use crate::catalog::CompiledRoutine;
use crate::error::EngineError;
use crate::exec::TraceSink;
use crate::ir::set_oriented::Report;
use crate::runtime::{Frame, RunOutcome};

impl Engine {
    /// 最近一次调用的集合化选择报告；OFF 或尚未分析时为空。
    pub fn set_oriented_reports(&self) -> &[Report] {
        &self.set_oriented_reports
    }

    pub(super) fn run_selected(
        &mut self,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<RunOutcome, EngineError> {
        let optimized;
        let prog = if self.set_oriented {
            let (prog, report) =
                crate::ir::set_oriented::optimize(routine, self.ex.as_ref(), self.batch_commit)?;
            self.set_oriented_reports.push(report);
            optimized = prog;
            &optimized
        } else {
            &routine.prog
        };
        crate::runtime::run_with_catalog(
            prog,
            frame,
            self.ex.as_ref(),
            sink,
            &self.catalog,
            0,
            crate::runtime::RunLimits {
                budget: self.step_budget,
                batch_commit: self.batch_commit,
            },
        )
    }
}
