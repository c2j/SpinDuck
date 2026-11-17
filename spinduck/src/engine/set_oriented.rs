//! 调用事务内的集合化选择：前缀先执行 → 入口守卫 → 批写或原样续跑
//! （设计 §4.5-2 五步运行状态机；关闭时保持原始执行路径）。

use super::Engine;
use crate::catalog::CompiledRoutine;
use crate::error::EngineError;
use crate::exec::TraceSink;
use crate::ir::set_oriented::{
    entry_ok, ConversionPlan, OptimizeCtx, Report, SetOrientedMode, SkipReason, Status,
};
use crate::runtime::{Frame, RunControl, RunLimits, RunOutcome, RunSpan};

impl Engine {
    /// 挂载离线候选；目录、规则或结构证据改变后不再复用。
    pub fn attach_set_oriented_plans(&mut self, plans: crate::ir::set_oriented::CompiledPlans) {
        self.schema_snapshot = Some(plans.snapshot.clone());
        self.set_oriented_plans = Some(plans);
    }

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
        let limits = RunLimits {
            budget: self.step_budget,
            batch_commit: self.batch_commit,
        };
        if self.set_oriented == SetOrientedMode::Off {
            return self.run_prog(&routine.prog, frame, sink, limits, RunSpan::default());
        }
        let p = match self.select_plan(routine)? {
            Err(reason) => {
                self.push_report(&routine.name, Status::Skipped { reason });
                return self.run_prog(&routine.prog, frame, sink, limits, RunSpan::default());
            }
            Ok(p) => p,
        };
        // 步 1：前缀按原 IR 执行至区域入口（stop_pc 处指令不执行、不计步）
        let out = self.run_prog(
            &routine.prog,
            frame,
            sink,
            limits,
            RunSpan {
                start_pc: 0,
                stop_pc: Some(p.open_pc),
            },
        )?;
        if !matches!(out, RunOutcome::Stopped) {
            // 前缀提前终结例程（RETURN 等）：区域不可达，选择仍记 Converted
            self.push_report(&routine.name, Status::Converted { sql: p.sql });
            return Ok(out);
        }
        // 步 2/3：入口不抛错守卫；失败则 pc=open_pc 原样续跑，绝不重放前缀
        match entry_ok(self.ex.as_ref(), &p, &self.optimize_ctx(), frame)? {
            Err(reason) => {
                self.push_report(&routine.name, Status::Skipped { reason });
                self.run_prog(
                    &routine.prog,
                    frame,
                    sink,
                    limits,
                    RunSpan {
                        start_pc: p.open_pc,
                        stop_pc: None,
                    },
                )
            }
            Ok(()) => {
                self.push_report(&routine.name, Status::Converted { sql: p.sql.clone() });
                // 步 4/5：批写与后缀同事务连续执行；失败直接上抛，严禁回放原循环
                let suffix = crate::ir::set_oriented::suffix_prog(routine, &p);
                self.run_prog(&suffix, frame, sink, limits, RunSpan::default())
            }
        }
    }

    fn optimize_ctx(&self) -> OptimizeCtx<'_> {
        OptimizeCtx {
            mode: self.set_oriented,
            batch_commit: self.batch_commit,
            rules: &self.rules,
            snapshot: self.schema_snapshot.as_ref(),
            catalog: &self.catalog,
        }
    }

    fn select_plan(
        &self,
        routine: &CompiledRoutine,
    ) -> Result<Result<ConversionPlan, SkipReason>, EngineError> {
        let ctx = self.optimize_ctx();
        let cached = self
            .set_oriented_plans
            .as_ref()
            .filter(|plans| plans.is_current(&ctx))
            .and_then(|plans| plans.get(&routine.name));
        match cached {
            Some(candidate) => Ok(candidate
                .as_ref()
                .map(|p| p.conversion.clone())
                .map_err(|r| *r)),
            None => crate::ir::set_oriented::plan(routine, self.ex.as_ref(), &ctx),
        }
    }

    fn push_report(&mut self, routine: &str, status: Status) {
        self.set_oriented_reports.push(Report {
            routine: routine.to_string(),
            status,
        });
    }

    fn run_prog(
        &self,
        prog: &crate::ir::IrProg,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        span: RunSpan,
    ) -> Result<RunOutcome, EngineError> {
        crate::runtime::run_spanned(
            prog,
            frame,
            self.ex.as_ref(),
            sink,
            &self.catalog,
            0,
            RunControl { limits, span },
        )
    }
}
