//! LookupTuple 识别（设计 §3.2）：摘要器只读 `RoutineCatalog` 已注册函数体。
//! 形态 = 前置纯赋值 + PushTrap(WHEN OTHERS) 包裹
//! `SqlQuery → 直线拷贝 → RETURN`，处理器体给每个 OUT 与返回值赋全定义
//! 常量且不 Raise；查询列/谓词/兜底全部来自函数体，不写死任何字面量。
//! 未注册名 → Ok(None)（交回 UnsupportedCall/UnresolvedFnSite）；
//! 形态不符（DML/循环/动态 SQL/未知调用）→ Ok(None)；形态符合但全定义
//! 证明失败（漏赋 OUT、Raise、可抛错拷贝）→ Err(MultiOutCallImpure)。

use super::super::lookup::{self, TupleQuery, TupleSlot, TupleSource};
use super::super::region::{self, CursorRegion};
use super::super::{BindSrc, IrExpr, IrProg, IrStmt, SkipReason, VarName};
use crate::catalog::{ParamMode, RoutineCatalog, RoutineKind};
use crate::ir::AssignTarget;
use crate::value::SpinValue;
use std::collections::BTreeMap;

/// 槽位来源：查询列下标或全路径常量。
#[derive(Clone)]
enum SlotSrc {
    Col(usize),
    Const(SpinValue),
}

/// 识别结果：Plan 可转换 / Impure 证明失败 / Plain 非本形态。
enum Shape {
    Plan(Plan),
    Impure,
    Plain,
}

/// 形态证明产物：函数体 SqlQuery 部件 + 拷贝追溯 + 处理器兜底。
struct Plan {
    sql: String,
    binds: Vec<(String, BindSrc)>,
    /// 局部变量名（小写）→ 槽位来源（INTO 目标与拷贝目标统一追溯）。
    env: BTreeMap<String, SlotSrc>,
    ret: SlotSrc,
    /// OUT 形参名（小写）→ 处理器兜底常量。
    out_defaults: BTreeMap<String, SpinValue>,
    ret_default: SpinValue,
}

/// 调用点上下文：实参、返回值写回槽与调用方程序/区域/游标记录。
pub(crate) struct CallSite<'a> {
    pub args: &'a [IrExpr],
    pub ret_var: &'a VarName,
    pub prog: &'a IrProg,
    pub region: &'a CursorRegion,
    pub record: &'a VarName,
}

/// 调用点入口：返回 Some(TupleQuery) 表示可按 LookupTuple 集合化。
pub(crate) fn tuple_query(
    catalog: &RoutineCatalog,
    name: &str,
    site: &CallSite<'_>,
) -> Result<Option<TupleQuery>, SkipReason> {
    let Some(r) = catalog.lookup(name) else {
        return Ok(None);
    };
    if !matches!(r.kind, RoutineKind::Function)
        || r.params
            .iter()
            .any(|p| !matches!(p.mode, ParamMode::In | ParamMode::Out))
        || site.args.len() != r.params.len()
        || !site
            .args
            .iter()
            .all(|a| matches!(a, IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. }))
    {
        return Ok(None);
    }
    for (p, a) in r.params.iter().zip(site.args) {
        if matches!(p.mode, ParamMode::Out) && !matches!(a, IrExpr::Var(_)) {
            return Ok(None);
        }
    }
    let plan = match recognize(r) {
        Shape::Plan(p) => p,
        Shape::Impure => return Err(SkipReason::MultiOutCallImpure),
        Shape::Plain => return Ok(None),
    };
    build(catalog, r, &plan, site)
}

/// 结构识别：恰一个 WHEN OTHERS PushTrap；前缀仅 Nop/常量赋值；
/// 成功路径 = SqlQuery → 直线 Assign → Return → PopTrap → Jump；
/// 处理器与尾部仅允许 Nop/Jump/Return/常量赋值。
/// 头部形态证据：trap 入口 + PushTrap 下标 + SqlQuery 部件引用。
struct Head<'c> {
    entry: &'c crate::ir::TrapEntry,
    push: usize,
    sql: &'c str,
    binds: &'c [(String, BindSrc)],
    into: &'c [AssignTarget],
}

/// 头部形态：恰一个 WHEN OTHERS PushTrap；前缀仅 Nop/常量赋值；
/// trap 紧随其后是单条无 fn_sites 的 SqlQuery，INTO 全为变量。
fn head<'c>(code: &'c [IrStmt]) -> Option<Head<'c>> {
    let traps: Vec<usize> = code
        .iter()
        .enumerate()
        .filter_map(|(i, s)| matches!(s, IrStmt::PushTrap { .. }).then_some(i))
        .collect();
    let [push] = traps.as_slice() else {
        return None;
    };
    let IrStmt::PushTrap { entries } = &code[*push] else {
        return None;
    };
    let [entry] = entries.as_slice() else {
        return None;
    };
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return None;
    }
    if code[..*push].iter().any(|s| !pure_prefix_stmt(s)) {
        return None;
    }
    let IrStmt::SqlQuery {
        sql,
        binds,
        into,
        fn_sites,
    } = &code[*push + 1]
    else {
        return None;
    };
    if !fn_sites.is_empty()
        || into.is_empty()
        || !into.iter().all(|t| matches!(t, AssignTarget::Var(_)))
    {
        return None;
    }
    Some(Head {
        entry,
        push: *push,
        sql,
        binds,
        into,
    })
}

fn pure_prefix_stmt(s: &IrStmt) -> bool {
    matches!(
        s,
        IrStmt::Nop
            | IrStmt::Assign {
                target: AssignTarget::Var(_),
                expr: IrExpr::Const(_)
            }
    )
}

/// 成功路径尾段：直线 Assign → Return → PopTrap → Jump；
/// trap 外尾部仅允许 Nop/Jump/Return/常量赋值。
fn success_tail(
    code: &[IrStmt],
    entry: &crate::ir::TrapEntry,
    push: usize,
) -> Option<(Vec<(VarName, IrExpr)>, IrExpr)> {
    let mut i = push + 2;
    let mut copies = Vec::new();
    while let Some(IrStmt::Assign {
        target: AssignTarget::Var(v),
        expr,
    }) = code.get(i)
    {
        copies.push((v.clone(), expr.clone()));
        i += 1;
    }
    let Some(IrStmt::Return {
        value: Some(ret_expr),
    }) = code.get(i)
    else {
        return None;
    };
    let ret_expr = ret_expr.clone();
    i += 1;
    while matches!(code.get(i), Some(IrStmt::Nop)) {
        i += 1;
    }
    if !matches!(code.get(i), Some(IrStmt::PopTrap)) {
        return None;
    }
    i += 1;
    while matches!(code.get(i), Some(IrStmt::Nop)) {
        i += 1;
    }
    if !matches!(code.get(i), Some(IrStmt::Jump(_))) {
        return None;
    }
    let mid = entry.body_start.saturating_sub(i + 1);
    for stmt in code
        .iter()
        .skip(i + 1)
        .take(mid)
        .chain(&code[entry.body_end..])
    {
        if !matches!(
            stmt,
            IrStmt::Nop
                | IrStmt::Jump(_)
                | IrStmt::Return { .. }
                | IrStmt::Assign {
                    target: AssignTarget::Var(_),
                    expr: IrExpr::Const(_)
                }
        ) {
            return None;
        }
    }
    Some((copies, ret_expr))
}

fn recognize(r: &crate::catalog::CompiledRoutine) -> Shape {
    let code = &r.prog.code;
    let Some(h) = head(code) else {
        return Shape::Plain;
    };
    let Some((copies, ret_expr)) = success_tail(code, h.entry, h.push) else {
        return Shape::Plain;
    };
    finish_shape(code, h.entry, h.sql, h.binds, h.into, copies, ret_expr)
}

/// 拷贝追溯 + 处理器兜底汇总；任一环节不可证 → Impure。
fn finish_shape(
    code: &[IrStmt],
    entry: &crate::ir::TrapEntry,
    sql: &str,
    binds: &[(String, BindSrc)],
    into: &[AssignTarget],
    copies: Vec<(VarName, IrExpr)>,
    ret_expr: IrExpr,
) -> Shape {
    let mut env = BTreeMap::new();
    for (idx, t) in into.iter().enumerate() {
        let AssignTarget::Var(v) = t else {
            return Shape::Plain;
        };
        env.insert(v.0.to_ascii_lowercase(), SlotSrc::Col(idx));
    }
    for (v, expr) in &copies {
        let src = match expr {
            IrExpr::Var(x) => match env.get(&x.0.to_ascii_lowercase()) {
                Some(s) => s.clone(),
                None => return Shape::Impure,
            },
            IrExpr::Const(c) => SlotSrc::Const(c.clone()),
            _ => return Shape::Impure,
        };
        env.insert(v.0.to_ascii_lowercase(), src);
    }
    let ret = match &ret_expr {
        IrExpr::Var(x) => match env.get(&x.0.to_ascii_lowercase()) {
            Some(s) => s.clone(),
            None => return Shape::Impure,
        },
        IrExpr::Const(c) => SlotSrc::Const(c.clone()),
        _ => return Shape::Impure,
    };
    let Some((out_defaults, ret_default)) = handler_defaults(code, entry) else {
        return Shape::Impure;
    };
    Shape::Plan(Plan {
        sql: sql.to_string(),
        binds: binds.to_vec(),
        env,
        ret,
        out_defaults,
        ret_default,
    })
}

/// 处理器体扫描：仅允许 Nop/常量赋值/恰好一次 Return；Raise 或其它
/// 语句形态 → None（调用方按 Impure 拒绝）。返回 (OUT 兜底表, 返回值兜底)。
fn handler_defaults(
    code: &[IrStmt],
    entry: &crate::ir::TrapEntry,
) -> Option<(BTreeMap<String, SpinValue>, SpinValue)> {
    let mut out_defaults = BTreeMap::new();
    let mut ret_default = None;
    let mut returns = 0;
    for stmt in &code[entry.body_start..entry.body_end] {
        match stmt {
            IrStmt::Nop => {}
            IrStmt::Assign {
                target: AssignTarget::Var(v),
                expr: IrExpr::Const(c),
            } => {
                out_defaults.insert(v.0.to_ascii_lowercase(), c.clone());
            }
            IrStmt::Assign { .. } => return None,
            IrStmt::Return { value } => {
                returns += 1;
                ret_default = match value {
                    Some(IrExpr::Const(c)) => Some(c.clone()),
                    None => Some(SpinValue::Null),
                    _ => return None,
                };
            }
            IrStmt::Raise { .. } => return None,
            _ => return None,
        }
    }
    if returns != 1 {
        return None;
    }
    Some((out_defaults, ret_default.unwrap_or(SpinValue::Null)))
}

/// 调用方装配：形参绑定映射 + 全定义文法解析 + 槽位绑定。
fn build(
    catalog: &RoutineCatalog,
    r: &crate::catalog::CompiledRoutine,
    plan: &Plan,
    site: &CallSite<'_>,
) -> Result<Option<TupleQuery>, SkipReason> {
    let caller_binds = match caller_binds(r, &plan.binds, site) {
        Some(b) => b,
        None => return Ok(None),
    };
    let parts = match lookup::parse_tuple_total(&plan.sql, &caller_binds, site.record) {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if !parts.in_filters.is_empty() || !parts.in_lists.is_empty() || parts.correlations.is_empty() {
        return Ok(None);
    }
    let writes = region::write_set(site.prog, site.region, catalog)?;
    if writes.contains(&parts.table.0) {
        return Err(SkipReason::TargetReadDependency);
    }
    let Some(slots) = out_slots(r, plan, site)? else {
        return Ok(None);
    };
    Ok(Some(TupleQuery {
        table: parts.table,
        columns: parts.columns,
        filter: parts.filter,
        correlations: parts.correlations,
        filter_binds: parts.binds,
        slots,
    }))
}

/// OUT 形参 + 返回值 → 槽位：漏赋 OUT 或兜底缺失 → MultiOutCallImpure；
/// OUT 实参非变量 → Ok(None)（形态不符，交回其它路径）。
fn out_slots(
    r: &crate::catalog::CompiledRoutine,
    plan: &Plan,
    site: &CallSite<'_>,
) -> Result<Option<Vec<TupleSlot>>, SkipReason> {
    let mut slots = Vec::new();
    for (p, arg) in r.params.iter().zip(site.args) {
        if !matches!(p.mode, ParamMode::Out) {
            continue;
        }
        let IrExpr::Var(v) = arg else {
            return Ok(None);
        };
        let key = p.name.to_ascii_lowercase();
        let Some(source) = plan.env.get(&key).cloned() else {
            return Err(SkipReason::MultiOutCallImpure);
        };
        let Some(default) = plan.out_defaults.get(&key).cloned() else {
            return Err(SkipReason::MultiOutCallImpure);
        };
        slots.push(TupleSlot {
            var: v.clone(),
            source: to_source(source),
            default,
        });
    }
    slots.push(TupleSlot {
        var: site.ret_var.clone(),
        source: to_source(plan.ret.clone()),
        default: plan.ret_default.clone(),
    });
    Ok(Some(slots))
}

/// 函数体 `$v_<形参>` 绑定 → 调用方 BindSrc（Var 实参 / 记录字段实参）。
fn caller_binds(
    r: &crate::catalog::CompiledRoutine,
    binds: &[(String, BindSrc)],
    site: &CallSite<'_>,
) -> Option<Vec<(String, BindSrc)>> {
    let mut out = Vec::with_capacity(binds.len());
    for (ph, src) in binds {
        let BindSrc::Var(pv) = src else {
            return None;
        };
        let idx = r
            .params
            .iter()
            .position(|p| p.name.eq_ignore_ascii_case(&pv.0))?;
        let bsrc = match &site.args[idx] {
            IrExpr::Var(v) => BindSrc::Var(v.clone()),
            IrExpr::Field { rec, field } if rec == site.record => BindSrc::Field {
                rec: rec.clone(),
                field: field.clone(),
            },
            _ => return None,
        };
        out.push((ph.clone(), bsrc));
    }
    Some(out)
}

fn to_source(src: SlotSrc) -> TupleSource {
    match src {
        SlotSrc::Col(i) => TupleSource::Col(i),
        SlotSrc::Const(v) => TupleSource::Const(v),
    }
}
