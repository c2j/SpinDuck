//! 顺序走查循环体，保留查找、分支及内联求值义务的先后关系。

use super::*;

mod agg_walk;
use agg_walk::agg_candidates;

#[derive(Default)]
pub(super) struct Body<'a> {
    pub insert_stmt: Option<&'a IrStmt>,
    pub tail_commit: bool,
    pub has_trap: bool,
    pub sinks: Vec<(VarName, SinkOp, String, String)>,
    pub lookups: Vec<lookup::LookupQuery>,
    pub tuples: Vec<lookup::TupleQuery>,
    /// LookupTuple 槽位变量：调用被集合化跳过后不得再被赋值（防陈旧绑定）。
    tuple_vars: std::collections::BTreeSet<VarName>,
    pub route: Option<(VarName, String)>,
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
    /// 当前 IF 门：(条件, 汇合点 pc)；pc 到达汇合点即失效。
    gate: Option<(IrExpr, usize)>,
}

impl<'a> Body<'a> {
    pub(super) fn walk(ctx: &Context<'a, '_>) -> Result<Self, SkipReason> {
        let mut body = Self {
            agg_candidates: agg_candidates(ctx),
            ..Default::default()
        };
        let mut pc = ctx.region.open_pc + 3;
        while pc < ctx.region.close_pc - 1 {
            pc = body.statement(ctx, pc)?;
        }
        Ok(body)
    }

    fn statement(&mut self, ctx: &Context<'a, '_>, pc: usize) -> Result<usize, SkipReason> {
        if self.gate.as_ref().is_some_and(|(_, until)| pc >= *until) {
            self.gate = None;
        }
        let stmt = &ctx.prog.code[pc];
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

    fn trap(&mut self, ctx: &Context<'_, '_>, pc: usize) -> Result<usize, SkipReason> {
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
        // Strict 的 reset_session 回滚语义与跳行过滤不等价。
        if ctx.mode == SetOrientedMode::BatchAtomic {
            if let Some(lq) = lookup::skip_lookup_at(ctx.prog, pc, ctx.region, ctx.catalog)? {
                self.add_group(vec![lq])?;
                self.has_trap = true;
                return Ok(pc + 2);
            }
        }
        if let Some((cond, group, resume)) =
            lookup::fallback_if_group_at(ctx.prog, pc, ctx.region, ctx.catalog)?
        {
            if self.route.is_some() {
                return Err(SkipReason::UnsupportedControlFlow);
            }
            let r = route::cond(&cond, &self.lookups).ok_or(SkipReason::UnsupportedControlFlow)?;
            self.add_group(group)?;
            self.route = Some(r);
            self.has_trap = true;
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
            self.gate = Some((cond.clone(), to));
            return Ok(pc + 1);
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
        ctx: &Context<'_, '_>,
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
                // 聚合槽位的前置清零：INTO/处理器在所有路径上覆写，属死存储。
                if !dead::eliminable(ctx.prog, v, c) && !self.agg_candidates.contains(v) {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
                self.dead.push(v.clone());
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

    /// 函数调用赋值：LookupTuple 内联优先，纯函数符号内联兜底。
    fn assign_call(
        &mut self,
        ctx: &Context<'_, '_>,
        v: &VarName,
        name: &str,
        args: &[IrExpr],
    ) -> Result<(), SkipReason> {
        self.reject_reassignment(v)?;
        let site = catalog_summary::CallSite {
            args,
            ret_var: v,
            prog: ctx.prog,
            region: ctx.region,
            record: &ctx.region.record,
        };
        if let Some(tq) = catalog_summary::tuple_query(ctx.catalog, name, &site)? {
            for slot in &tq.slots {
                self.reject_slot(&slot.var)?;
            }
            self.tuple_vars
                .extend(tq.slots.iter().map(|s| s.var.clone()));
            self.tuples.push(tq);
            return Ok(());
        }
        let call = catalog_summary::inline_pure_call(ctx.catalog, name, args)
            .ok_or(SkipReason::UnsupportedCall)?;
        catalog_summary::resolve_locals(&mut self.inlined);
        self.record_checks(v, call.checks);
        match call.expr {
            IrExpr::Const(c) => self.folded.push((v.clone(), c)),
            expr => self.inlined.push((v.clone(), expr)),
        }
        Ok(())
    }

    /// 处理器内 noop 调用放行，其余例程调用拒绝。
    fn call_noop(
        &self,
        ctx: &Context<'_, '_>,
        pc: usize,
        name: &str,
        args: &[IrExpr],
        out_targets: &[Option<AssignTarget>],
    ) -> Result<(), SkipReason> {
        if !region::in_handler_body(ctx.prog, ctx.region, pc)
            || !catalog_summary::call_is_noop(ctx.catalog, ctx.prog, name, args, out_targets)
        {
            return Err(SkipReason::UnsupportedCall);
        }
        Ok(())
    }

    /// LookupTuple 槽位变量与既有绑定/查找/写汇冲突即拒绝。
    fn reject_slot(&self, v: &VarName) -> Result<(), SkipReason> {
        if self.folded.iter().any(|(fv, _)| fv == v)
            || self.inlined.iter().any(|(iv, _)| iv == v)
            || self.sinks.iter().any(|(sv, _, _, _)| sv == v)
            || self.lookups.iter().any(|l| l.var == *v)
            || self.tuple_vars.contains(v)
        {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        Ok(())
    }

    fn reject_reassignment(&self, v: &VarName) -> Result<(), SkipReason> {
        if self.folded.iter().any(|(fv, _)| fv == v) || self.inlined.iter().any(|(iv, _)| iv == v) {
            return Err(SkipReason::UnsupportedControlFlow);
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
        if self.folded.iter().any(|(v, _)| {
            binds
                .iter()
                .any(|(_, src)| matches!(src, BindSrc::Var(bv) if bv == v))
        }) {
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
        Ok(())
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
