//! INSERT 跳过型处理器结构面（设计 §3.5 第 4 点 / §5.1-9）：
//! `PushTrap` 保护区恰为一条 `SqlExec`（本区域 INSERT），处理器体只含
//! `Nop` / `CallRoutine` / `Var := Const` 与收尾 `Jump`。坏行数由入口
//! 守卫裁决（`bad_rows == 0` 时处理器不可达，`inst_log` 等副作用不观测）；
//! 存在坏行则整段退回原解释器，不得把处理器改写成行过滤。

use super::CursorRegion;
use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt};

/// 被识别的 INSERT 跳过型 trap：处理器体区间与续跑 pc。
pub(crate) struct InsertLogTrap {
    /// 处理器体语句区间 `[body_start, body_end)`（`body_end` 为收尾 Jump 下标）。
    pub body_start: usize,
    pub body_end: usize,
    /// 处理器收尾 Jump 的目标（区域走查续跑 pc）。
    pub resume: usize,
}

/// pc 处 PushTrap 是否为 INSERT 跳过型处理器；是则返回结构面。
/// 保护区必须恰好一条 SqlExec——夹带其它语句时异常来源不唯一，拒绝。
pub(crate) fn insert_log_handler(prog: &IrProg, pc: usize) -> Option<InsertLogTrap> {
    let IrStmt::PushTrap { entries } = prog.code.get(pc)? else {
        return None;
    };
    let [entry] = entries.as_slice() else {
        return None;
    };
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return None;
    }
    // 保护区 = 一条 SqlExec + 其后可选的 TxCommit（BEGIN 内 COMMIT 形态，
    // admit_effects 的 guarded_tail 同口径放行）。
    let mut q = pc + 1;
    if !matches!(prog.code.get(q), Some(IrStmt::SqlExec { .. })) {
        return None;
    }
    q += 1;
    while matches!(prog.code.get(q), Some(IrStmt::TxCommit)) {
        q += 1;
    }
    if !matches!(prog.code.get(q), Some(IrStmt::PopTrap)) {
        return None;
    }
    let join = entry.body_end + 1;
    if !matches!(prog.code.get(q + 1), Some(IrStmt::Jump(t)) if *t == join)
        || !matches!(prog.code.get(entry.body_end), Some(IrStmt::Jump(t)) if *t == join)
    {
        return None;
    }
    if !handler_body_ok(prog, entry.body_start, entry.body_end)
        || !prog.code[entry.body_start..entry.body_end]
            .iter()
            .any(|st| matches!(st, IrStmt::CallRoutine { .. }))
    {
        return None;
    }
    Some(InsertLogTrap {
        body_start: entry.body_start,
        body_end: entry.body_end,
        resume: join,
    })
}

/// 处理器体白名单：Nop / 任意 CallRoutine / `Var := Const` / `Jump`。
/// 坏行存在时整段退回解释器逐条执行，处理器内容不参与集合化投影，
/// 因此不要求 callee 可证 NoOp、Jump 目标不校验——准入义务全部由
/// bad_rows==0 守卫承担（守卫通过时处理器整体不可达）。
fn handler_body_ok(prog: &IrProg, start: usize, end: usize) -> bool {
    (start..end).all(|bpc| {
        matches!(
            prog.code.get(bpc),
            Some(IrStmt::Nop)
                | Some(IrStmt::Jump(_))
                | Some(IrStmt::CallRoutine { .. })
                | Some(IrStmt::Assign {
                    target: AssignTarget::Var(_),
                    expr: IrExpr::Const(_),
                })
        )
    })
}

/// 区域内全部 INSERT 跳起型处理器体区间（admit_effects / write_set 共用）。
pub(crate) fn insert_log_bodies(prog: &IrProg, region: &CursorRegion) -> Vec<(usize, usize)> {
    (region.open_pc..=region.close_pc)
        .filter_map(|p| insert_log_handler(prog, p))
        .map(|t| (t.body_start, t.body_end))
        .collect()
}
