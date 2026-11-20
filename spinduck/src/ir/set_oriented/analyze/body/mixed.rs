//! 混合常量兜底 trap 的走查（region::mixed 结构面）：识别后把保护区
//! 逐语句交回 statement 走查（嵌套 trap/查找/赋值各自证明），深度 0
//! SELECT INTO 在对应 pc 消费待定查找；走完保护区即证明处理器不可达，
//! 跳过处理器体从续跑 pc 继续。保护区内被跳过的待定查找 = 死代码未证，
//! 保守拒绝。

use super::*;

impl<'a> Body<'a> {
    /// 识别并走查混合兜底 trap；形态不符 = Ok(None)。
    pub(super) fn mixed_trap(
        &mut self,
        ctx: &Context<'a, '_>,
        pc: usize,
    ) -> Result<Option<usize>, SkipReason> {
        let Some(group) = lookup::mixed_fallback_group_at(ctx.prog, pc, ctx.region, ctx.catalog)?
        else {
            return Ok(None);
        };
        self.pending_lookups.extend(group.lookups);
        self.has_trap = true;
        let mut p = pc + 1;
        while p <= group.pop_pc {
            p = self.statement(ctx, p)?;
        }
        if self
            .pending_lookups
            .range(pc + 1..=group.pop_pc)
            .next()
            .is_some()
        {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        Ok(Some(group.resume_pc))
    }

    /// 保护区深度 0 SELECT INTO 命中待定查找：登记组并续走下一语句。
    pub(super) fn pending_lookup(&mut self, pc: usize) -> Result<usize, SkipReason> {
        let lq = self
            .pending_lookups
            .remove(&pc)
            .ok_or(SkipReason::UnsupportedControlFlow)?;
        self.add_group(vec![lq])?;
        Ok(pc + 1)
    }
}
