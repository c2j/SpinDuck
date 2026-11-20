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
        // §5.1-9 分派：处理器 CallRoutine 全部可证 NoOp → 通用 trap
        // 路径保持原行过滤语义；摘要 Impure（真实副作用，如 inst_log）
        // → bad_rows==0 守卫整段退回；其余（未登记/形参不可证/纯表达式
        // 函数）与通用路径同口径 UnsupportedCall。
        match handler_calls(ctx, &shape)? {
            HandlerCalls::AllNoop => return Ok(None),
            HandlerCalls::Effectful => {}
            HandlerCalls::Unproven => return Err(SkipReason::UnsupportedCall),
        }
        if self.insert_stmt.is_some() {
            return Err(SkipReason::UnsupportedWrite);
        }
        self.insert_stmt = Some(&ctx.prog.code[pc + 1]);
        self.insert_log = true;
        self.has_trap = true;
        Ok(Some(shape.resume))
    }
}

/// 处理器体 CallRoutine 的副作用分类。
enum HandlerCalls {
    /// 全部可证 NoOp：坏行仅表现为跳行，交通用 trap 路径。
    AllNoop,
    /// 存在摘要 Impure 的真实副作用调用：须 bad_rows==0 守卫。
    Effectful,
    /// 存在不可证调用（未登记/实参或写回复核失败/纯表达式）：与通用
    /// 路径同口径 UnsupportedCall。
    Unproven,
}

/// 扫描处理器体：CallRoutine 按 call_is_noop → Impure 摘要 → 其余的
/// 顺序裁决；其余语句形态已由 insert_log_handler 白名单限定。
fn handler_calls(
    ctx: &Context<'_, '_>,
    shape: &region::InsertLogTrap,
) -> Result<HandlerCalls, SkipReason> {
    let mut effectful = false;
    for bpc in shape.body_start..=shape.body_end {
        let Some(IrStmt::CallRoutine {
            name,
            args,
            out_targets,
        }) = ctx.prog.code.get(bpc)
        else {
            continue;
        };
        if catalog_summary::call_is_noop(ctx.catalog, ctx.prog, name, args, out_targets) {
            continue;
        }
        match catalog_summary::summarize(ctx.catalog, name) {
            catalog_summary::Summary::Impure => effectful = true,
            _ => return Ok(HandlerCalls::Unproven),
        }
    }
    Ok(if effectful {
        HandlerCalls::Effectful
    } else {
        HandlerCalls::AllNoop
    })
}
