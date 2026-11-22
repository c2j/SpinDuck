//! 出口活跃变量 must-写证明（§4.2 live_out_check 的流敏感版）：
//! 区域内赋值且后缀读取的变量，要求后缀 CFG 上每条从区域出口到达
//! 读取点的可达路径都先写过它——入口处的区域残留值不计入。循环回边
//! 由 must 不动点覆盖；后缀跳回区域内部的路径无法证明，保守拒绝。

use super::{collect_stmt_reads, CursorRegion, SkipReason};
use crate::ir::{AssignTarget, IrProg, IrStmt, VarName};
use std::collections::BTreeSet;

/// 区域写出的标量变量（Assign Var / Fetch Var / SqlQuery INTO Var）。
fn region_writes(prog: &IrProg, region: &CursorRegion) -> BTreeSet<VarName> {
    let mut out = BTreeSet::new();
    for stmt in &prog.code[region.open_pc..=region.close_pc] {
        match stmt {
            IrStmt::Assign {
                target: AssignTarget::Var(v),
                ..
            } => {
                out.insert(v.clone());
            }
            IrStmt::Fetch { targets, .. } | IrStmt::SqlQuery { into: targets, .. } => {
                for t in targets {
                    if let AssignTarget::Var(v) = t {
                        out.insert(v.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 单语句写出的标量变量（含 CallRoutine OUT/INOUT 目标写回）。
fn stmt_writes(stmt: &IrStmt, out: &mut BTreeSet<VarName>) {
    match stmt {
        IrStmt::Assign {
            target: AssignTarget::Var(v),
            ..
        } => {
            out.insert(v.clone());
        }
        IrStmt::Fetch { targets, .. } | IrStmt::SqlQuery { into: targets, .. } => {
            for t in targets {
                if let AssignTarget::Var(v) = t {
                    out.insert(v.clone());
                }
            }
        }
        IrStmt::CallRoutine { out_targets, .. } => {
            for t in out_targets.iter().flatten() {
                if let AssignTarget::Var(v) = t {
                    out.insert(v.clone());
                }
            }
        }
        _ => {}
    }
}

/// 静态 CFG 后继（与 writebuf_policy::successors 同口径，区域后缀局部）。
fn successors(prog: &IrProg, pc: usize) -> Vec<usize> {
    match &prog.code[pc] {
        IrStmt::Jump(to) => vec![*to],
        IrStmt::JumpIf { to, .. } | IrStmt::JumpIfNot { to, .. } => vec![*to, pc + 1],
        IrStmt::Return { .. } | IrStmt::TxRollback | IrStmt::Raise { .. } => Vec::new(),
        _ if pc + 1 < prog.code.len() => vec![pc + 1],
        _ => Vec::new(),
    }
}

/// 出口活跃变量检查：区域内赋值的变量若在后缀被读取，其每个可达
/// 读取点必须被后缀写点 must-覆盖，否则 LiveOutState。
pub(crate) fn live_out_check(prog: &IrProg, region: &CursorRegion) -> Result<(), SkipReason> {
    let assigned = region_writes(prog, region);
    let live = live_vars(prog, region, &assigned);
    if live.is_empty() {
        return Ok(());
    }
    let must = must_write_sets(prog, region.close_pc + 1, &live)?;
    for (pc, must_set) in must.iter().enumerate().skip(region.close_pc + 1) {
        let mut reads = BTreeSet::new();
        collect_stmt_reads(&prog.code[pc], &mut reads);
        if reads
            .iter()
            .any(|v| live.contains(v) && !must_set.contains(v))
        {
            return Err(SkipReason::LiveOutState);
        }
    }
    Ok(())
}

/// 区域赋值 ∩ 后缀读取 = 待证活跃变量集。
fn live_vars(
    prog: &IrProg,
    region: &CursorRegion,
    assigned: &BTreeSet<VarName>,
) -> BTreeSet<VarName> {
    let mut reads = BTreeSet::new();
    for stmt in &prog.code[region.close_pc + 1..] {
        collect_stmt_reads(stmt, &mut reads);
    }
    assigned.intersection(&reads).cloned().collect()
}

/// must-写不动点：`input[n]` = 所有入口→n 可达路径上都已写出的变量集
/// （入口 = 区域出口下一条，区域残留不计）。返回各 pc 的 input 集；
/// 不可达 pc 与 entry 自身为 ∅。后缀跳回区域内部 = 无法证明，
/// 保守 LiveOutState。
fn must_write_sets(
    prog: &IrProg,
    entry: usize,
    vars: &BTreeSet<VarName>,
) -> Result<Vec<BTreeSet<VarName>>, SkipReason> {
    let n = prog.code.len();
    let mut succ = vec![Vec::new(); n];
    for (pc, outs) in succ.iter_mut().enumerate().skip(entry) {
        for s in successors(prog, pc) {
            // 后缀跳回区域/前缀：路径重新进入已转换区域，残留值口径
            // 无法由本分析刻画，保守拒绝。
            if s < entry {
                return Err(SkipReason::LiveOutState);
            }
            if s < n {
                outs.push(s);
            }
        }
    }
    let reachable = reachable_from(entry, &succ);
    let preds = predecessors(&succ, &reachable);
    // 顶 = vars（不可达 pc 保持顶，死代码读取天然放行）；entry = ∅。
    let mut input = vec![vars.clone(); n];
    input[entry] = BTreeSet::new();
    loop {
        let mut changed = false;
        for pc in entry + 1..n {
            if !reachable[pc] || preds[pc].is_empty() {
                continue;
            }
            let merged = merge_preds(prog, &preds[pc], &input, vars);
            if merged != input[pc] {
                input[pc] = merged;
                changed = true;
            }
        }
        if !changed {
            return Ok(input);
        }
    }
}

fn reachable_from(entry: usize, succ: &[Vec<usize>]) -> Vec<bool> {
    let mut seen = vec![false; succ.len()];
    let mut queue = vec![entry];
    seen[entry] = true;
    while let Some(pc) = queue.pop() {
        for &s in &succ[pc] {
            if !seen[s] {
                seen[s] = true;
                queue.push(s);
            }
        }
    }
    seen
}

fn predecessors(succ: &[Vec<usize>], reachable: &[bool]) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); succ.len()];
    for (pc, ss) in succ.iter().enumerate() {
        for &s in ss {
            if reachable[s] {
                preds[s].push(pc);
            }
        }
    }
    preds
}

/// `n` 的新输入 = 全体可达前驱 `out(p)` 的交集；`out(p)` = input(p) ∪ 写(p)。
fn merge_preds(
    prog: &IrProg,
    pred_list: &[usize],
    input: &[BTreeSet<VarName>],
    vars: &BTreeSet<VarName>,
) -> BTreeSet<VarName> {
    let mut acc: Option<BTreeSet<VarName>> = None;
    for &p in pred_list {
        let mut out = input[p].clone();
        let mut writes = BTreeSet::new();
        stmt_writes(&prog.code[p], &mut writes);
        out.extend(writes.intersection(vars).cloned());
        acc = Some(match acc {
            None => out,
            Some(a) => a.intersection(&out).cloned().collect(),
        });
    }
    acc.unwrap_or_default()
}
