//! 顺序走查循环体，保留查找、分支及内联求值义务的先后关系。

use super::*;

mod agg_walk;
mod arith_walk;
mod call;
mod insert_log;
mod mixed;
use agg_walk::agg_candidates;

/// IF 门种类：§3.4 聚合字段覆写 / §3.3 条件算术覆写（THEN 恰一条赋值，
/// 走查直接跳过该语句，投影时组装外层 CASE）。
enum Gate {
    Agg(IrExpr),
    Arith {
        cond: IrExpr,
        var: VarName,
        expr: IrExpr,
    },
}

#[derive(Default)]
pub(super) struct Body<'a> {
    pub insert_stmt: Option<&'a IrStmt>,
    pub tail_commit: bool,
    pub has_trap: bool,
    /// §3.5-4 INSERT 跳起型处理器：坏行数由入口守卫裁决。
    pub insert_log: bool,
    pub sinks: Vec<(VarName, SinkOp, String, String)>,
    pub lookups: Vec<lookup::LookupQuery>,
    pub tuples: Vec<lookup::TupleQuery>,
    /// LookupTuple 槽位变量：调用被集合化跳过后不得再被赋值（防陈旧绑定）。
    tuple_vars: std::collections::BTreeSet<VarName>,
    pub route: Option<(VarName, String)>,
    /// route 决策点（D7 双臂走查 divert 输入；两种 route 形态见
    /// [`super::RouteSite`]）。
    pub route_site: Option<super::RouteSite>,
    /// 本次走查是否为 D7 THEN 臂（裸 SqlQuery 的 case_sink 受理面）。
    then_arm: bool,
    pub folded: Vec<(VarName, crate::value::SpinValue)>,
    pub inlined: Vec<(VarName, IrExpr)>,
    pub inline_checks: Vec<(VarName, IrExpr)>,
    pub varchar_inputs: Vec<IrExpr>,
    pub inline_preconditions: std::collections::BTreeMap<String, Vec<VarName>>,
    pub dead: Vec<VarName>,
    /// 条件聚合查表（§3.4）：trap 包裹 SUM 查询。
    pub aggs: Vec<agg::AggQuery>,
    /// 聚合结果写回游标字段（可挂 IF 门），驱动 CASE 投影。
    pub field_writes: Vec<agg::FieldWrite>,
    /// 已识别聚合的 INTO 槽位变量（禁止后续赋值）。
    agg_vars: std::collections::BTreeSet<VarName>,
    /// 区域内 SqlQuery INTO 单变量的候选集（前置清零死存储证明用）。
    agg_candidates: std::collections::BTreeSet<VarName>,
    /// 当前 IF 门：(种类, 汇合点 pc)；pc 到达汇合点即失效。
    gate: Option<(Gate, usize)>,
    /// 混合兜底 trap 保护区深度 0 SELECT INTO 的待定查找（pc → 查找）。
    pending_lookups: std::collections::BTreeMap<usize, lookup::LookupQuery>,
    /// §3.3 条件算术覆写：IF 门内 `var := <DECIMAL 乘/减>`。
    pub arith_writes: Vec<ArithWrite>,
}

impl<'a> Body<'a> {
    pub(super) fn walk(ctx: &Context<'a, '_>) -> Result<Self, SkipReason> {
        Self::walk_diverted(ctx, None)
    }

    /// D7 双臂（§12.3(b) 每臂独立走查）：在 route 决策点分流进
    /// THEN 臂、臂尾跳到汇合后续走——公共后缀在本臂入口状态下独立
    /// 走查，不做两臂事实合并。trap 形态臂含 trap 语义，预置
    /// `has_trap`（PushTrap/PopTrap/handler 被 divert 跳过）。
    pub(super) fn walk_then(
        ctx: &Context<'a, '_>,
        site: super::RouteSite,
    ) -> Result<Self, SkipReason> {
        Self::walk_diverted(ctx, Some(site))
    }

    fn walk_diverted(
        ctx: &Context<'a, '_>,
        site: Option<super::RouteSite>,
    ) -> Result<Self, SkipReason> {
        let redirects: Vec<(usize, usize)> = match site {
            Some(super::RouteSite::Branch { pc, to }) => {
                vec![(pc, pc + 1), (to - 1, ctx.region.close_pc - 1)]
            }
            Some(super::RouteSite::TrapGroup {
                push_pc,
                else_start,
                resume_pc,
            }) => vec![(push_pc, push_pc + 2), (else_start - 1, resume_pc)],
            None => Vec::new(),
        };
        let trap_route = matches!(site, Some(super::RouteSite::TrapGroup { .. }));
        let mut body = Self {
            agg_candidates: agg_candidates(ctx),
            then_arm: site.is_some(),
            ..Default::default()
        };
        if trap_route {
            body.has_trap = true;
        }
        let mut pc = ctx.region.open_pc + 3;
        while pc < ctx.region.close_pc - 1 {
            if let Some((_, next)) = redirects.iter().find(|(at, _)| *at == pc) {
                pc = *next;
                continue;
            }
            pc = body.statement(ctx, pc)?;
        }
        // D7 THEN 臂事后冲突检查：折叠常量之后又被任一写汇/查找/内联
        // 事实接管同名变量 → 折叠值对覆写前的读取不再成立，整体拒绝。
        if site.is_some() {
            for (v, _) in &body.folded {
                if body.arm_fact_conflict(v, false) {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
            }
        }
        Ok(body)
    }

    /// THEN 臂折叠常量的同名事实冲突检查：变量已被查找/写汇/内联/
    /// 算术覆写/聚合/死存储等任一事实持有时拒绝（§12.3 臂内独立事实）。
    /// `include_folded`：赋值时点检查含 folded 自身（重复赋值拒绝）；
    /// 事后检查排除（folded 条目自身即被检对象）。
    fn reject_arm_fact(&self, v: &VarName) -> Result<(), SkipReason> {
        if self.arm_fact_conflict(v, true) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        Ok(())
    }

    fn arm_fact_conflict(&self, v: &VarName, include_folded: bool) -> bool {
        (include_folded && self.folded.iter().any(|(fv, _)| fv == v))
            || self.inlined.iter().any(|(iv, _)| iv == v)
            || self.sinks.iter().any(|(sv, _, _, _)| sv == v)
            || self.arith_writes.iter().any(|w| w.var == *v)
            || self.lookups.iter().any(|l| l.var == *v)
            || self.aggs.iter().any(|a| a.var == *v)
            || self.agg_vars.contains(v)
            || self.tuple_vars.contains(v)
            || self.dead.contains(v)
    }

    fn statement(&mut self, ctx: &Context<'a, '_>, pc: usize) -> Result<usize, SkipReason> {
        if self.gate.as_ref().is_some_and(|(_, until)| pc >= *until) {
            if let Some((Gate::Arith { cond, var, expr }, _)) = self.gate.take() {
                self.arith_writes.push(ArithWrite { var, cond, expr });
            } else {
                self.gate = None;
            }
        }
        let stmt = &ctx.prog.code[pc];
        if self.pending_lookups.contains_key(&pc) {
            return self.pending_lookup(pc);
        }
        match stmt {
            IrStmt::PushTrap { .. } => return self.trap(ctx, pc),
            IrStmt::SqlExec { .. } => {
                if self.insert_stmt.is_some() {
                    return Err(SkipReason::UnsupportedWrite);
                }
                self.insert_stmt = Some(stmt);
            }
            IrStmt::TxCommit if ctx.mode == SetOrientedMode::BatchAtomic => self.tail_commit = true,
            IrStmt::PopTrap | IrStmt::Nop | IrStmt::Jump(_) => self.has_trap = true,
            IrStmt::JumpIfNot { cond, to } => return self.branch(ctx, pc, cond, *to),
            IrStmt::Assign {
                target: AssignTarget::Var(v),
                expr,
            } => self.assign(ctx, v, expr)?,
            IrStmt::Assign {
                target: AssignTarget::Field { rec, field },
                expr,
            } => return self.field_assign(ctx, pc, rec, field, expr),
            IrStmt::CallRoutine {
                name,
                args,
                out_targets,
            } => self.call_noop(ctx, pc, name, args, out_targets)?,
            IrStmt::SqlQuery {
                sql,
                binds,
                into,
                fn_sites,
            } => {
                if !fn_sites.is_empty() {
                    return Err(SkipReason::UnresolvedFnSite);
                }
                // D7 THEN 臂：裸 SELECT INTO 仅受理 sys_dummy CASE 写汇
                // （入口守卫已证 total，无 miss 语义）。
                if self.then_arm {
                    let [AssignTarget::Var(var)] = into.as_slice() else {
                        return Err(SkipReason::UnsupportedControlFlow);
                    };
                    match lookup::parse_then_case_sink(sql, var.clone(), binds, &ctx.region.record)?
                    {
                        Some(lq) => {
                            self.add_group(vec![lq])?;
                            return Ok(pc + 1);
                        }
                        None => return Err(SkipReason::UnsupportedControlFlow),
                    }
                }
                if !matches!(into.as_slice(), [AssignTarget::Var(_)]) || !binds.is_empty() {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
                invariant_reads_disjoint(ctx.prog, ctx.region, sql, ctx.catalog)?;
                return Err(SkipReason::UnsupportedControlFlow);
            }
            // 其余语句未取得区域准入证明。
            _ => return Err(SkipReason::UnsupportedControlFlow),
        }
        Ok(pc + 1)
    }

    fn trap(&mut self, ctx: &Context<'a, '_>, pc: usize) -> Result<usize, SkipReason> {
        // §3.4 聚合形态是 fallback_group 的子集，必须先试聚合解析。
        if let Some((query, resume)) = agg::at(ctx.prog, pc, ctx.region, ctx.catalog)? {
            self.agg_vars.insert(query.var.clone());
            self.aggs.push(query);
            self.has_trap = true;
            return Ok(resume);
        }
        // IF 门内其余查找/陷阱投影均无条件化，语义漂移 → 拒绝；
        // 但先透出解析期全定义证明失败（如 UnprovenQueryTotalness）。
        if self.gate.is_some() {
            lookup::fallback_group_at(ctx.prog, pc, ctx.region, ctx.catalog)?;
            return Err(SkipReason::UnsupportedControlFlow);
        }
        if let Some(group) = lookup::fallback_group_at(ctx.prog, pc, ctx.region, ctx.catalog)? {
            let n = group.len();
            self.add_group(group)?;
            return Ok(pc + 2 * n + 4);
        }
        // §3.5 零异常守卫查找：处理器不改 INTO 变量，命中基数由入口守卫裁决。
        if let Some((lq, resume)) =
            vguard::carryover_group_at(ctx.prog, pc, ctx.region, ctx.catalog)?
        {
            self.add_group(vec![lq])?;
            self.has_trap = true;
            return Ok(resume);
        }
        // Strict 的 reset_session 回滚语义与跳行过滤不等价。
        if ctx.mode == SetOrientedMode::BatchAtomic {
            if let Some(lq) = lookup::skip_lookup_at(ctx.prog, pc, ctx.region, ctx.catalog)? {
                self.add_group(vec![lq])?;
                self.has_trap = true;
                return Ok(pc + 2);
            }
        }
        if let Some((cond, group, resume, else_start)) =
            lookup::fallback_if_group_at(ctx.prog, pc, ctx.region, ctx.catalog)?
        {
            if self.route.is_some() {
                return Err(SkipReason::UnsupportedControlFlow);
            }
            let r = route::cond(&cond, &self.lookups).ok_or(SkipReason::UnsupportedControlFlow)?;
            self.add_group(group)?;
            self.route = Some(r);
            self.route_site = Some(super::RouteSite::TrapGroup {
                push_pc: pc,
                else_start,
                resume_pc: resume,
            });
            self.has_trap = true;
            return Ok(resume);
        }
        if let Some(resume) = self.mixed_trap(ctx, pc)? {
            return Ok(resume);
        }
        if let Some(resume) = self.insert_log_trap(ctx, pc)? {
            return Ok(resume);
        }
        self.has_trap = true;
        Ok(pc + 1)
    }

    fn add_group(&mut self, mut group: Vec<lookup::LookupQuery>) -> Result<(), SkipReason> {
        if group
            .iter()
            .any(|lq| self.lookups.iter().any(|l| l.var == lq.var))
        {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        let gid = self.lookups.len();
        for lq in &mut group {
            lq.group = gid;
        }
        self.lookups.extend(group);
        Ok(())
    }

    fn branch(
        &mut self,
        ctx: &Context<'_, '_>,
        pc: usize,
        cond: &IrExpr,
        to: usize,
    ) -> Result<usize, SkipReason> {
        let back_pc = ctx.region.close_pc - 1;
        if to < back_pc {
            catalog_summary::resolve_locals(&mut self.inlined);
            if let Some((v, case, inputs)) = cond_reassign(ctx.prog, pc, &self.inlined, ctx.catalog)
            {
                if let Some(pos) = self.inlined.iter().position(|(iv, _)| iv == &v) {
                    self.record_checks(&v, inputs);
                    self.inline_checks.push(self.inlined[pos].clone());
                    self.inlined[pos].1 = case;
                    return Ok(to);
                }
            }
        }
        // 常量剪枝先于路由形状检查，允许 THEN 跳回体内汇合点。
        if let Some((v, c)) = route::eq_var_const(cond) {
            if let Some((_, f)) = self.folded.iter().find(|(fv, _)| fv == v) {
                if route::static_cond(f, c) == Some(false) {
                    return Ok(to);
                }
                return Err(SkipReason::UnsupportedControlFlow);
            }
        }
        // §3.4 IF 门：THEN 线性走查，字段覆写投影为外层 CASE。
        if self.gate.is_none()
            && ctx.mode == SetOrientedMode::BatchAtomic
            && agg::gate_renderable(cond)
            && to > pc + 1
            && to < back_pc
            && matches!(ctx.prog.code.get(to - 1), Some(IrStmt::Jump(t)) if *t == to)
        {
            self.gate = Some((Gate::Agg(cond.clone()), to));
            return Ok(pc + 1);
        }
        // §3.3 算术覆写门：THEN 恰一条 `v := <DECIMAL 乘/减>`。
        if let Some(next) = self.arith_gate(ctx, pc, cond, to, back_pc) {
            return Ok(next);
        }
        if self.route.is_some()
            || to <= pc + 1
            || to > back_pc
            || !matches!(ctx.prog.code.get(to - 1), Some(IrStmt::Jump(t)) if *t == back_pc)
        {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        self.route =
            Some(route::cond(cond, &self.lookups).ok_or(SkipReason::UnsupportedControlFlow)?);
        self.route_site = Some(super::RouteSite::Branch { pc, to });
        Ok(to)
    }

    fn record_checks(&mut self, v: &VarName, checks: catalog_summary::CallChecks) {
        self.varchar_inputs
            .extend(freeze_inputs(checks.varchar_inputs, &self.inlined));
        self.inline_checks.extend(
            freeze_inputs(checks.evaluations, &self.inlined)
                .into_iter()
                .map(|expr| (v.clone(), expr)),
        );
    }

    fn assign(
        &mut self,
        ctx: &Context<'a, '_>,
        v: &VarName,
        expr: &IrExpr,
    ) -> Result<(), SkipReason> {
        if self.tuple_vars.contains(v) || self.agg_vars.contains(v) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        // IF 门内仅常量赋值可证（死存储/聚合前置清零）；其余表达式
        // 的投影无条件化会语义漂移。
        if self.gate.is_some() && !matches!(expr, IrExpr::Const(_)) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        match expr {
            IrExpr::FuncCall { name, args, .. } => self.assign_call(ctx, v, name, args)?,
            IrExpr::Bin { op, l, r } => self.sink(ctx.region, v, (*op, l, r))?,
            IrExpr::Const(c) => {
                // D7 THEN 臂：活常量赋值折叠为字面量事实（臂内独立，
                // 不影响 ELSE 臂逐字路径）；真死存储仍走 dead。
                if self.then_arm {
                    if dead::eliminable(ctx.prog, v, c) {
                        self.dead.push(v.clone());
                    } else {
                        self.reject_arm_fact(v)?;
                        self.folded.push((v.clone(), c.clone()));
                    }
                // 聚合槽位的前置清零：INTO/处理器在所有路径上覆写，属死存储。
                } else if !dead::eliminable(ctx.prog, v, c) && !self.agg_candidates.contains(v) {
                    return Err(SkipReason::UnsupportedControlFlow);
                } else {
                    self.dead.push(v.clone());
                }
            }
            IrExpr::SqlFn { .. } => {
                self.reject_reassignment(v)?;
                self.inlined.push((v.clone(), expr.clone()));
            }
            // 仅上述表达式具有赋值下沉证明。
            _ => return Err(SkipReason::UnsupportedControlFlow),
        }
        if self.inlined.iter().any(|(iv, _)| iv == v) {
            self.inline_preconditions.insert(
                v.0.clone(),
                self.lookups
                    .iter()
                    .filter(|l| l.filter_on_miss)
                    .map(|l| l.var.clone())
                    .collect(),
            );
        }
        Ok(())
    }

    fn sink(
        &mut self,
        region: &region::CursorRegion,
        v: &VarName,
        bin: (BinOp, &IrExpr, &IrExpr),
    ) -> Result<(), SkipReason> {
        let op = match bin.0 {
            BinOp::Div => SinkOp::Div,
            BinOp::Concat => SinkOp::Concat,
            // 其余运算尚无写汇等价证明。
            _ => return Err(SkipReason::UnsupportedControlFlow),
        };
        let (IrExpr::Field { rec: lr, field: lf }, IrExpr::Field { rec: rr, field: rf }) =
            (bin.1, bin.2)
        else {
            return Err(SkipReason::UnsupportedControlFlow);
        };
        if lr != &region.record
            || rr != &region.record
            || self.sinks.iter().any(|(sv, _, _, _)| sv == v)
        {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        self.sinks.push((v.clone(), op, lf.clone(), rf.clone()));
        Ok(())
    }

    pub(super) fn validate(
        &self,
        ctx: &Context<'_, '_>,
        binds: &[(String, BindSrc)],
    ) -> Result<(), SkipReason> {
        self.validate_sinks(ctx, binds)?;
        // D7 THEN 臂活常量折叠：folded 变量可作 INSERT 绑定源——
        // 投影层（copy/inline.rs）按字面量渲染，逐行同值。
        if !self.then_arm
            && self.folded.iter().any(|(v, _)| {
                binds
                    .iter()
                    .any(|(_, src)| matches!(src, BindSrc::Var(bv) if bv == v))
            })
        {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        for lq in &self.lookups {
            for (_, src) in &lq.filter_binds {
                if let BindSrc::Var(v) = src {
                    // 查找结果逐行代入投影，不是帧单值绑定。
                    if self.lookups.iter().any(|other| other.var == *v) {
                        continue;
                    }
                    if !region::var_invariant_in_region(ctx.prog, ctx.region, v) {
                        return Err(SkipReason::UnsupportedControlFlow);
                    }
                }
            }
        }
        self.validate_tuples(ctx)?;
        self.validate_aggs(ctx)?;
        arith_walk::validate(self, ctx.mode)
    }

    /// LookupTuple：静态过滤变量须循环不变；OUT/返回值槽位变量在后缀
    /// 不得被读取（集合化跳过调用，帧槽不会被函数写回）。
    fn validate_tuples(&self, ctx: &Context<'_, '_>) -> Result<(), SkipReason> {
        if self.tuples.is_empty() {
            return Ok(());
        }
        for tq in &self.tuples {
            for (_, src) in &tq.filter_binds {
                if let BindSrc::Var(v) = src {
                    if self.lookups.iter().any(|l| l.var == *v) {
                        continue;
                    }
                    if !region::var_invariant_in_region(ctx.prog, ctx.region, v) {
                        return Err(SkipReason::UnsupportedControlFlow);
                    }
                }
            }
        }
        let mut reads = std::collections::BTreeSet::new();
        for stmt in &ctx.prog.code[ctx.region.close_pc + 1..] {
            region::collect_stmt_reads(stmt, &mut reads);
        }
        if self
            .tuples
            .iter()
            .flat_map(|t| t.slots.iter().map(|s| &s.var))
            .any(|v| reads.contains(v))
        {
            return Err(SkipReason::LiveOutState);
        }
        Ok(())
    }

    fn validate_sinks(
        &self,
        ctx: &Context<'_, '_>,
        binds: &[(String, BindSrc)],
    ) -> Result<(), SkipReason> {
        if self.sinks.is_empty() {
            return Ok(());
        }
        if !self.has_trap || ctx.mode != SetOrientedMode::BatchAtomic {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        for (v, op, _, _) in &self.sinks {
            if *op == SinkOp::Div
                && ctx.prog.var_types.get(&v.0) == Some(&crate::types::SpinType::Varchar)
            {
                return Err(SkipReason::UnprovenExpression);
            }
            let used = binds
                .iter()
                .filter(|(_, src)| matches!(src, BindSrc::Var(bv) if bv == v))
                .count();
            if used != 1 {
                return Err(SkipReason::UnsupportedControlFlow);
            }
        }
        Ok(())
    }
}
