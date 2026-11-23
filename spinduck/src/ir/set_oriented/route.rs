//! IF 路由（设计 §4.3 区域入口循环不变探针）：条件 = 不变量兜底查找
//! 变量与常量等值；入口以三值判据 `(值 = 常量) IS NOT TRUE` 证明 THEN
//! 分支不可达后才选用 ELSE 集合化计划，否则无损退回原区域解释执行。

use super::lookup::{LookupQuery, ResolvedLookup};
use super::{BinOp, IrExpr, SkipReason, VarName};
use crate::value::SpinValue;

/// 条件识别：不变量查找变量 = 常量（Eq 左右皆可）；相关/Top-1 查找
/// 逐轮变化，不具备入口单点求值资格。
pub(super) fn cond(cond: &IrExpr, lookups: &[LookupQuery]) -> Option<(VarName, String)> {
    let (v, c) = eq_var_const(cond)?;
    lookups
        .iter()
        .find(|l| &l.var == v && l.correlations.is_empty() && l.top1.is_none())?;
    let lit = match c {
        SpinValue::Str(s) => format!("'{}'", s.replace('\'', "''")),
        SpinValue::Int(i) => i.to_string(),
        _ => return None,
    };
    Some((v.clone(), lit))
}

/// `变量 = 常量` 形态识别（Eq 左右皆可）。
pub(super) fn eq_var_const(cond: &IrExpr) -> Option<(&VarName, &SpinValue)> {
    let IrExpr::Bin {
        op: BinOp::Eq,
        l,
        r,
    } = cond
    else {
        return None;
    };
    match (l.as_ref(), r.as_ref()) {
        (IrExpr::Var(v), IrExpr::Const(c)) | (IrExpr::Const(c), IrExpr::Var(v)) => Some((v, c)),
        _ => None,
    }
}

/// 折叠常量与字面量的 PL 等值静态判定：Some(false) 含 NULL 比较
/// （结果为 NULL，IF 不取 THEN）；异类比较不可证 = None。
pub(super) fn static_cond(folded: &SpinValue, lit: &SpinValue) -> Option<bool> {
    match (folded, lit) {
        (SpinValue::Null, _) | (_, SpinValue::Null) => Some(false),
        (SpinValue::Int(a), SpinValue::Int(b)) => Some(a == b),
        (SpinValue::Str(a), SpinValue::Str(b)) => Some(a == b),
        _ => None,
    }
}

/// 入口守卫 SQL：查找值（命中 NULL 保留、0/多行落兜底）与常量等值
/// 的三值否定，期望 TRUE；守卫自身全定义不抛错。
pub(super) fn guard(l: &ResolvedLookup, lit: &str) -> Result<String, SkipReason> {
    if !l.render.key_checks.is_empty() {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    Ok(format!(
        "SELECT ((CASE WHEN {} THEN {} ELSE {} END) = {lit}) IS NOT TRUE",
        l.render.hit, l.render.value, l.render.default_sql
    ))
}

/// THEN 臂备选计划的路由判定 SQL：与 [`guard`] 同一值表达式，判定
/// "条件 IS TRUE"（THEN 可达）——分臂备选的入口按数据选臂，
/// NULL/不等值落 ELSE 计划（PL IF 语义：条件非 TRUE 即走 ELSE）。
pub(super) fn pick(l: &ResolvedLookup, lit: &str) -> Result<String, SkipReason> {
    if !l.render.key_checks.is_empty() {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    Ok(format!(
        "SELECT (((CASE WHEN {} THEN {} ELSE {} END) = {lit}) IS TRUE)",
        l.render.hit, l.render.value, l.render.default_sql
    ))
}
