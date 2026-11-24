//! SO 行级混合的计划期证明（so-exception-row-hybrid §2/R2）：
//! 判定区域是否满足"异常行单行重放"的控制流前提。
//!
//! 硬性条件（任一不满足 → `None`，计划退回普通 SO/串行语义）：
//! - 循环体/处理器内所有跳转目标 ∈ `[fetch_pc, close_pc+1]`：
//!   早退边（EXIT WHEN/GOTO 出循环）只允许落在唯一出口 pc；
//!   跳出区域的控制流无法用单行重放界定，直接拒绝。
//! - 早退目标唯一：`close_pc`（CloseCursor）与 `close_pc+1` 不得同时
//!   作为出口——`RunSpan.exit_pc` 只支持单停止点。
//! - 区域内不得出现 `TxRollback`（含处理器体）：回滚会丢弃尚未
//!   提交的段写，串行"逐行已提交"边界无法复现。`TxCommit`/
//!   `TxCommitIfWritten` 不在此列——与普通 SO 同口径折叠到尾提交
//!   （`tail_commit` 已证可折叠）；异常行重放经真实 Fetch 路径
//!   原生执行体内 COMMIT，边界语义由此保留。
//! - 区域内 `PushTrap` 的处理器体必须落在 `[fetch_pc, close_pc+1]` 内
//!   （重放 trap 栈自空开始，体外处理器不可达即语义漂移）。

use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{IrProg, IrStmt, VarName};
use std::collections::BTreeSet;

/// 混合调度规格：游标名 + FETCH pc + 唯一早退出口 pc + `sd_cur`
/// 物化 SELECT（`__sd_rn` 0 基行号，只在物化时赋一次）。
#[derive(Debug, Clone)]
pub(crate) struct HybridSpec {
    pub cursor: VarName,
    pub fetch_pc: usize,
    /// 循环唯一出口 pc（`close_pc` 或 `close_pc+1`）；无早退边时
    /// 取 `close_pc`（终止空 FETCH 的 NOTFOUND 边同样落此）。
    pub exit_pc: usize,
    /// `sd_cur` 物化查询（含 `ROW_NUMBER() OVER () - 1 AS __sd_rn`）；
    /// 计划期由 `generate::finish` 填充，`analyze` 先置空串。
    pub hcur_sql: String,
}

/// 区域混合资格证明。返回 `None` = 不满足前提，走普通路径。
pub(crate) fn analyze(prog: &IrProg, region: &CursorRegion) -> Option<HybridSpec> {
    let (open_pc, fetch_pc, close_pc) = (region.open_pc, region.fetch_pc, region.close_pc);
    let hi = close_pc.saturating_add(1);
    let mut exits = BTreeSet::new();
    for stmt in prog
        .code
        .iter()
        .take(close_pc.saturating_add(1))
        .skip(open_pc)
    {
        match stmt {
            IrStmt::PushTrap { entries } => {
                for e in entries {
                    if e.body_start < fetch_pc || e.body_start > hi || e.body_end > hi + 1 {
                        return None;
                    }
                }
            }
            IrStmt::Jump(t) | IrStmt::JumpIf { to: t, .. } | IrStmt::JumpIfNot { to: t, .. } => {
                if *t < fetch_pc || *t > hi {
                    return None;
                }
                if *t >= close_pc {
                    exits.insert(*t);
                }
            }
            IrStmt::TxRollback => return None,
            // 逃逸证明只关心控制转移/事务/陷阱边界；其余指令变体
            // （赋值/SQL/例程调用等）不改变"跳转目标 ∈ 区域"论证，
            // 显式忽略——新增跳转类变体时须回到本 match 评审。
            _ => {}
        }
    }
    let exit_pc = match exits.len() {
        0 => close_pc,
        1 => *exits.iter().next()?,
        _ => return None,
    };
    Some(HybridSpec {
        cursor: region.cursor.clone(),
        fetch_pc,
        exit_pc,
        hcur_sql: String::new(),
    })
}
