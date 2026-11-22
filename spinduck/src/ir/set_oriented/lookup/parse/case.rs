//! 异构 CASE 写汇解析：`CASE $v_operand WHEN lit THEN $v_field ...
//! ELSE <字段|数值字面量> END`（§4.4 B1 / §3.3 sys_dummy 形态）。

use super::tokens::LiteralTokens;
use super::{BindSrc, CaseElse, CaseSink, Name, SkipReason, VarName};

/// 异构 CASE 投影：`CASE $v_operand WHEN lit THEN $v_rec__field ... ELSE
/// $v_rec__field|数值字面量 END`。操作数须为变量绑定（同组查找），
/// 分支须为游标字段绑定。
pub(super) fn parse_case_sink(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<CaseSink, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    t.take("case");
    let operand_var = match parse_case_bind(t, binds, record)? {
        CaseBind::Var(v) => v,
        CaseBind::Field(_) => return Err(bad),
    };
    let mut whens = Vec::new();
    while t.take("when") {
        let key = t.literal().ok_or(bad)?;
        if !t.take("then") {
            return Err(bad);
        }
        let field = match parse_case_bind(t, binds, record)? {
            CaseBind::Field(f) => f,
            CaseBind::Var(_) => return Err(bad),
        };
        whens.push((key, field));
    }
    if whens.is_empty() || !t.take("else") {
        return Err(bad);
    }
    let else_value = case_else(t, binds, record)?;
    if !t.take("end") {
        return Err(bad);
    }
    Ok(CaseSink {
        operand: operand_var,
        whens,
        else_value,
    })
}

enum CaseBind {
    Var(VarName),
    Field(Name),
}

/// CASE 内的绑定占位符：`$v_<var>`（变量）或 `$v_<rec>__<field>`（游标字段）。
fn parse_case_bind(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<CaseBind, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let ph = t.peek().ok_or(bad)?.to_string();
    if !ph.starts_with("$v_") {
        return Err(bad);
    }
    t.take(&ph);
    match binds.iter().find(|(p, _)| *p == ph).map(|(_, s)| s) {
        Some(BindSrc::Var(v)) => Ok(CaseBind::Var(v.clone())),
        Some(BindSrc::Field { rec, field }) if rec == record => {
            Ok(CaseBind::Field(Name(field.clone())))
        }
        _ => Err(bad),
    }
}

/// ELSE 分支：游标字段绑定或未加引号数值字面量（`ELSE 0`）；
/// 变量/字符串字面量会破坏 DECIMAL 公共类型证明，保守拒绝。
fn case_else(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<CaseElse, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    match parse_case_bind(t, binds, record) {
        Ok(CaseBind::Field(f)) => return Ok(CaseElse::Field(f)),
        Ok(CaseBind::Var(_)) => return Err(bad),
        Err(_) => {}
    }
    let lit = t.literal().ok_or(bad)?;
    if lit.starts_with('\'') || !lit.chars().any(|c| c.is_ascii_digit()) {
        return Err(bad);
    }
    Ok(CaseElse::Lit(lit))
}
