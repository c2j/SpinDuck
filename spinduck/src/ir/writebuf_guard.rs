//! V3-6 红线 1 守卫数据流（自 `writebuf_policy.rs` 拆出，AGENTS §3）：
//! 从例程入口到每个跨表修改写的 must-守卫分析。守卫 = 到达该写前，
//! 一条最终 `Buffered` 的 `INSERT ... VALUES`（目标表被该写触及）之后
//! 没有把**整个缓冲**刷掉的点——`flush_if_touches` 命中任一缓冲表即
//! 整刷（`SqlQuery`/`FoldedQuery`/非 `VALUES` 修改写同样触发），
//! `CallRoutine`/`SqlExecDynamic` 恒刷。

use super::types::IrStmt;
use super::writebuf_policy::{modifies_table, plain_insert_values, touches_table};
use std::collections::{HashMap, HashSet, VecDeque};

/// 计算「无守卫」的跨表修改写 pc 集：存在一条从例程入口（pc 0）到该写的
/// 路径，其间没有任何「最终 `Buffered`、目标表被该写触及」的 `INSERT ...
/// VALUES` 保持有效。
///
/// must-数据流：`input[n]` = 所有入口→n 路径上都仍处于守卫状态的表集
/// （到达 n 之前）。多表守卫取保守口径——到达写时守卫交集为空即视为
/// 存在无守卫路径。
pub(super) fn unguarded_writes(
    code: &[IrStmt],
    succ: &[Vec<usize>],
    candidates: &[(usize, String)],
    immediate: &HashSet<usize>,
) -> HashSet<usize> {
    let buffered = buffered_candidates(candidates, immediate);
    let targets: HashSet<&str> = buffered.values().copied().collect();
    let reachable = entry_reachable(succ);
    let preds = reachable_predecessors(succ, &reachable);
    let input = must_guard_inputs(code, &preds, &reachable, &buffered, &targets);
    unguarded_write_pcs(code, &reachable, &input)
}

/// 最终 `Buffered` 的候选：pc → 目标表。
fn buffered_candidates<'a>(
    candidates: &'a [(usize, String)],
    immediate: &HashSet<usize>,
) -> HashMap<usize, &'a str> {
    candidates
        .iter()
        .filter(|(pc, _)| !immediate.contains(pc))
        .map(|(pc, t)| (*pc, t.as_str()))
        .collect()
}

/// 入口（pc 0）可达集。
fn entry_reachable(succ: &[Vec<usize>]) -> Vec<bool> {
    let mut reachable = vec![false; succ.len()];
    if succ.is_empty() {
        return reachable;
    }
    reachable[0] = true;
    let mut queue = VecDeque::from([0usize]);
    while let Some(pc) = queue.pop_front() {
        for &s in &succ[pc] {
            if s < succ.len() && !reachable[s] {
                reachable[s] = true;
                queue.push_back(s);
            }
        }
    }
    reachable
}

/// 可达节点的前驱表（不可达前驱不计入 must 交集）。
fn reachable_predecessors(succ: &[Vec<usize>], reachable: &[bool]) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); succ.len()];
    for (pc, ss) in succ.iter().enumerate() {
        if !reachable[pc] {
            continue;
        }
        for &s in ss {
            if s < succ.len() && reachable[s] {
                preds[s].push(pc);
            }
        }
    }
    preds
}

/// must 不动点：`input[n]` = 全体可达前驱 `out(p)` 的交集；
/// `input[0]` = ∅（入口之前无守卫）。
fn must_guard_inputs<'a>(
    code: &'a [IrStmt],
    preds: &[Vec<usize>],
    reachable: &[bool],
    buffered: &HashMap<usize, &'a str>,
    targets: &HashSet<&'a str>,
) -> Vec<HashSet<&'a str>> {
    let mut input: Vec<HashSet<&str>> = vec![targets.clone(); code.len()];
    input[0] = HashSet::new();
    loop {
        let mut changed = false;
        for n in 1..code.len() {
            if !reachable[n] || preds[n].is_empty() {
                continue;
            }
            let merged = merge_preds(code, &preds[n], &input, buffered, targets);
            if merged != input[n] {
                input[n] = merged;
                changed = true;
            }
        }
        if !changed {
            return input;
        }
    }
}

/// `n` 的新输入 = 全体前驱 `out(p)` 的交集；无可达前驱回退 top（不可达）。
fn merge_preds<'a>(
    code: &'a [IrStmt],
    pred_list: &[usize],
    input: &[HashSet<&'a str>],
    buffered: &HashMap<usize, &'a str>,
    targets: &HashSet<&'a str>,
) -> HashSet<&'a str> {
    let mut acc: Option<HashSet<&str>> = None;
    for &p in pred_list {
        let out = guarded_transfer(code, p, &input[p], buffered);
        acc = Some(match acc {
            None => out,
            Some(a) => a.intersection(&out).copied().collect(),
        });
    }
    acc.unwrap_or_else(|| targets.clone())
}

/// 节点 `pc` 的守卫转移：最终 `Buffered` 的候选 INSERT 把其目标表加入
/// 守卫集（不清别人）；`CallRoutine`/`SqlExecDynamic` 清空全部守卫；
/// `SqlQuery`/`FoldedQuery` 与非 `VALUES` 修改写只要触及守卫集里任意一张
/// 表即整刷清空（`flush_if_touches` 命中即刷整个缓冲）；其余透传。
fn guarded_transfer<'a>(
    code: &'a [IrStmt],
    pc: usize,
    inn: &HashSet<&'a str>,
    buffered: &HashMap<usize, &'a str>,
) -> HashSet<&'a str> {
    if let Some(t) = buffered.get(&pc) {
        let mut s = inn.clone();
        s.insert(*t);
        return s;
    }
    let flushes = match &code[pc] {
        IrStmt::CallRoutine { .. } | IrStmt::SqlExecDynamic { .. } => true,
        IrStmt::SqlQuery { sql, .. } | IrStmt::FoldedQuery { sql, .. } => {
            touches_any_guarded(sql, inn)
        }
        IrStmt::SqlExec { sql, .. }
            if plain_insert_values(sql).is_none() && modifies_table(sql) =>
        {
            touches_any_guarded(sql, inn)
        }
        _ => false,
    };
    if flushes {
        HashSet::new()
    } else {
        inn.clone()
    }
}

/// SQL 文本是否触及守卫集里任意一张表（整缓冲刷盘的触发条件）。
fn touches_any_guarded(sql: &str, inn: &HashSet<&str>) -> bool {
    inn.iter().any(|t| touches_table(sql, t))
}

/// 无守卫写判定：可达的非 `VALUES` 修改性 `SqlExec`，到达时守卫集与其
/// 触及表无交集 → 存在无守卫入口路径 → 跨表写耦合。
fn unguarded_write_pcs(
    code: &[IrStmt],
    reachable: &[bool],
    input: &[HashSet<&str>],
) -> HashSet<usize> {
    code.iter()
        .enumerate()
        .filter_map(|(pc, st)| match st {
            IrStmt::SqlExec { sql, .. }
                if reachable[pc] && plain_insert_values(sql).is_none() && modifies_table(sql) =>
            {
                let guarded = input[pc].iter().any(|t| touches_table(sql, t));
                (!guarded).then_some(pc)
            }
            _ => None,
        })
        .collect()
}
