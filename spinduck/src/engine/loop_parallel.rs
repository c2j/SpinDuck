//! `loop-parallel` 驱动（设计 §5/§6）：`set_oriented` 未转换或退回后的
//! 二级降级通道——纯 CFG 前缀并行预计算 + 主线程 `InterpState` 按
//! `row_idx` 顺序 `FETCH` 与多出口续跑。默认 `Off` 时不进入本路径。
//!
//! 主线程独占：`FETCH`/游标位置、trace 顺序、suffix/续跑执行、事务与
//! 写缓冲、trap 分派。Worker 只产出字段粒度写日志 + 步数 + trace +
//! trap 栈，按 `row_idx` 顺序落位；`WorkerOutcome::Error`、预算临界
//! 或运行时回滚一律回退原生串行（语义零变化，宁可少并行不可错语义）。

use super::Engine;
use crate::catalog::{CompiledRoutine, ParamMode, RoutineKind};
use crate::error::EngineError;
use crate::exec::{BindMap, Row, TraceEvent, TraceNoopSink, TraceSink};
use crate::ir::loop_parallel::{
    analyze, LoopParallelMode, LoopParallelReport, ParallelSkipReason, ParallelStatus,
    PurePrefixPlan,
};
use crate::ir::{AssignTarget, IrProg, IrStmt, VarName};
use crate::lookup_cache::PrewarmOutcome;
use crate::runtime::cursor::CursorState;
use crate::runtime::interp::{
    find_trap_handler, run_state_spanned, InterpState, RunControl, RunLimits, RunOutcome, RunSpan,
    TrapFrame,
};
use crate::runtime::parallel::calls::{CallOutcome, CallSnapshot, PendingCall};
use crate::runtime::parallel::pool::{dispatch_chunk, ChunkJob};
use crate::runtime::parallel::worker::{WorkerOutcome, WorkerRowResult};
use crate::runtime::run_with_catalog;
use crate::runtime::Frame;
use crate::value::SpinValue;
use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;

/// 单行 `NeedKeys` 重派硬上限（防御异常循环；正常上界 = 行内
/// 物化调用点数，purchase 实测 ≤3）。
const MAX_NEEDKEY_ROUNDS: usize = 64;

/// 并行驱动入口：`Start` = 例程 pc0 起（SO 未接管/计划期失败，
/// 先跑前缀至 open_pc）；`RegionOpen` = SO 前缀已执行、入口守卫
/// 运行态拒绝，只补 `[open_pc, fetch_pc)` 的 OPEN 段。
#[derive(Clone, Copy)]
enum ParallelEntry {
    Start,
    RegionOpen,
}

/// trace 包装汇：旁路侦测 `TxRollback` / 回滚性 SQL——命中即作废
/// 快照与在途 Worker 结果，剩余行原生串行（设计 §3.2/§5.2）。
struct RollbackWatch<'a> {
    inner: &'a mut dyn TraceSink,
    hit: Cell<bool>,
}

impl TraceSink for RollbackWatch<'_> {
    fn is_enabled(&self) -> bool {
        self.inner.is_enabled()
    }
    fn on_event(&mut self, ev: TraceEvent) {
        match &ev {
            TraceEvent::IrStmt { variant } if *variant == "TxRollback" => {
                self.hit.set(true);
            }
            TraceEvent::SqlExec { sql, .. } if sql.to_ascii_lowercase().contains("rollback") => {
                self.hit.set(true);
            }
            _ => {}
        }
        self.inner.on_event(ev);
    }
}

impl Engine {
    /// 最近一次调用的循环并行调度报告；`Off` 或尚未分析时为空。
    pub fn loop_parallel_reports(&self) -> &[LoopParallelReport] {
        &self.loop_parallel_reports
    }

    /// `run_selected` 的串行回退入口：`Safe` 模式下先尝试静态门禁 +
    /// 预热协议，全部通过才接管游标循环区；任一环节失败按原因记报告
    /// 并返回 `Ok(None)` 交原 `run_prog` 串行执行（语义零变化）。
    pub(super) fn try_loop_parallel(
        &mut self,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
    ) -> Result<Option<RunOutcome>, EngineError> {
        if self.loop_parallel.mode != LoopParallelMode::Safe {
            return Ok(None);
        }
        let prog = &routine.prog;
        let plan = match analyze(prog, &self.catalog, limits.batch_commit) {
            Ok(p) => p,
            Err(reason) => {
                self.push_parallel_report(&routine.name, ParallelStatus::Skipped { reason });
                return Ok(None);
            }
        };
        // 帧所有权移交驱动（与 run_spanned 同一 mem::replace 口径）；
        // 驱动内任何分流执行都在同一 InterpState 上续跑，绝不重放前缀。
        let mut state = InterpState::new(std::mem::replace(frame, Frame::new()));
        let mut watch = RollbackWatch {
            inner: sink,
            hit: Cell::new(false),
        };
        let out = self.drive_parallel_loop(
            routine,
            &mut state,
            &mut watch,
            limits,
            &plan,
            ParallelEntry::Start,
        );
        self.lp_finish(routine, state, watch, limits, out, frame)
    }

    /// SO+LP 派发缺口修复：`entry_ok` 入口守卫运行态拒绝（route/top1/
    /// 表达式守卫等）时，同一区域交由 loop-parallel 接管——SO 前缀
    /// `[0, open_pc)` 已执行完毕，驱动只补 `[open_pc, fetch_pc)` 的
    /// OPEN 段，绝不重放前缀。LP 静态分析必须与 SO 计划指向同一
    /// 区域（open/close pc 一致），否则返回 `Ok(None)` 交原
    /// `run_prog` 区域续跑。
    pub(super) fn try_loop_parallel_region(
        &mut self,
        routine: &CompiledRoutine,
        so_plan: &crate::ir::set_oriented::ConversionPlan,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
    ) -> Result<Option<RunOutcome>, EngineError> {
        if self.loop_parallel.mode != LoopParallelMode::Safe {
            return Ok(None);
        }
        let prog = &routine.prog;
        let plan = match analyze(prog, &self.catalog, limits.batch_commit) {
            Ok(p) => p,
            Err(reason) => {
                self.push_parallel_report(&routine.name, ParallelStatus::Skipped { reason });
                return Ok(None);
            }
        };
        if plan.region.open_pc != so_plan.open_pc || plan.region.close_pc != so_plan.close_pc {
            self.push_parallel_report(
                &routine.name,
                ParallelStatus::Skipped {
                    reason: ParallelSkipReason::UnsupportedControlFlow,
                },
            );
            return Ok(None);
        }
        let mut state = InterpState::new(std::mem::replace(frame, Frame::new()));
        let mut watch = RollbackWatch {
            inner: sink,
            hit: Cell::new(false),
        };
        let out = self.drive_parallel_loop(
            routine,
            &mut state,
            &mut watch,
            limits,
            &plan,
            ParallelEntry::RegionOpen,
        );
        self.lp_finish(routine, state, watch, limits, out, frame)
    }

    /// 驱动返回后的帧回写与串行尾续跑（两个入口共用）：`Ok(None)` =
    /// 已推进到 `fetch_pc` 但循环未并行（游标形状/行数/预热不满足）——
    /// 同一 `InterpState` 上从当前 pc 原生续跑至终结，绝不重放前缀。
    fn lp_finish(
        &self,
        routine: &CompiledRoutine,
        mut state: InterpState,
        mut watch: RollbackWatch<'_>,
        limits: RunLimits,
        out: Result<Option<RunOutcome>, EngineError>,
        frame: &mut Frame,
    ) -> Result<Option<RunOutcome>, EngineError> {
        match out {
            Ok(Some(outcome)) => {
                *frame = state.frame;
                Ok(Some(outcome))
            }
            Ok(None) => {
                let pc = state.pc;
                let tail = run_state_spanned(
                    &mut state,
                    &routine.prog,
                    self.ex.as_ref(),
                    &mut watch,
                    &self.catalog,
                    0,
                    RunControl {
                        limits,
                        span: RunSpan {
                            start_pc: pc,
                            stop_pc: None,
                            exit_pc: None,
                            entry_exec: false,
                        },
                    },
                );
                *frame = state.frame;
                tail.map(Some)
            }
            Err(e) => {
                *frame = state.frame;
                Err(e)
            }
        }
    }

    /// 并行驱动。返回 `Ok(Some(out))` = 例程终结；`Ok(None)` = 已推进到
    /// `fetch_pc` 但不满足并行条件——调用方须在同一 `InterpState` 上
    /// 串行续跑（`lp_finish` 的 `Ok(None)` 分支已处理）。
    fn drive_parallel_loop(
        &mut self,
        routine: &CompiledRoutine,
        state: &mut InterpState,
        watch: &mut RollbackWatch<'_>,
        limits: RunLimits,
        plan: &PurePrefixPlan,
        entry: ParallelEntry,
    ) -> Result<Option<RunOutcome>, EngineError> {
        let prog = &routine.prog;
        // 计划结构不变量（region::build 契约）：cond 紧邻 body、
        // exit = close_pc + 1。debug 断言防计划/驱动 PC 口径漂移。
        debug_assert_eq!(plan.body_start_pc, plan.cond_pc + 1);
        debug_assert_eq!(plan.exit_pc, plan.region.close_pc + 1);
        match entry {
            // 步 1：例程前缀执行至 OPEN（stop 不执行 open_pc 指令）。
            ParallelEntry::Start => {
                state.pc = 0;
                let out = self.span_to(state, prog, watch, limits, plan.region.open_pc)?;
                if !matches!(out, RunOutcome::Stopped) {
                    return Ok(Some(out));
                }
            }
            // SO 回退入口：前缀已由 run_prog 执行，直接从区域入口补 OPEN 段。
            ParallelEntry::RegionOpen => {
                state.pc = plan.region.open_pc;
            }
        }
        // 步 2：OPEN 游标（stop 于 fetch_pc，FETCH 指令不执行）。
        let out = self.span_to(state, prog, watch, limits, plan.region.fetch_pc)?;
        if !matches!(out, RunOutcome::Stopped) {
            return Ok(Some(out));
        }
        // 步 3：游标形状与行数门槛（§2：record FETCH / 可共享物化行 /
        // min_rows）。不满足 → Ok(None)，同状态串行续跑。
        let cursor = plan.region.cursor.clone();
        let Some(material) =
            self.cursor_material(&state.frame, &cursor, prog, plan.region.fetch_pc)
        else {
            self.push_parallel_report(
                &routine.name,
                ParallelStatus::Skipped {
                    reason: ParallelSkipReason::UnsupportedControlFlow,
                },
            );
            return Ok(None);
        };
        // 运行态复核（静态门禁已证，此处防御 IR 与计划口径漂移）：
        // 游标源必须是静态 SQL，预热才有意义。
        if !crate::ir::loop_parallel::prewarm::cursor_is_prewarmable(prog, plan) {
            self.push_parallel_report(
                &routine.name,
                ParallelStatus::Skipped {
                    reason: ParallelSkipReason::UnsupportedControlFlow,
                },
            );
            return Ok(None);
        }
        let remaining = material.total.saturating_sub(material.base_idx);
        if remaining < self.loop_parallel.min_rows.max(1) {
            self.push_parallel_report(
                &routine.name,
                ParallelStatus::Skipped {
                    reason: ParallelSkipReason::InsufficientParallelWork,
                },
            );
            return Ok(None);
        }
        // 步 4：静态预热 + 冻结快照（trace 静默、不计步数；
        // 失败/不支持 → 串行续跑 + PrewarmFailed 报告）。
        let t0 = Instant::now();
        let mut prewarmed = 0usize;
        for q in &plan.queries {
            match self.prewarm_one(prog, q.pc, state) {
                Ok(true) => prewarmed += 1,
                _ => {
                    self.push_parallel_report(
                        &routine.name,
                        ParallelStatus::Skipped {
                            reason: ParallelSkipReason::PrewarmFailed,
                        },
                    );
                    return Ok(None);
                }
            }
        }
        let prewarm_ms = t0.elapsed().as_millis() as u64;
        let snapshot = self.lookup_exec.export_frozen();
        // E2 调用物化快照：Worker 探测 `FuncCall`/`SqlFn`，未命中经
        // `NeedKeys` 回主线程补物化（`materialize_pending`）。
        let calls = CallSnapshot::new();
        // 步 5：按 chunk 派发 Worker；主线程按 row_idx 顺序 FETCH +
        // 续跑 suffix。预算临界/回滚命中 → 剩余行原生串行。
        let threads = self.resolve_threads();
        let chunk = self.loop_parallel.chunk_size.max(1);
        let indices: Vec<usize> = (material.base_idx..material.total).collect();
        let mut stats = ParallelStats {
            threads,
            dispatched: indices.len(),
            rows_fallback: 0,
            rollback_invalidated: false,
            prewarmed,
            prewarm_ms,
            need_keys: 0,
        };
        let mut serial_tail = false;
        'outer: for chunk_indices in indices.chunks(chunk) {
            if serial_tail || watch.hit.get() {
                break;
            }
            let base_frame = state.frame.clone();
            let results = dispatch_chunk(&ChunkJob {
                prog,
                plan,
                base_frame: &base_frame,
                rows: &material.rows,
                cursor_schema: &material.schema,
                record_target: &material.record_target,
                snapshot: &snapshot,
                catalog: &self.catalog,
                calls: &calls,
                var_types: &prog.var_types,
                loose: self.catalog.sql_rules().implicit_assignment_loose,
                traps_seed: &state.traps,
                indices: chunk_indices,
                threads,
            });
            for res in results {
                // E2 NeedKeys：快照未命中 → 主线程补物化 → 单行重派，
                // 直至产出确定性结局（Yield/Error/终态交权）。每轮至少
                // 回填一个新键，重派次数以行内调用点数为上界；设硬上限
                // 防御异常循环，超限该行起全部原生串行。
                let mut res = res;
                let mut need_rounds = 0usize;
                while let Some(request) = need_keys_request(&res) {
                    need_rounds += 1;
                    if need_rounds > MAX_NEEDKEY_ROUNDS {
                        stats.rows_fallback += material.total.saturating_sub(res.row_idx);
                        serial_tail = true;
                        break 'outer;
                    }
                    stats.need_keys += 1;
                    let key = request.key();
                    let outcome = self.materialize_pending(&request);
                    calls.insert(key, outcome);
                    let retry = dispatch_chunk(&ChunkJob {
                        prog,
                        plan,
                        base_frame: &base_frame,
                        rows: &material.rows,
                        cursor_schema: &material.schema,
                        record_target: &material.record_target,
                        snapshot: &snapshot,
                        catalog: &self.catalog,
                        calls: &calls,
                        var_types: &prog.var_types,
                        loose: self.catalog.sql_rules().implicit_assignment_loose,
                        traps_seed: &state.traps,
                        indices: std::slice::from_ref(&res.row_idx),
                        threads: 1,
                    });
                    match retry.into_iter().next() {
                        Some(r) => res = r,
                        None => {
                            stats.rows_fallback += material.total.saturating_sub(res.row_idx);
                            serial_tail = true;
                            break 'outer;
                        }
                    }
                }
                let row_idx = res.row_idx;
                match self.consume_row(state, prog, watch, limits, plan, res)? {
                    RowFlow::Next => {}
                    RowFlow::Done(out) => {
                        stats.rollback_invalidated = watch.hit.get();
                        return self.finish_parallel(routine, out, &stats, &calls);
                    }
                    RowFlow::SerialRest => {
                        stats.rows_fallback += material.total.saturating_sub(row_idx);
                        serial_tail = true;
                        break 'outer;
                    }
                }
                if watch.hit.get() {
                    // 回滚已发生：当前行已由原生 suffix 跑完，
                    // 剩余在途/未派发行全部作废串行。
                    stats.rows_fallback += material.total.saturating_sub(row_idx + 1);
                    stats.rollback_invalidated = true;
                    serial_tail = true;
                    break 'outer;
                }
            }
        }
        if serial_tail {
            // 丢弃剩余 Worker 结果：state.pc == fetch_pc 起原生串行。
            let out = self.span_open(state, prog, watch, limits)?;
            stats.rollback_invalidated = watch.hit.get();
            return self.finish_parallel(routine, out, &stats, &calls);
        }
        // 全部行消费完：最后一次 FETCH 必然 NOTFOUND，原生跑完出口。
        let out = self.span_open(state, prog, watch, limits)?;
        stats.rollback_invalidated = watch.hit.get();
        self.finish_parallel(routine, out, &stats, &calls)
    }

    /// E2 主线程物化：`NeedKeys` 请求的唯一执行点。标量 SQL 与
    /// `eval_sqlfn`/`eval_builtin` 同口径（首行首列，空 → Null）；
    /// 例程按 `eval_funccall` 同口径执行并捕获返回值 + OUT/INOUT
    /// 写回。任何失败 → `CallOutcome::Failed`，Worker 交权原生复现。
    fn materialize_pending(&self, req: &PendingCall) -> CallOutcome {
        match req {
            PendingCall::Scalar { sql, binds } => self.materialize_scalar(sql, binds),
            PendingCall::Routine { name, args } => self.materialize_routine(name, args),
        }
    }

    fn materialize_scalar(&self, sql: &str, binds: &BindMap) -> CallOutcome {
        match self.ex.query(sql, binds) {
            Ok(mut rows) => {
                let v = rows
                    .fetch_many(1)
                    .into_iter()
                    .next()
                    .and_then(|r| r.into_iter().next())
                    .unwrap_or(SpinValue::Null);
                CallOutcome::Scalar(v)
            }
            Err(_) => CallOutcome::Failed,
        }
    }

    /// 例程物化：构建 callee 帧（缺省形参补齐）→ `run_with_catalog`
    /// 静默执行 → 捕获 `Returned` 值与全部 `mode != In` 形参终值。
    /// 与 `eval_funccall` 的写回窗口一致：非 `Returned` 结局 ret=Null
    /// 但 OUT 写回仍然生效。
    fn materialize_routine(&self, name: &str, args: &[SpinValue]) -> CallOutcome {
        let Some(routine) = self.catalog.lookup(name) else {
            return CallOutcome::Failed;
        };
        if routine.kind != RoutineKind::Function {
            return CallOutcome::Failed;
        }
        let mut callee_args = Vec::with_capacity(routine.params.len());
        for (i, p) in routine.params.iter().enumerate() {
            callee_args.push(match args.get(i) {
                Some(v) => v.clone(),
                None => p.default.clone().unwrap_or(SpinValue::Null),
            });
        }
        let mut callee = Frame::new();
        let loose = self.catalog.sql_rules().implicit_assignment_loose;
        if callee
            .install_params(&routine.params, &callee_args, loose)
            .is_err()
        {
            return CallOutcome::Failed;
        }
        let mut sink = TraceNoopSink;
        let out = run_with_catalog(
            &routine.prog,
            &mut callee,
            self.ex.as_ref(),
            &mut sink,
            &self.catalog,
            1,
            RunLimits::default(),
        );
        let Ok(outcome) = out else {
            return CallOutcome::Failed;
        };
        let ret = match outcome {
            RunOutcome::Returned(Some(v)) => v,
            _ => SpinValue::Null,
        };
        let mut outs = vec![None; routine.params.len()];
        for (i, p) in routine.params.iter().enumerate() {
            if p.mode == ParamMode::In {
                continue;
            }
            outs[i] = callee.get(&VarName(p.name.to_ascii_lowercase())).cloned();
        }
        CallOutcome::Routine { ret, outs }
    }

    /// `state.pc` 起无边界续跑至例程终结。
    fn span_open(
        &self,
        state: &mut InterpState,
        prog: &IrProg,
        watch: &mut RollbackWatch<'_>,
        limits: RunLimits,
    ) -> Result<RunOutcome, EngineError> {
        let pc = state.pc;
        run_state_spanned(
            state,
            prog,
            self.ex.as_ref(),
            watch,
            &self.catalog,
            0,
            RunControl {
                limits,
                span: RunSpan {
                    start_pc: pc,
                    stop_pc: None,
                    exit_pc: None,
                    entry_exec: false,
                },
            },
        )
    }

    /// `[state.pc, stop_pc)` 区间续跑（stop 指令不执行）。
    fn span_to(
        &self,
        state: &mut InterpState,
        prog: &IrProg,
        watch: &mut RollbackWatch<'_>,
        limits: RunLimits,
        stop_pc: usize,
    ) -> Result<RunOutcome, EngineError> {
        let pc = state.pc;
        run_state_spanned(
            state,
            prog,
            self.ex.as_ref(),
            watch,
            &self.catalog,
            0,
            RunControl {
                limits,
                span: RunSpan {
                    start_pc: pc,
                    stop_pc: Some(stop_pc),
                    exit_pc: None,
                    entry_exec: false,
                },
            },
        )
    }

    /// 游标物化信息提取：仅 `record` 型单目标 FETCH + Open 态可共享行。
    /// `None` = 形状不满足并行（调用方串行续跑，绝不改变语义）。
    fn cursor_material(
        &self,
        frame: &Frame,
        cursor: &VarName,
        prog: &IrProg,
        fetch_pc: usize,
    ) -> Option<CursorMaterial> {
        let IrStmt::Fetch {
            targets, record, ..
        } = prog.code.get(fetch_pc)?
        else {
            return None;
        };
        if !*record || targets.len() != 1 {
            return None;
        }
        let cs = frame.cursor_state(cursor)?;
        let CursorState::Open { stream, .. } = cs else {
            return None;
        };
        let rows = cs.shared_rows()?;
        let base = cs.fetched();
        let total = rows.len();
        if total <= base {
            return None;
        }
        Some(CursorMaterial {
            rows,
            schema: stream.schema.clone(),
            record_target: targets[0].clone(),
            base_idx: base,
            total,
        })
    }

    /// 单条预热查询：解析**循环不变 range binds** → `try_prewarm_only`。
    /// 行键 binds（`BindSrc::Field` 等）在 FETCH 前无值可解析，且
    /// 预热只需 range binds（build/bind_subset 口径）——probe 期由
    /// Worker 逐行 binds 求值（lookup_cache `key_binds` 设计本意）。
    /// `Ok(true)` = Frozen；绑定失败/非 Frozen 一律视为预热失败。
    fn prewarm_one(
        &self,
        prog: &IrProg,
        pc: usize,
        state: &InterpState,
    ) -> Result<bool, EngineError> {
        let IrStmt::SqlQuery { sql, binds, .. } = &prog.code[pc] else {
            return Ok(false);
        };
        let Some(shape) = crate::lookup_cache::shape::recognize(sql) else {
            return Ok(false);
        };
        let needed: std::collections::BTreeSet<String> = shape.range_binds().into_iter().collect();
        let filtered: Vec<(String, crate::ir::BindSrc)> = binds
            .iter()
            .filter(|(n, _)| needed.contains(n))
            .cloned()
            .collect();
        let bound = crate::runtime::interp::resolve_binds(&filtered, &state.frame)?;
        Ok(matches!(
            self.lookup_exec.try_prewarm_only(sql, &bound),
            PrewarmOutcome::Frozen
        ))
    }

    fn resolve_threads(&self) -> usize {
        if self.loop_parallel.threads > 0 {
            return self.loop_parallel.threads;
        }
        std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(1)
    }

    /// 消费一行 Worker 结果（按 `row_idx` 顺序调用）。前置条件：
    /// `state.pc == fetch_pc`（上一行 suffix 已停在此处，或刚 OPEN 完）。
    fn consume_row(
        &self,
        state: &mut InterpState,
        prog: &IrProg,
        watch: &mut RollbackWatch<'_>,
        limits: RunLimits,
        plan: &PurePrefixPlan,
        res: WorkerRowResult,
    ) -> Result<RowFlow, EngineError> {
        // 步骤预算前置检查（设计 §3.2）：以静态上界 max_prefix_steps
        // 判定"进入预算边界"——接受 Worker 前缀会越界时该行起全部
        // 原生串行（丢弃结果、不计 worker 步数；实际步数仍由解释器
        // 在串行侧精确复核）。
        if state.steps.saturating_add(plan.max_prefix_steps) > limits.budget {
            return Ok(RowFlow::SerialRest);
        }
        if state.steps.saturating_add(res.steps) > limits.budget {
            return Ok(RowFlow::SerialRest);
        }
        // FETCH + %NOTFOUND 条件：行必存在（indices 只覆盖物化行），
        // Stopped 于 body_start_pc。
        let out = self.span_to(state, prog, watch, limits, plan.body_start_pc)?;
        if !matches!(out, RunOutcome::Stopped) {
            return Ok(RowFlow::Done(out));
        }
        match res.outcome {
            // NeedKeys 正常不达此处（驱动层已解析重派）；防御性与
            // Error 同口径原生重跑，语义零变化。
            WorkerOutcome::Error | WorkerOutcome::NeedKeys { .. } => {
                // 整行作废：从 body_start 原生重跑前缀+后缀（trap/
                // __sqlerrm/步数全部由解释器精确复现）。
                state.pc = plan.body_start_pc;
                let out = self.span_to(state, prog, watch, limits, plan.region.fetch_pc)?;
                if matches!(out, RunOutcome::Stopped) {
                    Ok(RowFlow::Next)
                } else {
                    Ok(RowFlow::Done(out))
                }
            }
            WorkerOutcome::QueryError { kind, .. } => {
                let err = kind.into_error();
                // 预检：Worker 前缀后的 trap 栈能否处理该错误。不能处理
                // 时与 `Error` 同口径整行原生重跑——writebuf 丢弃等
                // 未处理语义只能由原生路径精确复现（步骤/写集作废）。
                if !trap_would_handle(&res.traps, &err) {
                    state.pc = plan.body_start_pc;
                    let out = self.span_to(state, prog, watch, limits, plan.region.fetch_pc)?;
                    return if matches!(out, RunOutcome::Stopped) {
                        Ok(RowFlow::Next)
                    } else {
                        Ok(RowFlow::Done(out))
                    };
                }
                // 已处理路径：落位前缀产物（步数/写日志/trace/trap 栈），
                // 再注入与串行 `Err(e)` 分支逐字段一致的 trap 分派——
                // 被探测判定的失败查询不再原生重跑（V8-E9）。
                state.steps += res.steps;
                for (target, value) in &res.writes {
                    state.frame.assign(target, value.clone())?;
                }
                if watch.is_enabled() {
                    for ev in res.traces {
                        watch.on_event(ev);
                    }
                }
                state.traps = res.traps;
                let Some(body_start) = find_trap_handler(&mut state.traps, &err) else {
                    // 预检已证必有处理器；防御性上抛（不应到达）。
                    return Err(err);
                };
                state.frame.set(
                    &VarName("__sqlerrm".to_string()),
                    SpinValue::Str(err.message.clone()),
                );
                state.current_error = Some(err);
                state.pc = body_start;
                let out = self.span_to(state, prog, watch, limits, plan.region.fetch_pc)?;
                if matches!(out, RunOutcome::Stopped) {
                    Ok(RowFlow::Next)
                } else {
                    Ok(RowFlow::Done(out))
                }
            }
            WorkerOutcome::Yield { pc } => {
                // 落位：步数、字段写日志、trace、trap 栈。
                state.steps += res.steps;
                for (target, value) in &res.writes {
                    state.frame.assign(target, value.clone())?;
                }
                if watch.is_enabled() {
                    for ev in res.traces {
                        watch.on_event(ev);
                    }
                }
                state.traps = res.traps;
                state.pc = pc;
                // suffix/续跑至下一次 FETCH（stop 不执行 FETCH）。
                let out = self.span_to(state, prog, watch, limits, plan.region.fetch_pc)?;
                if matches!(out, RunOutcome::Stopped) {
                    Ok(RowFlow::Next)
                } else {
                    Ok(RowFlow::Done(out))
                }
            }
        }
    }

    /// 例程终结/串行尾部统一出口：写 Parallelized 报告。
    fn finish_parallel(
        &mut self,
        routine: &CompiledRoutine,
        out: RunOutcome,
        stats: &ParallelStats,
        calls: &CallSnapshot,
    ) -> Result<Option<RunOutcome>, EngineError> {
        self.push_parallel_report(
            &routine.name,
            ParallelStatus::Parallelized {
                workers: stats.threads,
                rows_dispatched: stats.dispatched,
                rows_fallback_serial: stats.rows_fallback,
                rollback_invalidated: stats.rollback_invalidated,
                prewarmed_tables: stats.prewarmed,
                prewarm_ms: stats.prewarm_ms,
                snapshot_hits: calls.hits(),
                snapshot_misses: calls.misses(),
                peak_snapshot_bytes: 0,
            },
        );
        Ok(Some(out))
    }

    fn push_parallel_report(&mut self, routine: &str, status: ParallelStatus) {
        self.loop_parallel_reports.push(LoopParallelReport {
            routine: routine.to_string(),
            status,
        });
    }
}

/// 游标物化信息（`cursor_material` 返回）：Worker 派发所需的全部
/// 只读输入 + 行数区间。
struct CursorMaterial {
    rows: Arc<Vec<Row>>,
    schema: Vec<String>,
    record_target: AssignTarget,
    /// 首个未消费行的绝对下标（`fetched`）。
    base_idx: usize,
    /// 物化行总数。
    total: usize,
}

/// 并行调度统计（`finish_parallel` 报告载荷）。
struct ParallelStats {
    threads: usize,
    dispatched: usize,
    rows_fallback: usize,
    rollback_invalidated: bool,
    prewarmed: usize,
    prewarm_ms: u64,
    /// E2：`NeedKeys` 主线程补物化次数（审计/调优观测）。
    need_keys: u64,
}

/// `NeedKeys` 待物化请求提取（克隆返回——`res` 随后会被重派结果
/// 整体替换，借用不能跨越赋值）。
fn need_keys_request(res: &WorkerRowResult) -> Option<PendingCall> {
    match &res.outcome {
        WorkerOutcome::NeedKeys { request } => Some(request.clone()),
        _ => None,
    }
}

/// trap 栈能否处理该错误（`find_trap_handler` 的非破坏性预检：
/// 自顶向下任一帧含匹配条件即视为可处理——与分派遍历的
/// 「handled」结论同口径）。仅用于 `QueryError` 注入前的分流判定。
fn trap_would_handle(traps: &[TrapFrame], e: &EngineError) -> bool {
    traps.iter().rev().any(|top| {
        top.entries
            .iter()
            .any(|en| en.conds.iter().any(|c| crate::error::cond_matches(c, e)))
    })
}

/// 单行消费后的驱动流向。
enum RowFlow {
    /// 继续下一行（state.pc == fetch_pc）。
    Next,
    /// 例程终结（outcome 直接上交）。
    Done(RunOutcome),
    /// 预算临界：该行起剩余全部原生串行。
    SerialRest,
}
