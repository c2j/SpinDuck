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
        // 深度 0 SELECT INTO 命中待定查找后，其 miss/dup 会让处理器跳至
        // 续跑 pc、跳过保护区内其后全部语句（含后续待定查找——首查找落空
        // 时次查找的 INTO 变量由处理器兜底常量写回，与批写独立求值的
        // 命中值不等价）。故消费首个待定查找后仅允许 PopTrap/Nop 收尾，
        // 其余语句一律保守拒绝。
        let mut seen_lookup = false;
        while p <= group.pop_pc {
            if self.pending_lookups.contains_key(&p) {
                if seen_lookup {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
                seen_lookup = true;
            } else if seen_lookup && !matches!(ctx.prog.code[p], IrStmt::PopTrap | IrStmt::Nop) {
                return Err(SkipReason::UnsupportedControlFlow);
            }
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
