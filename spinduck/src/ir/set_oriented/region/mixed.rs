//! 混合常量兜底 trap 结构面：单个 WHEN OTHERS 的保护区除连续 SELECT INTO
//! 外还可含任意其他语句（函数调用赋值、嵌套 trap 等），处理器体为与全部
//! 深度 0 查询 INTO 变量同集的常量赋值 + 续跑 Jump。集合化语义：保护区
//! 逐语句经既有证明后不可抛异常，处理器整体不可达，其常量赋值不参与投影；
//! 各 SELECT INTO 由 lookup 子系统按兜底常量解析为查找。

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt, TrapEntry, VarName};
use crate::value::SpinValue;

/// 深度 0 SELECT INTO 与其兜底常量的配对。
pub(crate) struct MixedQuery {
    pub pc: usize,
    pub var: VarName,
    pub default: SpinValue,
}

/// 形态识别结果：配对查询、保护区 PopTrap pc 与续跑 pc。
pub(crate) struct MixedFallback {
    pub queries: Vec<MixedQuery>,
    pub pop_pc: usize,
    pub resume_pc: usize,
}

/// 识别 pc 处 PushTrap 的混合常量兜底形态；形态不符 = None。
/// 全定义证明不在此裁决（由 lookup::mixed_fallback_group_at 经 parse_total）。
pub(crate) fn mixed_fallback_group(
    prog: &IrProg,
    pc: usize,
    entry: &TrapEntry,
) -> Option<MixedFallback> {
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return None;
    }
    let (queries, pop) = scan_queries(prog, pc)?;
    let join = entry.body_end + 1;
    if !matches!(prog.code.get(pop + 1), Some(IrStmt::Jump(t)) if *t == join)
        || !matches!(prog.code.get(entry.body_end), Some(IrStmt::Jump(t)) if *t == join)
    {
        return None;
    }
    let defaults = handler_defaults(prog, entry)?;
    pair_queries(queries, defaults, pop, join)
}

/// 深度扫描保护区：PushTrap/PopTrap 配对计深，深度 0 的 SqlQuery 单变量
/// INTO 记入查询列，深度 0 PopTrap 为保护区终点。
fn scan_queries(prog: &IrProg, pc: usize) -> Option<(Vec<(usize, VarName)>, usize)> {
    let mut queries = Vec::new();
    let mut depth = 0usize;
    for (i, stmt) in prog.code.iter().enumerate().skip(pc + 1) {
        match stmt {
            IrStmt::PushTrap { .. } => depth += 1,
            IrStmt::PopTrap => {
                if depth == 0 {
                    return Some((queries, i));
                }
                depth -= 1;
            }
            IrStmt::SqlQuery { into, fn_sites, .. } if depth == 0 => {
                let [AssignTarget::Var(v)] = into.as_slice() else {
                    return None;
                };
                if !fn_sites.is_empty() {
                    return None;
                }
                queries.push((i, v.clone()));
            }
            _ => {}
        }
    }
    None
}

/// 处理器体 = 全部 `Var := Const`；返回 变量 → 兜底常量映射。
fn handler_defaults(prog: &IrProg, entry: &TrapEntry) -> Option<BTreeMap<VarName, SpinValue>> {
    let mut defaults = BTreeMap::new();
    for bpc in entry.body_start..entry.body_end {
        let IrStmt::Assign {
            target: AssignTarget::Var(v),
            expr: IrExpr::Const(c),
        } = prog.code.get(bpc)?
        else {
            return None;
        };
        if defaults.insert(v.clone(), c.clone()).is_some() {
            return None;
        }
    }
    Some(defaults)
}

/// 查询变量集与兜底常量集必须一一对应（双射），否则异常路径的变量落点
/// 无法用常量刻画，保守拒绝。
fn pair_queries(
    queries: Vec<(usize, VarName)>,
    defaults: BTreeMap<VarName, SpinValue>,
    pop: usize,
    join: usize,
) -> Option<MixedFallback> {
    if queries.is_empty() {
        return None;
    }
    let qvars: BTreeSet<&VarName> = queries.iter().map(|(_, v)| v).collect();
    if qvars.len() != queries.len()
        || defaults.len() != qvars.len()
        || !qvars.iter().all(|v| defaults.contains_key(*v))
    {
        return None;
    }
    let paired = queries
        .into_iter()
        .map(|(qpc, var)| {
            let default = defaults.get(&var)?.clone();
            Some(MixedQuery {
                pc: qpc,
                var,
                default,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(MixedFallback {
        queries: paired,
        pop_pc: pop,
        resume_pc: join,
    })
}
