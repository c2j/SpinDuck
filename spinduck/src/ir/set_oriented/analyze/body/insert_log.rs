//! §3.5-4 INSERT 跳起型处理器走查分支（自 body.rs 拆出）：
//! 坏行数由入口守卫裁决，存在坏行则整段退回，不得改写成行过滤。

use super::*;

impl<'a> Body<'a> {
    /// 识别成功返回续跑 pc（跳过处理器体）；非 BatchAtomic 或形态不符
    /// 返回 Ok(None)，由 trap() 继续走通用路径。
    pub(super) fn insert_log_trap(
        &mut self,
        ctx: &Context<'a, '_>,
        pc: usize,
    ) -> Result<Option<usize>, SkipReason> {
        if ctx.mode != SetOrientedMode::BatchAtomic {
            return Ok(None);
        }
        let Some(shape) = region::insert_log_handler(ctx.prog, pc) else {
            return Ok(None);
        };
        if self.insert_stmt.is_some() {
            return Err(SkipReason::UnsupportedWrite);
        }
        self.insert_stmt = Some(&ctx.prog.code[pc + 1]);
        self.insert_log = true;
        self.has_trap = true;
        Ok(Some(shape.resume))
    }
}
