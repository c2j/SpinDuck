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
        // 设计 §2.1：原 `routine.prog` 只读不变，按已选区域列表分段
        // `RunSpan` 驱动——前缀止于 open_pc、区域按 entry_ok 裁决走集合
        // SQL 或原 PC 续跑、后缀从 close_pc+1 起在原程序上继续，绝对
        // `GOTO` 与 `PushTrap` 的 body_start/body_end 全部保持原 PC。
        let plans = match self.select_plan(routine)? {
            Err(reason) => {
                self.push_report(&routine.name, Status::Skipped { reason });
                return self.run_prog(&routine.prog, frame, sink, limits, RunSpan::default());
            }
            Ok(p) => vec![p],
        };
        let mut cursor_pc = 0usize;
        for p in &plans {
            // 步 1：前缀按原 IR 执行至区域入口（stop_pc 处指令不执行、不计步）；
            // 非 Stopped 按 §2.1 直接返回——区域未执行，不记 Converted
            let out = self.run_prog(
                &routine.prog,
                frame,
                sink,
                limits,
                RunSpan {
                    start_pc: cursor_pc,
                    stop_pc: Some(p.open_pc),
                },
            )?;
            if !matches!(out, RunOutcome::Stopped) {
                return Ok(out);
            }
            // 步 2-5：入口守卫裁决 → 集合 SQL 直发或原区域续跑
            if let Some(out) = self.adjudicate_region(routine, p, frame, sink, limits)? {
                return Ok(out);
            }
            cursor_pc = p.close_pc + 1;
        }
        // 步 6：剩余后缀在原程序上续跑至结束
        self.run_prog(
            &routine.prog,
            frame,
            sink,
            limits,
            RunSpan {
                start_pc: cursor_pc,
                stop_pc: None,
            },
        )
    }

    /// 区域裁决（设计 §2.1 步 2-5）：`entry_ok` 外层 `Err(EngineError)` 经
    /// `?` 直接上抛（交 call_with_sink 回滚），仅内层 `Ok(Err(SkipReason))`
    /// 事务仍干净才退回原区域 `[open_pc .. close_pc + 1)`，绝不重放前缀；
    /// 守卫通过则集合 SQL 当场直发（不经 WriteBuffer），开始执行即失败
    /// 直接上抛严禁回放原循环。返回 `Some(out)` 表示例程已提前终结。
    fn adjudicate_region(
        &mut self,
        routine: &CompiledRoutine,
        plan: &ConversionPlan,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
    ) -> Result<Option<RunOutcome>, EngineError> {
        match entry_ok(self.ex.as_ref(), plan, &self.optimize_ctx(), frame)? {
            Err(reason) => {
                self.push_report(&routine.name, Status::Skipped { reason });
                let out = self.run_prog(
                    &routine.prog,
                    frame,
                    sink,
                    limits,
                    RunSpan {
                        start_pc: plan.open_pc,
                        stop_pc: Some(plan.close_pc + 1),
                    },
                )?;
                Ok((!matches!(out, RunOutcome::Stopped)).then_some(out))
            }
            Ok(()) => {
                self.push_report(
                    &routine.name,
                    Status::Converted {
                        sql: plan.sql.clone(),
                    },
                );
                let rows = self.exec_region_sql(plan, frame, sink)?;
                self.commit_region_tail(plan, rows, sink)?;
                Ok(None)
            }
        }
    }

    /// 尾提交（设计 §2.1 契约 2）：仅 `tail_commit && rows_affected > 0` 时
    /// 发 `IrStmt` variant `TxCommit` trace（绝不发 `TxCommitIfWritten`）；
    /// `batch_commit=false` 当场 `COMMIT + BEGIN TRANSACTION`，`true` 时
    /// 延迟到 `call_with_sink` 出口统一提交。0 行不发 trace、不切快照。
    fn commit_region_tail(
        &self,
        plan: &ConversionPlan,
        rows: usize,
        sink: &mut dyn TraceSink,
    ) -> Result<(), EngineError> {
        if !(plan.tail_commit && rows > 0) {
            return Ok(());
        }
        if sink.is_enabled() {
            sink.on_event(crate::exec::TraceEvent::IrStmt {
                variant: "TxCommit",
            });
        }
        if !self.batch_commit {
            self.ex.execute("COMMIT", &crate::exec::BindMap::new())?;
            self.ex
                .execute("BEGIN TRANSACTION", &crate::exec::BindMap::new())?;
        }
        Ok(())
    }

    /// 集合区域 SQL 的直发执行：按实际顺序发 `IrStmt` variant `SqlExec`
    /// trace 与 `TraceEvent::SqlExec { sql, binds, elapsed_us: 0 }`，返回
    /// 影响行数供 `tail_commit` 判定（设计 §2.1 契约 2）。
    fn exec_region_sql(
        &self,
        plan: &ConversionPlan,
        frame: &Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<usize, EngineError> {
        // 与解释路径同序：先 IrStmt variant 埋点（主循环派发前），
        // 再解析绑定发 TraceEvent::SqlExec，最后下发执行
        if sink.is_enabled() {
            sink.on_event(crate::exec::TraceEvent::IrStmt { variant: "SqlExec" });
        }
        let bound = crate::runtime::interp::resolve_binds(&plan.binds, frame)?;
        if sink.is_enabled() {
            sink.on_event(crate::exec::TraceEvent::SqlExec {
                sql: plan.sql.clone(),
                binds: bound.clone(),
                elapsed_us: 0,
            });
        }
        Ok(self.ex.execute(&plan.sql, &bound)?.rows_affected)
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
