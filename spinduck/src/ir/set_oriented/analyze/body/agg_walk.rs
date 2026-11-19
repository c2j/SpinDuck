//! §3.4 条件聚合走查：IF 门字段覆写与槽位证明（自 body.rs 拆出）。

use super::*;

/// 区域内 SqlQuery INTO 单变量的候选集（前置清零死存储证明用；
/// 真实准入仍由 trap() 的聚合识别把关）。
pub(super) fn agg_candidates(ctx: &Context<'_, '_>) -> std::collections::BTreeSet<VarName> {
    ctx.prog.code[ctx.region.open_pc..ctx.region.close_pc]
        .iter()
        .filter_map(|stmt| match stmt {
            IrStmt::SqlQuery { into, .. } => match into.as_slice() {
                [AssignTarget::Var(v)] => Some(v.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

impl<'a> Body<'a> {
    /// §3.4 字段覆写：仅 `r.<field> := <agg var>`，每字段至多一次。
    pub(super) fn field_assign(
        &mut self,
        ctx: &Context<'_, '_>,
        pc: usize,
        rec: &VarName,
        field: &str,
        expr: &IrExpr,
    ) -> Result<usize, SkipReason> {
        if rec != &ctx.region.record || ctx.mode != SetOrientedMode::BatchAtomic {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        let IrExpr::Var(v) = expr else {
            return Err(SkipReason::UnsupportedControlFlow);
        };
        if !self.agg_vars.contains(v) || self.field_writes.iter().any(|w| w.field.0 == field) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        let gate = self.gate.as_ref().map(|(c, _)| c.clone());
        self.field_writes.push(agg::FieldWrite {
            field: shape::Name(field.to_string()),
            var: v.clone(),
            gate,
            pc,
        });
        Ok(pc + 1)
    }

    /// §3.4 聚合：槽位变量读取仅限字段覆写；覆写前不得有同字段读取
    /// （投影 CASE 全局生效，前读会错拿覆写后值）。
    pub(super) fn validate_aggs(&self, ctx: &Context<'_, '_>) -> Result<(), SkipReason> {
        if self.aggs.is_empty() && self.field_writes.is_empty() {
            return Ok(());
        }
        if ctx.mode != SetOrientedMode::BatchAtomic {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        for aq in &self.aggs {
            let allowed: Vec<usize> = self
                .field_writes
                .iter()
                .filter(|w| w.var == aq.var)
                .map(|w| w.pc)
                .collect();
            if allowed.is_empty() || !dead::read_only_within(ctx.prog, &aq.var, &allowed) {
                return Err(SkipReason::UnsupportedControlFlow);
            }
        }
        for w in &self.field_writes {
            if agg::field_read_before(ctx.prog, ctx.region, &ctx.region.record, &w.field.0, w.pc) {
                return Err(SkipReason::UnsupportedControlFlow);
            }
        }
        Ok(())
    }
}
