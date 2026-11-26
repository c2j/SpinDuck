//! 调用事务内的集合化选择：前缀先执行 → 入口守卫 → 批写或原样续跑
//! （设计 §4.5-2 五步运行状态机；关闭时保持原始执行路径）。

use super::Engine;
use crate::catalog::CompiledRoutine;
use crate::error::EngineError;
use crate::exec::{SqlExecutor, TraceSink};
use crate::ir::set_oriented::{
    entry_ok, Arm, ConversionPlan, Discriminator, OptimizeCtx, Report, SetOrientedMode, SkipReason,
    Status,
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
            // loop-parallel 二级降级通道（默认 Off 时 try_loop_parallel 立即返回）
            if let Some(out) = self.try_loop_parallel(routine, frame, sink, limits)? {
                return Ok(out);
            }
            return self.run_prog(&routine.prog, frame, sink, limits, RunSpan::default());
        }
        // 设计 §2.1：原 `routine.prog` 只读不变，按已选区域列表分段
        // `RunSpan` 驱动——前缀止于 open_pc、区域按 entry_ok 裁决走集合
        // SQL 或原 PC 续跑、后缀从 close_pc+1 起在原程序上继续，绝对
        // `GOTO` 与 `PushTrap` 的 body_start/body_end 全部保持原 PC。
        let plans = match self.select_plan(routine)? {
            Err(reason) => {
                self.push_report(&routine.name, Status::Skipped { reason });
                // 集合化未接管：loop-parallel 作为二级降级通道再试一次
                if let Some(out) = self.try_loop_parallel(routine, frame, sink, limits)? {
                    return Ok(out);
                }
                return self.run_fallback(routine, frame, sink, limits, RunSpan::default());
            }
            Ok(p) => vec![p],
        };
        // 视图物化探测前移：`CREATE … AS SELECT * FROM <视图>` 遇坏数据
        // 会让 DuckDB 事务进入 aborted 态，此后任何语句（含解释器续跑）
        // 都失败——必须在任何用户代码（前缀、fn 物化前导）执行前探测；
        // 失败时事务内尚无副作用，ROLLBACK 重开后整段按原 IR 解释执行。
        // 计划期已证前缀与 fn 前导写集同视图底表不相交，探测通过 ⇒
        // 物化时点数据与探测时点一致。
        // D7 双臂：判别式选中臂的 view_preludes 同样在 pick_arm 内物化
        // ——物化失败会中止事务且彼时用户代码已跑，无法安全回退，
        // 故臂计划一并纳入前移探测（未选中臂的臂视图同样先建，
        // 选中臂在裁决点按同名 CREATE OR REPLACE 重建覆盖，幂等）。
        let view_plans = plans.iter().flat_map(|p| {
            p.discriminator
                .iter()
                .flat_map(|d| d.arms.iter().map(|a| a.plan.as_ref()))
                .chain(std::iter::once(p))
        });
        let view_plans: Vec<&ConversionPlan> = view_plans.collect();
        if view_plans.iter().any(|p| !p.view_preludes.is_empty()) {
            let mut failed = false;
            for p in &view_plans {
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
                return self.run_fallback(routine, frame, sink, limits, RunSpan::default());
            }
        }
        let mut cursor_pc = 0usize;
        for (i, p) in plans.iter().enumerate() {
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
                    exit_pc: None,
                    entry_exec: false,
                },
            )?;
            if !matches!(out, RunOutcome::Stopped) {
                return Ok(out);
            }
            // 步 2-5：入口守卫裁决 → 集合 SQL 直发或原区域续跑
            if let Some(out) =
                self.adjudicate_region(routine, p, frame, sink, limits, i + 1 == plans.len())?
            {
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
                exit_pc: None,
                entry_exec: false,
            },
        )
    }

    /// 区域裁决（设计 §2.1 步 2-5）：`entry_ok` 外层 `Err(EngineError)` 经
    /// `?` 直接上抛（交 call_with_sink 回滚），仅内层 `Ok(Err(SkipReason))`
    /// 事务仍干净才退回原区域 `[open_pc .. close_pc + 1)`，绝不重放前缀；
    /// 守卫通过则集合 SQL 当场直发（不经 WriteBuffer），开始执行即失败
    /// 直接上抛严禁回放原循环。返回 `Some(out)` 表示例程已提前终结。
    /// `is_last` = 本区域是计划列表末项：守卫运行态拒绝时允许
    /// loop-parallel 接管（其串行尾会跑完例程剩余全部 PC，故仅末区
    /// 域安全——前面区域拒绝时后续计划仍须按原序裁决）。
    fn adjudicate_region(
        &mut self,
        routine: &CompiledRoutine,
        plan: &ConversionPlan,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        is_last: bool,
    ) -> Result<Option<RunOutcome>, EngineError> {
        // 守卫 SQL 引用缓存表的计划（verbatim cur_scan / 写守卫子查询）：
        // 物化前导先于入口守卫，否则守卫查询打到不存在的 sd_fn*/sd_bf*。
        // 回退时解释器会重新物化——例程纯度已证，仅多一次去重调用。
        // 备选臂计划的 fn_preludes 与主计划同源（游标/INSERT 位）：
        // D7 判别式下 THEN 臂前导由 pick_arm 在选中时前置物化，
        // 此处仅按主计划口径决定前移。
        let preludes_first = plan.preludes_first;
        let mut preluded = false;
        if preludes_first && !plan.fn_preludes.is_empty() {
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
        // D7 双臂（§12.3(d)）：判别式取值选臂——恰一行整数标签才可证，
        // NULL/不等值经 SQL `=` 三值语义已落 ELSE 臂（PL IF 口径，
        // 与 PR#62 route::pick 一致）；标签越界/非整数/查询异常 →
        // RouteDiscriminatorNotTotal 整段退回（防御位）。未选中臂
        // 的守卫不求值。判别式与主计划前导同序物化（可内嵌 sd_vwN）。
        // PR#62 route_alt（备选计划 + 入口守卫选臂）由判别式取代：
        // 同点消歧语义 + 结构化 Arm 上报（admit 集成）。
        let (arm_plan, arm_label) = match &plan.discriminator {
            None => (plan, None),
            Some(d) => match self.pick_arm(d, plan, routine, frame, sink)? {
                ArmPick::Plan(p, a) => (p, Some(a)),
                ArmPick::NotTotal => {
                    self.push_report(
                        &routine.name,
                        Status::Skipped {
                            reason: SkipReason::RouteDiscriminatorNotTotal,
                        },
                    );
                    // SO+LP 派发缺口：判别式运行态拒绝（数据相关）时，
                    // 末区域交 loop-parallel 接管——前缀已执行，LP 只补
                    // OPEN 段；LP 自身门禁/预热失败退回串行续跑，语义零变化。
                    if is_last {
                        if let Some(out) =
                            self.try_loop_parallel_region(routine, plan, frame, sink, limits)?
                        {
                            return Ok(Some(out));
                        }
                    }
                    let out = self.run_fallback(
                        routine,
                        frame,
                        sink,
                        limits,
                        RunSpan {
                            start_pc: plan.open_pc,
                            stop_pc: Some(plan.close_pc + 1),
                            exit_pc: None,
                            entry_exec: false,
                        },
                    )?;
                    return Ok((!matches!(out, RunOutcome::Stopped)).then_some(out));
                }
            },
        };
        match entry_ok(self.ex.as_ref(), arm_plan, &self.optimize_ctx(), frame)? {
            Err(reason) => {
                self.push_report(&routine.name, Status::Skipped { reason });
                // SO+LP 派发缺口：守卫运行态拒绝（数据相关）时，末区域
                // 交 loop-parallel 接管——前缀已执行，LP 只补 OPEN 段；
                // LP 自身门禁/预热失败会退回同状态串行续跑，语义零变化。
                if is_last {
                    if let Some(out) =
                        self.try_loop_parallel_region(routine, plan, frame, sink, limits)?
                    {
                        return Ok(Some(out));
                    }
                }
                let out = self.run_fallback(
                    routine,
                    frame,
                    sink,
                    limits,
                    RunSpan {
                        start_pc: plan.open_pc,
                        stop_pc: Some(plan.close_pc + 1),
                        exit_pc: None,
                        entry_exec: false,
                    },
                )?;
                Ok((!matches!(out, RunOutcome::Stopped)).then_some(out))
            }
            Ok(()) => {
                // 混合计划延迟上报（PR#62 P0-3）：异常行集/基数前置证明
                // 在 `run_hybrid_region` 内完成，失败回退串行时报 Skipped
                // 而非 Converted。混合×判别式臂为罕见组合：延迟路径的
                // 上报不带臂标签（行为不受影响，报告面如实记录）。
                let hybrid_plan = arm_plan.hybrid.is_some();
                if !hybrid_plan {
                    self.push_report(
                        &routine.name,
                        Status::Converted {
                            sql: arm_plan.sql.clone(),
                            arm: arm_label,
                        },
                    );
                }
                // fn_site 物化前导：与解释器 OpenCursor 时点同口径，
                // 失败直接上抛（同点失败语义），绝不部分物化后续跑批写。
                // pick_arm 已物化 Then 臂前导；ELSE 臂=主计划走既有口径。
                if !preluded && arm_label != Some(Arm::Then) && !arm_plan.fn_preludes.is_empty() {
                    crate::runtime::interp::materialize_fn_preludes(
                        &arm_plan.fn_preludes,
                        frame,
                        self.ex.as_ref(),
                        sink,
                        &self.catalog,
                        &routine.prog.var_types,
                    )?;
                }
                // 混合调度（so-exception-row-hybrid §3 + P0-3b）：先把
                // `sd_cur` 物化为会话临时表并替换计划 SQL 占位符——
                // `__sd_rn` 只在物化时赋一次，异常守卫/段批写共用同一
                // 编号（消除各 SQL 各自 ROW_NUMBER 扫描的行序漂移）。
                // 空异常集落入下方既有 SO 路径（物化表版 plan.sql）。
                let mut hcur: Option<(String, ConversionPlan)> = None;
                if hybrid_plan {
                    match self.materialize_hcur(arm_plan, frame)? {
                        Some((name, hp)) => {
                            // exc_rows 三态：Err=查询错误（上抛，先回收物
                            // 化表）；None=守卫行号畸形（串行兜底）；
                            // Some(空集)=无异常行走普通 SO 批写。
                            let exc = match crate::ir::set_oriented::exc_rows(
                                self.ex.as_ref(),
                                &hp.exc_guards,
                                frame,
                            ) {
                                Ok(e) => e,
                                Err(e) => {
                                    self.drop_hcur_table(&name);
                                    return Err(e);
                                }
                            };
                            match exc {
                                Some(exc) if !exc.is_empty() => {
                                    return self.run_hybrid_region(
                                        routine,
                                        &hp,
                                        frame,
                                        sink,
                                        limits,
                                        HybridRun {
                                            exc,
                                            hcur_name: &name,
                                            report_sql: &arm_plan.sql,
                                        },
                                    );
                                }
                                Some(_) => {
                                    // 空异常集：全行批写（物化表版 plan.sql）。
                                    self.push_report(
                                        &routine.name,
                                        Status::Converted {
                                            sql: hp.sql.clone(),
                                            arm: arm_label,
                                        },
                                    );
                                    hcur = Some((name, hp));
                                }
                                None => {
                                    // 畸形守卫行号：证据不可信 → 整区域串行
                                    // 兜底（此点尚未产生任何写入，语义零变化）。
                                    self.drop_hcur_table(&name);
                                    return self.hybrid_serial_from_open(
                                        routine,
                                        arm_plan,
                                        frame,
                                        sink,
                                        limits,
                                        SkipReason::HybridPreflightFailed,
                                    );
                                }
                            }
                        }
                        None => {
                            // 入口物化/重绑失败：串行兜底（分因上报，与
                            // 守卫证据类 HybridPreflightFailed 区分）。
                            return self.hybrid_serial_from_open(
                                routine,
                                arm_plan,
                                frame,
                                sink,
                                limits,
                                SkipReason::HybridHcurMaterializeFailed,
                            );
                        }
                    }
                }
                let exec_plan = hcur.as_ref().map_or(arm_plan, |(_, hp)| hp);
                let rows = self.exec_region_sql(exec_plan, frame, sink)?;
                // miss 仿真标量出口：NO_DATA_FOUND 处理器赋值回填——
                // 存在 miss 行时取回逐行（均匀性已由入口守卫证明）
                // 处理器赋值结果写回帧；无 miss 行保持批写前值。
                for mo in &exec_plan.miss_outs {
                    let bm = crate::runtime::interp::resolve_binds(&mo.binds, frame)?;
                    let mut stream = self.ex.query(&mo.sql, &bm)?;
                    if let Some(row) = stream.fetch_many(1).into_iter().next() {
                        for (var, val) in mo.targets.iter().zip(row) {
                            frame.set(var, val);
                        }
                    }
                }
                self.commit_region_tail(exec_plan, rows, sink)?;
                if let Some((name, _)) = &hcur {
                    self.drop_hcur_table(name);
                }
                Ok(None)
            }
        }
    }

    /// D7 判别式选臂（§12.3(d).2）：`d.sql` 在 SQL 侧产整型标签——
    /// `0..arms.len()-1` → 对应臂计划（先补物化其前导），
    /// `arms.len()` → 主计划（ELSE 臂 =值域补集；判别值 NULL 已在
    /// SQL 侧落 ELSE，与 PL IF 语义一致）。越界/非整数/查询不产
    /// 恰一行 → `NotTotal` 整段回退（防御位）。臂等值比较已在
    /// 判别式 SQL 内完成（`=` 语义与原 IF 条件同源），此处不再做
    /// SpinValue 跨类型比对。
    fn pick_arm<'p>(
        &self,
        d: &'p Discriminator,
        plan: &'p ConversionPlan,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<ArmPick<'p>, EngineError> {
        let mut stream = self.ex.query(&d.sql, &crate::exec::BindMap::new())?;
        let tag = match stream.fetch_many(1).first().and_then(|r| r.first()) {
            Some(crate::value::SpinValue::Int(t)) => *t,
            _ => return Ok(ArmPick::NotTotal),
        };
        if tag == d.arms.len() as i64 {
            return Ok(ArmPick::Plan(plan, Arm::Else));
        }
        let Some(arm) = usize::try_from(tag).ok().and_then(|i| d.arms.get(i)) else {
            return Ok(ArmPick::NotTotal);
        };
        let p = arm.plan.as_ref();
        // 臂计划前导：视图物化恒先于守卫；fn 前导同主计划
        // preludes_first 口径——臂计划一旦被选中其守卫/批写
        // 可能引用 sd_fn*/sd_bf*，统一前置物化（纯度已证，
        // 幂等去重）。
        if !p.view_preludes.is_empty() || !p.fn_preludes.is_empty() {
            self.materialize_views(p)?;
            if !p.fn_preludes.is_empty() {
                crate::runtime::interp::materialize_fn_preludes(
                    &p.fn_preludes,
                    frame,
                    self.ex.as_ref(),
                    sink,
                    &self.catalog,
                    &routine.prog.var_types,
                )?;
            }
        }
        Ok(ArmPick::Plan(p, Arm::Then))
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

    /// 段批写提交边界（V55.4）：串行中每行写由体内 `TxCommit(IfWritten)`
    /// 逐行提交，后续行异常回滚只撤当行；段写若滞留未提交，重放行的
    /// 异常 `ROLLBACK` 会连带撤销整段批写。`tail_commit=true`（体内含
    /// 提交）时段写在下一个重放前提交到等价边界；`false` 时不提交——
    /// 串行同样保留挂起写由回滚撤销，语义一致。`batch_commit` 下混合
    /// 计划已在计划期整体关闭（写缓冲口径不可与交错回滚混用），此处
    /// 的 `!self.batch_commit` 仅为与 `commit_region_tail` 同口径的
    /// 防御。
    fn commit_hybrid_segment(
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

    /// 混合调度的物化表存在性探测：一次交叉空集查询覆盖全部
    /// `sd_fn*`/`sd_bf*`/`sd_vw*` 物化表——任一缺失整查询报错，
    /// 由调用方触发刷新。目录错误不毒化事务，可安全续跑。
    fn so_materializations_intact(&self, plan: &ConversionPlan) -> bool {
        let names: Vec<&str> = plan
            .fn_preludes
            .iter()
            .map(|p| p.table.as_str())
            .chain(plan.view_preludes.iter().filter_map(|v| {
                // `CREATE OR REPLACE TEMP TABLE <name> AS ...` → 取 TABLE 后词。
                let mut it = v.sql.split_whitespace();
                it.by_ref()
                    .find(|w| w.eq_ignore_ascii_case("TABLE"))
                    .and_then(|_| it.next())
            }))
            .collect();
        if names.is_empty() {
            return true;
        }
        let probe = format!(
            "SELECT COUNT(*) FROM {}",
            names
                .iter()
                .map(|n| format!("(SELECT 1 FROM \"{n}\" LIMIT 0) _t"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.ex.query(&probe, &crate::exec::BindMap::new()).is_ok()
    }

    /// 段前守卫：物化表被重放例程调用撤销时才重建（探测代价 =
    /// 一次空集查询，远低于全量重物化）。
    fn ensure_so_materializations(
        &self,
        plan: &ConversionPlan,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<(), EngineError> {
        if !self.so_materializations_intact(plan) {
            self.refresh_so_materializations(plan, routine, frame, sink)?;
        }
        Ok(())
    }

    /// 混合调度的物化刷新：单行重放会原生执行体内例程调用，会话
    /// 回滚可撤销 `sd_fn*`/`sd_bf*`/`sd_vw*` 临时表（解释器路径由
    /// dynamic.rs 逐站点重写兜底，混合路径的批写段没有该机制）。
    /// 次序镜像入口期 views→fn→views；重写幂等（distinct 实参不
    /// 依赖帧内行级变量），失败上抛——事务已中止无法安全续跑。
    fn refresh_so_materializations(
        &self,
        plan: &ConversionPlan,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<(), EngineError> {
        if plan.view_preludes.is_empty() && plan.fn_preludes.is_empty() {
            return Ok(());
        }
        self.materialize_views(plan)?;
        crate::runtime::interp::materialize_fn_preludes(
            &plan.fn_preludes,
            frame,
            self.ex.as_ref(),
            sink,
            &self.catalog,
            &routine.prog.var_types,
        )?;
        self.materialize_views(plan)?;
        Ok(())
    }

    /// 集合区域 SQL 的直发执行：按实际顺序发 `IrStmt` variant `SqlExec`
    /// trace 与实测 `elapsed_us` 的 `TraceEvent::SqlExec`，返回
    /// 影响行数供 `tail_commit` 判定（设计 §2.1 契约 2）。
    fn exec_region_sql(
        &self,
        plan: &ConversionPlan,
        frame: &Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<usize, EngineError> {
        self.exec_sql(&plan.sql, &plan.binds, frame, sink)
    }

    /// 集合区域 SQL 的直发执行（混合分段复用同一 trace/绑定口径）：
    /// 先 `IrStmt` variant `SqlExec` 埋点，再 `TraceEvent::SqlExec`，
    /// 最后下发执行并返回影响行数。
    fn exec_sql(
        &self,
        sql: &str,
        binds: &[(String, crate::ir::BindSrc)],
        frame: &Frame,
        sink: &mut dyn TraceSink,
    ) -> Result<usize, EngineError> {
        if sink.is_enabled() {
            sink.on_event(crate::exec::TraceEvent::IrStmt { variant: "SqlExec" });
        }
        let bound = crate::runtime::interp::resolve_binds(binds, frame)?;
        let t = std::time::Instant::now();
        let out = self.ex.execute(sql, &bound);
        if sink.is_enabled() {
            sink.on_event(crate::exec::TraceEvent::SqlExec {
                sql: sql.to_string(),
                binds: bound.clone(),
                elapsed_us: t.elapsed().as_micros() as u64,
            });
        }
        Ok(out?.rows_affected)
    }

    /// 混合段批写：`plan.sql` 段谓词占位符替换为 [lo,hi) 字面量
    /// （__SD_HI__ 排他）后按普通 SO 同一 trace/绑定口径直发。
    fn exec_hybrid_segment(
        &self,
        plan: &ConversionPlan,
        frame: &Frame,
        sink: &mut dyn TraceSink,
        lo: u64,
        hi: u64,
    ) -> Result<usize, EngineError> {
        let tmpl = plan
            .seg_sql
            .as_deref()
            .ok_or_else(|| EngineError::invariant(32, "hybrid plan missing seg_sql"))?;
        let sql = tmpl
            .replace("__SD_LO__", &lo.to_string())
            .replace("__SD_HI__", &hi.to_string());
        debug_assert!(!sql.contains("__SD_"));
        self.exec_sql(&sql, &plan.binds, frame, sink)
    }

    /// 混合调度包装：无论成败都回收 `sd_cur` 物化临时表（唯一表名，
    /// 不回收会在长会话内累积）。
    fn run_hybrid_region(
        &mut self,
        routine: &CompiledRoutine,
        plan: &ConversionPlan,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        run: HybridRun<'_>,
    ) -> Result<Option<RunOutcome>, EngineError> {
        let hcur_name = run.hcur_name;
        let r = self.run_hybrid_body(routine, plan, frame, sink, limits, run);
        self.drop_hcur_table(hcur_name);
        r
    }

    /// SO 行级混合执行（so-exception-row-hybrid §3）：正常行按行段
    /// 批写（与异常行重放保序交错），异常行 `seek_before`+真实 Fetch
    /// 单行重放；早退/返回原样上交；末尾补终止空 FETCH 对齐游标属性，
    /// `exit_pc==close_pc` 时补执行 CloseCursor 后交还外层续跑。
    fn run_hybrid_body(
        &mut self,
        routine: &CompiledRoutine,
        plan: &ConversionPlan,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        run: HybridRun<'_>,
    ) -> Result<Option<RunOutcome>, EngineError> {
        let (exc, hcur_name, report_sql) = (run.exc, run.hcur_name, run.report_sql);
        let spec = plan
            .hybrid
            .as_ref()
            .ok_or_else(|| EngineError::invariant(32, "hybrid plan missing spec"))?;
        // OPEN：真实游标就位（解释器 OpenCursor 指令，open_pc..fetch_pc）。
        let out = self.run_prog(
            &routine.prog,
            frame,
            sink,
            limits,
            RunSpan {
                start_pc: plan.open_pc,
                stop_pc: Some(spec.fetch_pc),
                exit_pc: None,
                entry_exec: false,
            },
        )?;
        if !matches!(out, RunOutcome::Stopped) {
            return Ok(Some(out));
        }
        // P0-3b：游标快照重绑定到 `sd_cur` 物化表序（`ORDER BY
        // __sd_rn` 取回、剥离 rn 列）——FETCH/seek_before/异常守卫/
        // 段批写共用同一行序，`__sd_rn` 与 FETCH 位置的绑定由构造
        // 保证，消除"游标查询与守卫各自 ROW_NUMBER 扫描"的双执行
        // 行序漂移（无 ORDER BY 游标源两次执行本就可能给出不同序）。
        // 重绑失败（查询失败/行宽失配）整区域串行兜底（零写入）。
        if !self.rebind_hcur_cursor(hcur_name, frame, spec)? {
            return self.hybrid_serial_fallback(
                routine,
                frame,
                sink,
                limits,
                spec,
                SkipReason::HybridHcurMaterializeFailed,
            );
        }
        let (total, shared) = match frame.cursor_state(&spec.cursor) {
            Some(crate::runtime::CursorState::Open { total, .. }) => (
                *total,
                frame
                    .cursor_state(&spec.cursor)
                    .and_then(|c| c.shared_rows()),
            ),
            _ => {
                return Err(EngineError::invariant(
                    32,
                    "hybrid cursor not open after stream rebind",
                ))
            }
        };
        // P0-3a 前置证明（任何段写之前完成，失败零写入回退串行）：
        // 异常行号必须全部落在游标总数内——越界说明守卫行号与
        // FETCH 序已失配，继续执行只会错写。
        if exc.iter().any(|e| *e >= total as u64) {
            return self.hybrid_serial_fallback(
                routine,
                frame,
                sink,
                limits,
                spec,
                SkipReason::HybridPreflightFailed,
            );
        }
        self.push_report(
            &routine.name,
            Status::Converted {
                sql: report_sql.to_string(),
                arm: None,
            },
        );
        let mut written = 0usize;
        let mut seg_lo = 0u64;
        let mut exited = false;
        for &e in &exc {
            // 前置证明已证 e < total；此处仅作计划/驱动口径漂移的
            // debug 断言（release 下不可达，不产生写后 invariant）。
            debug_assert!(e < total as u64);
            if e > seg_lo {
                self.ensure_so_materializations(plan, routine, frame, sink)?;
                self.ensure_hcur(hcur_name, plan, frame, spec, shared_rows_slice(&shared))?;
                let n = self.exec_hybrid_segment(plan, frame, sink, seg_lo, e)?;
                written += n;
                self.commit_hybrid_segment(plan, n, sink)?;
            }
            let rspec = crate::runtime::interp::ReplaySpec {
                cursor: &spec.cursor,
                row_index: e as usize,
                fetch_pc: spec.fetch_pc,
                exit_pc: spec.exit_pc,
                limits,
            };
            match crate::runtime::interp::replay_cursor_row(
                &routine.prog,
                frame,
                &rspec,
                self.ex.as_ref(),
                sink,
                &self.catalog,
                0,
            )? {
                crate::runtime::interp::RowReplay::IterDone => {}
                crate::runtime::interp::RowReplay::LoopExited => {
                    exited = true;
                    break;
                }
                crate::runtime::interp::RowReplay::Returned(v) => {
                    return Ok(Some(RunOutcome::Returned(v)))
                }
                crate::runtime::interp::RowReplay::Completed => {
                    return Ok(Some(RunOutcome::Completed))
                }
            }
            seg_lo = e + 1;
        }
        if !exited {
            if seg_lo < total as u64 {
                self.ensure_so_materializations(plan, routine, frame, sink)?;
                self.ensure_hcur(hcur_name, plan, frame, spec, shared_rows_slice(&shared))?;
                let n = self.exec_hybrid_segment(plan, frame, sink, seg_lo, total as u64)?;
                written += n;
                self.commit_hybrid_segment(plan, n, sink)?;
            }
            // 终止空 FETCH：游标属性（%NOTFOUND/%ROWCOUNT）对齐串行——
            // seek_before(total)+真实 Fetch 必然走 NOTFOUND 出口边。
            let rspec = crate::runtime::interp::ReplaySpec {
                cursor: &spec.cursor,
                row_index: total,
                fetch_pc: spec.fetch_pc,
                exit_pc: spec.exit_pc,
                limits,
            };
            match crate::runtime::interp::replay_cursor_row(
                &routine.prog,
                frame,
                &rspec,
                self.ex.as_ref(),
                sink,
                &self.catalog,
                0,
            )? {
                crate::runtime::interp::RowReplay::LoopExited
                | crate::runtime::interp::RowReplay::IterDone => {}
                crate::runtime::interp::RowReplay::Returned(v) => {
                    return Ok(Some(RunOutcome::Returned(v)))
                }
                crate::runtime::interp::RowReplay::Completed => {
                    return Ok(Some(RunOutcome::Completed))
                }
            }
        }
        // miss 仿真标量出口 + 尾提交：与普通 SO 同序同口径。
        // 末段重放可能再次撤销物化表——出口查询前探测补齐。
        self.ensure_so_materializations(plan, routine, frame, sink)?;
        for mo in &plan.miss_outs {
            let bm = crate::runtime::interp::resolve_binds(&mo.binds, frame)?;
            let mut stream = self.ex.query(&mo.sql, &bm)?;
            if let Some(row) = stream.fetch_many(1).into_iter().next() {
                for (var, val) in mo.targets.iter().zip(row) {
                    frame.set(var, val);
                }
            }
        }
        self.commit_region_tail(plan, written, sink)?;
        // CloseCursor：exit_pc==close_pc 时补执行 CLOSE 指令；
        // exit_pc==close_pc+1 时串行 EXIT 本就跳过 CLOSE，游标保持打开
        // （与串行状态一致），直接交还外层续跑。
        if spec.exit_pc == plan.close_pc {
            let out = self.run_prog(
                &routine.prog,
                frame,
                sink,
                limits,
                RunSpan {
                    start_pc: spec.exit_pc,
                    stop_pc: Some(spec.exit_pc + 1),
                    exit_pc: None,
                    entry_exec: false,
                },
            )?;
            if !matches!(out, RunOutcome::Stopped) {
                return Ok(Some(out));
            }
        }
        Ok(None)
    }

    /// 混合 `sd_cur` 物化（P0-3b）：`CREATE OR REPLACE TEMP TABLE
    /// __sd_hcur_<seq> AS <hcur_sql>`——`__sd_rn` 只在此赋一次，
    /// 异常守卫/段批写共用同一编号。返回 `(表名, 占位符已替换的
    /// 计划副本)`；物化失败返回 `None`（调用方整区域串行兜底，
    /// 真实错误由串行路径原样上抛）。
    fn materialize_hcur(
        &self,
        plan: &ConversionPlan,
        frame: &Frame,
    ) -> Result<Option<(String, ConversionPlan)>, EngineError> {
        let Some(spec) = plan.hybrid.as_ref() else {
            return Ok(None);
        };
        if spec.hcur_sql.is_empty() {
            return Ok(None);
        }
        let name = format!("__sd_hcur_{}", next_hcur_seq());
        let bm = crate::runtime::interp::resolve_binds(&plan.binds, frame)?;
        let ddl = format!("CREATE OR REPLACE TEMP TABLE {name} AS {}", spec.hcur_sql);
        if self.ex.execute(&ddl, &bm).is_err() {
            // DDL 失败会把事务打入 aborted 态（KERNEL-0010），直接
            // 串行兜底会全线失败。混合计划仅在前缀写集已证为空时生成
            // （计划期 `hybrid_prefix_clean` 门禁），此处 ROLLBACK 重开
            // 不丢用户写——与视图物化探测失败同口径。
            self.ex.execute("ROLLBACK", &crate::exec::BindMap::new())?;
            self.ex
                .execute("BEGIN TRANSACTION", &crate::exec::BindMap::new())?;
            return Ok(None);
        }
        Ok(Some((name.clone(), substitute_hcur(plan, &name))))
    }

    /// 游标快照重绑定到 `sd_cur` 物化表序（P0-3b）：`ORDER BY
    /// __sd_rn` 取回全行、剥离末列 `__sd_rn`，以原 schema 重建
    /// `RowStream` 换入游标——此后 FETCH/`seek_before`/`shared_rows`
    /// 与异常守卫、段批写共用同一行序。行宽与游标 schema 不符
    /// （物化 SELECT 与游标投影漂移）返回 `false` 由调用方串行兜底。
    fn rebind_hcur_cursor(
        &self,
        name: &str,
        frame: &mut Frame,
        spec: &crate::ir::set_oriented::HybridSpec,
    ) -> Result<bool, EngineError> {
        let mut stream = match self.ex.query(
            &format!("SELECT * FROM {name} ORDER BY __sd_rn"),
            &crate::exec::BindMap::new(),
        ) {
            Ok(s) => s,
            Err(_) => {
                // 查询失败同样中止事务：复位后串行兜底（前缀写集已证
                // 为空，回滚零用户写损失）。
                self.ex.execute("ROLLBACK", &crate::exec::BindMap::new())?;
                self.ex
                    .execute("BEGIN TRANSACTION", &crate::exec::BindMap::new())?;
                return Ok(false);
            }
        };
        let mut rows = stream.fetch_many(usize::MAX);
        let schema = match frame.cursor_state(&spec.cursor) {
            Some(crate::runtime::CursorState::Open { stream: s, .. }) => s.schema.clone(),
            _ => return Ok(false),
        };
        if rows.iter().any(|r| r.len() != schema.len() + 1) {
            return Ok(false);
        }
        for r in &mut rows {
            r.pop();
        }
        let rebound = crate::exec::RowStream::new(schema, rows);
        match frame.cursor_mut(&spec.cursor) {
            Ok(c) => {
                c.swap_stream(rebound);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    /// `sd_cur` 物化表与游标权威行集的位置校验（P0-3b/P0-3a 合一）：
    /// `ORDER BY __sd_rn` 取回的行必须与 `shared_rows()`（FETCH 序）
    /// 逐位相等，且末列 `__sd_rn` 恰为行下标——一次证明覆盖基数、
    /// 行序与 rn↔位置绑定。按成因返回 [`HcurOrder`]，由调用方区分
    /// 错误消息（ENG-0032 不再统一误报 "order drift"）。
    fn hcur_order_ok(
        &self,
        name: &str,
        shared: Option<&[crate::exec::Row]>,
    ) -> Result<HcurOrder, EngineError> {
        let Some(shared) = shared else {
            return Ok(HcurOrder::CountMismatch);
        };
        let sql = format!("SELECT * FROM {name} ORDER BY __sd_rn");
        let mut stream = match self.ex.query(&sql, &crate::exec::BindMap::new()) {
            Ok(s) => s,
            Err(_) => {
                // 查询失败中止事务：复位后由调用方按既定口径处理
                // （前置证明失败→串行兜底；重建路径→invariant 上抛，
                // 复位无害）。
                self.ex.execute("ROLLBACK", &crate::exec::BindMap::new())?;
                self.ex
                    .execute("BEGIN TRANSACTION", &crate::exec::BindMap::new())?;
                return Ok(HcurOrder::ProbeFailed);
            }
        };
        let rows = stream.fetch_many(usize::MAX);
        if rows.len() != shared.len() {
            return Ok(HcurOrder::CountMismatch);
        }
        for (i, a) in rows.iter().enumerate() {
            if a.len() != shared[i].len() + 1 {
                return Ok(HcurOrder::WidthMismatch);
            }
            if a[..shared[i].len()] != shared[i][..] {
                return Ok(HcurOrder::RowMismatch);
            }
            if a.last() != Some(&crate::value::SpinValue::Int(i as i64)) {
                return Ok(HcurOrder::RnMismatch);
            }
        }
        Ok(HcurOrder::Ok)
    }

    /// 段批写前的物化表探活：重放行的原生例程调用可回滚会话临时表
    /// （与 `sd_fn*/sd_bf*` 同因）。缺失则重建并重做位置校验——
    /// 重建行序漂移无法在已提交段写后回退，只能 invariant 上抛
    /// （理论上同查询同数据重建序应稳定，漂移属内核行为异常）。
    fn ensure_hcur(
        &mut self,
        name: &str,
        plan: &ConversionPlan,
        frame: &Frame,
        spec: &crate::ir::set_oriented::HybridSpec,
        shared: Option<&[crate::exec::Row]>,
    ) -> Result<(), EngineError> {
        let probe = format!("SELECT 1 FROM {name} LIMIT 0");
        if self.ex.query(&probe, &crate::exec::BindMap::new()).is_ok() {
            return Ok(());
        }
        // 重建数据源 = 冻结的 `shared_rows`（游标绑定序），**绝不重跑
        // `hcur_sql`**：游标源表可能已被循环体写改，重查会得到漂移
        // 内容（实测 ENG-0032 order drift）。列名/类型经 `LIMIT 0`
        // 空结果物化获得（依赖的 sd_* 物化表由调用方先行刷新）。
        let Some(shared) = shared else {
            return Err(EngineError::invariant(
                32,
                "hybrid rebuild without frozen cursor rows",
            ));
        };
        let bm = crate::runtime::interp::resolve_binds(&plan.binds, frame)?;
        let ddl = format!(
            "CREATE OR REPLACE TEMP TABLE {name} AS {} LIMIT 0",
            spec.hcur_sql
        );
        self.ex.execute(&ddl, &bm)?;
        self.load_hcur_rows(name, shared)?;
        match self.hcur_order_ok(name, Some(shared))? {
            HcurOrder::Ok => Ok(()),
            HcurOrder::ProbeFailed => Err(EngineError::invariant(
                32,
                "sd_cur rematerialize probe failed",
            )),
            HcurOrder::CountMismatch => Err(EngineError::invariant(
                32,
                "sd_cur rematerialize row count drift",
            )),
            HcurOrder::WidthMismatch | HcurOrder::RowMismatch => Err(EngineError::invariant(
                32,
                "sd_cur rematerialize order drift",
            )),
            HcurOrder::RnMismatch => Err(EngineError::invariant(
                32,
                "sd_cur rematerialize row-number drift",
            )),
        }
    }

    /// `__sd_hcur` 重建的数据装载：冻结游标行 + 0 基 `__sd_rn` 逐块
    /// `INSERT ... VALUES`（分块控制单语句规模；仅重建路径触发，
    /// 性能不敏感）。
    fn load_hcur_rows(&self, name: &str, shared: &[crate::exec::Row]) -> Result<(), EngineError> {
        const CHUNK: usize = 256;
        for (ci, chunk) in shared.chunks(CHUNK).enumerate() {
            let base = ci * CHUNK;
            let mut values = String::new();
            for (j, row) in chunk.iter().enumerate() {
                if j > 0 {
                    values.push_str(", ");
                }
                values.push('(');
                for (k, v) in row.iter().enumerate() {
                    if k > 0 {
                        values.push_str(", ");
                    }
                    values.push_str(&crate::runtime::interp::sql_literal(v));
                }
                values.push_str(&format!(", {}", base + j));
                values.push(')');
            }
            self.ex.execute(
                &format!("INSERT INTO {name} VALUES {values}"),
                &crate::exec::BindMap::new(),
            )?;
        }
        Ok(())
    }

    /// 回收混合物化临时表；失败静默（会话结束自然回收，唯一表名
    /// 不影响后续区域）。
    fn drop_hcur_table(&self, name: &str) {
        let _ = self.ex.execute(
            &format!("DROP TABLE IF EXISTS {name}"),
            &crate::exec::BindMap::new(),
        );
    }

    /// 混合前置证明失败的整区域串行兜底（游标尚未 OPEN）：从
    /// `open_pc` 起原生续跑，FETCH 驱动整段循环，trace/步数/trap/
    /// 事务语义与串行逐语句一致；已上报 Skipped，不产生批写。
    fn hybrid_serial_from_open(
        &mut self,
        routine: &CompiledRoutine,
        plan: &ConversionPlan,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        reason: SkipReason,
    ) -> Result<Option<RunOutcome>, EngineError> {
        self.push_report(&routine.name, Status::Skipped { reason });
        let out = self.run_prog(
            &routine.prog,
            frame,
            sink,
            limits,
            RunSpan {
                start_pc: plan.open_pc,
                stop_pc: Some(plan.close_pc + 1),
                exit_pc: None,
                entry_exec: false,
            },
        )?;
        Ok((!matches!(out, RunOutcome::Stopped)).then_some(out))
    }

    /// 混合前置证明失败的串行兜底：游标已 OPEN 且 `fetched == 0`，
    /// 从 `fetch_pc` 起原生续跑——FETCH 驱动整段循环，trace/步数/
    /// trap/事务语义与串行逐语句一致；已上报 Skipped，不产生批写。
    fn hybrid_serial_fallback(
        &mut self,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        spec: &crate::ir::set_oriented::HybridSpec,
        reason: SkipReason,
    ) -> Result<Option<RunOutcome>, EngineError> {
        self.push_report(&routine.name, Status::Skipped { reason });
        let out = self.run_prog(
            &routine.prog,
            frame,
            sink,
            limits,
            RunSpan {
                start_pc: spec.fetch_pc,
                stop_pc: None,
                exit_pc: None,
                entry_exec: false,
            },
        )?;
        Ok((!matches!(out, RunOutcome::Stopped)).then_some(out))
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
        let report = Report {
            routine: routine.to_string(),
            status,
        };
        self.set_oriented_reports.push(report.clone());
        // D8：即时回调先于入队返回——`call --on-skip` 的判定行在区域
        // 裁决点输出，不等整次调用返回（OFF 路径不产生报告，不触发）。
        if let Some(cb) = &mut self.on_set_oriented_report {
            cb(&report);
        }
    }

    fn run_prog(
        &self,
        prog: &crate::ir::IrProg,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        span: RunSpan,
    ) -> Result<RunOutcome, EngineError> {
        self.run_prog_on(self.ex.as_ref(), prog, frame, sink, limits, span)
    }

    /// Skipped 回退统一入口（issue #65）：区域内可证的静态 `SELECT INTO`
    /// 由 `lookup_outcome` 包装执行器做逐键 outcome 缓存；无可证站点
    /// 或区域不可抽取时与 `run_prog` 逐字同路径。
    fn run_fallback(
        &self,
        routine: &CompiledRoutine,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        span: RunSpan,
    ) -> Result<RunOutcome, EngineError> {
        let prog = &routine.prog;
        if let Ok(region) = crate::ir::set_oriented::region::extract(prog) {
            // 分段续跑（守卫运行态拒绝）只允许缓存与 span 边界一致的
            // 区域，防止把别的区域的站点证据套到本段。
            let span_matches = span.start_pc == 0
                || (span.start_pc == region.open_pc && span.stop_pc == Some(region.close_pc + 1));
            if span_matches {
                if let Some(ex) = crate::ir::set_oriented::lookup_outcome::prepare(
                    prog,
                    &region,
                    self.ex.as_ref(),
                    &routine.name,
                ) {
                    // R3：压入例程作用域——嵌套例程经 run_with_catalog
                    // 复用本执行器，其同 SQL 文本查询按 owner 失配
                    // bypass，站点身份成为匹配一等键。
                    let _scope = crate::ir::set_oriented::lookup_outcome::push_scope(&routine.name);
                    return self.run_prog_on(&ex, prog, frame, sink, limits, span);
                }
            }
        }
        self.run_prog(prog, frame, sink, limits, span)
    }

    fn run_prog_on(
        &self,
        ex: &dyn SqlExecutor,
        prog: &crate::ir::IrProg,
        frame: &mut Frame,
        sink: &mut dyn TraceSink,
        limits: RunLimits,
        span: RunSpan,
    ) -> Result<RunOutcome, EngineError> {
        crate::runtime::run_spanned(
            prog,
            frame,
            ex,
            sink,
            &self.catalog,
            0,
            RunControl { limits, span },
        )
    }
}

/// D7 选臂结果：`Plan` = 选中计划与臂标签；`NotTotal` = 判别式
/// 0/>1 行或 NULL，无法证选臂（§12.3(d).4 保守回退）。
enum ArmPick<'a> {
    Plan(&'a ConversionPlan, Arm),
    NotTotal,
}

/// 混合调度运行上下文：异常行集 + `sd_cur` 物化临时表名。
struct HybridRun<'a> {
    exc: std::collections::BTreeSet<u64>,
    hcur_name: &'a str,
    /// 报告用 SQL：占位符版（`__SD_HCUR__`），不含会话唯一临时表名。
    report_sql: &'a str,
}

/// 混合物化临时表序号：进程内唯一，递归/嵌套区域互不覆盖。
static HCUR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_hcur_seq() -> u64 {
    HCUR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// `__SD_HCUR__` 占位符 → 实际临时表名：批写/段模板/异常守卫 SQL
/// 全部替换（`route_alt` 仅入口期消费，此后不再执行，不替换）。
fn substitute_hcur(plan: &ConversionPlan, name: &str) -> ConversionPlan {
    let mut p = plan.clone();
    p.sql = p.sql.replace(crate::ir::set_oriented::SD_HCUR_TABLE, name);
    if let Some(seg) = &mut p.seg_sql {
        *seg = seg.replace(crate::ir::set_oriented::SD_HCUR_TABLE, name);
    }
    for (sql, _) in &mut p.exc_guards {
        *sql = sql.replace(crate::ir::set_oriented::SD_HCUR_TABLE, name);
    }
    p
}

fn shared_rows_slice(
    shared: &Option<std::sync::Arc<Vec<crate::exec::Row>>>,
) -> Option<&[crate::exec::Row]> {
    shared.as_ref().map(|r| r.as_slice())
}

/// `hcur_order_ok` 的分型结果（ENG-0032 按成因区分消息）：
/// `Ok` 通过；`ProbeFailed` 校验查询本身失败（已事务复位）；
/// `CountMismatch` 行数不符；`WidthMismatch`/`RowMismatch` 行宽或
/// 内容/行序失配；`RnMismatch` 末列 `__sd_rn` 与行下标不符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HcurOrder {
    Ok,
    ProbeFailed,
    CountMismatch,
    WidthMismatch,
    RowMismatch,
    RnMismatch,
}
