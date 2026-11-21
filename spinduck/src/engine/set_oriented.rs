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
        // 视图物化探测前移：`CREATE … AS SELECT * FROM <视图>` 遇坏数据
        // 会让 DuckDB 事务进入 aborted 态，此后任何语句（含解释器续跑）
        // 都失败——必须在任何用户代码（前缀、fn 物化前导）执行前探测；
        // 失败时事务内尚无副作用，ROLLBACK 重开后整段按原 IR 解释执行。
        // 计划期已证前缀与 fn 前导写集同视图底表不相交，探测通过 ⇒
        // 物化时点数据与探测时点一致。
        if plans.iter().any(|p| !p.view_preludes.is_empty()) {
            let mut failed = false;
            for p in &plans {
                if self.materialize_views(p).is_err() {
                    failed = true;
                    break;
                }
            }
            if failed {
                self.ex.execute("ROLLBACK", &crate::exec::BindMap::new())?;
                self.ex
                    .execute("BEGIN TRANSACTION", &crate::exec::BindMap::new())?;
                self.push_report(
                    &routine.name,
                    Status::Skipped {
                        reason: SkipReason::UnprovenQueryTotalness,
                    },
                );
                return self.run_prog(&routine.prog, frame, sink, limits, RunSpan::default());
            }
        }
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
        // 守卫 SQL 引用缓存表的计划（verbatim cur_scan / 写守卫子查询）：
        // 物化前导先于入口守卫，否则守卫查询打到不存在的 sd_fn*/sd_bf*。
        // 回退时解释器会重新物化——例程纯度已证，仅多一次去重调用。
        let mut preluded = false;
        if plan.preludes_first && !plan.fn_preludes.is_empty() {
            // INSERT 位前导的 distinct-over-sd_cur 查询可内嵌 sd_vwN
            // 查找 CTE——fn 前导先于视图物化，须先物化一次；例程调用
            // 可回滚会话临时表，视图在 fn 后由下方步骤再物化（幂等）。
            // 物化失败则 fn 前导不可跑（distinct 引用 sd_vwN），跳过
            // 物化——下方视图步骤同样失败，走既有回退。
            // 前移探测已证物化必成功；此处失败属计划期证明缺陷，
            // 直接上抛——吞错会让事务停在 aborted 态。
            let views_ready = plan.view_preludes.is_empty()
                || !plan.fn_preludes.iter().any(|p| p.needs_sd_cur)
                || {
                    self.materialize_views(plan)?;
                    true
                };
            if views_ready {
                crate::runtime::interp::materialize_fn_preludes(
                    &plan.fn_preludes,
                    frame,
                    self.ex.as_ref(),
                    sink,
                    &self.catalog,
                    &routine.prog.var_types,
                )?;
                preluded = true;
            }
        }
        // 视图物化前导：先于入口守卫（守卫可内嵌 sd_vwN 派生表），且在
        // fn 物化之后——例程调用可回滚会话本地建表，视图表必须最后建。
        // 前移探测 + 计划期写集不相交证明保证此处物化必成功；失败上抛
        // （事务已中止，无法安全回退解释器）。
        if !plan.view_preludes.is_empty() {
            self.materialize_views(plan)?;
        }
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
                // fn_site 物化前导：与解释器 OpenCursor 时点同口径，
                // 失败直接上抛（同点失败语义），绝不部分物化后续跑批写。
                if !preluded && !plan.fn_preludes.is_empty() {
                    crate::runtime::interp::materialize_fn_preludes(
                        &plan.fn_preludes,
                        frame,
                        self.ex.as_ref(),
                        sink,
                        &self.catalog,
                        &routine.prog.var_types,
                    )?;
                }
                let rows = self.exec_region_sql(plan, frame, sink)?;
                // miss 仿真标量出口：NO_DATA_FOUND 处理器赋值回填——
                // 存在 miss 行时取回逐行（均匀性已由入口守卫证明）
                // 处理器赋值结果写回帧；无 miss 行保持批写前值。
                for mo in &plan.miss_outs {
                    let bm = crate::runtime::interp::resolve_binds(&mo.binds, frame)?;
                    let mut stream = self.ex.query(&mo.sql, &bm)?;
                    if let Some(row) = stream.fetch_many(1).into_iter().next() {
                        for (var, val) in mo.targets.iter().zip(row) {
                            frame.set(var, val);
                        }
                    }
                }
                self.commit_region_tail(plan, rows, sink)?;
                Ok(None)
            }
        }
    }

    /// 视图物化前导执行：`CREATE OR REPLACE TEMP TABLE sd_vwN AS
    /// SELECT * FROM <视图>`，按声明序；任一失败上抛——物化失败会
    /// 中止事务，调用方必须已在无用户副作用的时点探测过（前移
    /// 探测）或按计划期证明保证不会失败。
    fn materialize_views(&self, plan: &ConversionPlan) -> Result<(), EngineError> {
        for p in &plan.view_preludes {
            self.ex.execute(&p.sql, &crate::exec::BindMap::new())?;
        }
        Ok(())
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
            Some(Ok(candidate)) => Ok(Ok(candidate.conversion.clone())),
            // 离线 Static 快照没有执行器：verbatim 列探测、视图定义与对象
            // 清单一律不可证，候选只会是保守 Skipped。运行期 Runtime 源
            // 持有同一执行器，证明面与未挂计划的 `--file` 路径完全一致——
            // 以运行期重证为准，证明不了时由同一证明器给出原因。
            Some(Err(_)) | None => crate::ir::set_oriented::plan(routine, self.ex.as_ref(), &ctx),
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
