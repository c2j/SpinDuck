//! 区域内函数调用准入：LookupTuple 优先、PureExpr 符号内联兜底、
//! 处理器内 noop 放行（自 body.rs 拆出）。

use super::*;

impl<'a> Body<'a> {
    /// 函数调用赋值：LookupTuple 内联优先，纯函数符号内联兜底。
    pub(super) fn assign_call(
        &mut self,
        ctx: &Context<'a, '_>,
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
        // OUT 写回槽位：实参必为变量（inline_pure_call 已证），表达式同返回值口径登记。
        for (idx, oexpr) in call.outs {
            let IrExpr::Var(target) = &args[idx] else {
                return Err(SkipReason::UnsupportedCall);
            };
            self.reject_slot(target)?;
            match oexpr {
                IrExpr::Const(c) => self.folded.push((target.clone(), c)),
                expr => self.inlined.push((target.clone(), expr)),
            }
        }
        catalog_summary::resolve_locals(&mut self.inlined);
        Ok(())
    }

    /// 处理器内 noop 调用放行，其余例程调用拒绝。
    pub(super) fn call_noop(
        &self,
        ctx: &Context<'a, '_>,
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

    pub(super) fn reject_reassignment(&self, v: &VarName) -> Result<(), SkipReason> {
        if self.folded.iter().any(|(fv, _)| fv == v) || self.inlined.iter().any(|(iv, _)| iv == v) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        Ok(())
    }
}
