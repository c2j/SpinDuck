//! IN 谓词解析（§4.4 B1）：IN 子查询、游标字段 IN 值列表与 concat/substring
//! 表达式项。自 parse.rs 拆分（AGENTS §3 规模约束；行为一字不改）。

use super::super::{BindSrc, Correlation, InFilter, InList, InListItem, Name, SkipReason, VarName};
use super::tokens::LiteralTokens;

/// IN 子查询：单表投影 + 静态字面量/形参 BETWEEN/相关键等值谓词
/// （禁止嵌套子查询）。相关键非空时渲染须 LATERAL（sd_cur 在作用域内）。
pub(super) fn parse_in_filter(
    t: &mut LiteralTokens,
    column: Name,
    binds: &[(String, BindSrc)],
    record: &VarName,
    filter_binds: &mut Vec<(String, BindSrc)>,
) -> Result<InFilter, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("(") || !t.take("select") {
        return Err(bad);
    }
    let inner_column = t.column().ok_or(bad)?;
    if !t.take("from") {
        return Err(bad);
    }
    let table = t.name().ok_or(bad)?;
    t.skip_alias();
    let mut ctx = InCtx {
        binds,
        record,
        filter_binds,
        parts: Vec::new(),
        correlations: Vec::new(),
    };
    if t.take("where") {
        loop {
            in_where_pred(t, &mut ctx)?;
            if !t.take("and") {
                break;
            }
        }
    }
    if !t.take(")") {
        return Err(bad);
    }
    let filter = if ctx.parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", ctx.parts.join(" AND "))
    };
    Ok(InFilter {
        column,
        table,
        inner_column,
        filter,
        correlations: ctx.correlations,
    })
}

/// IN 子查询 WHERE 解析上下文（聚合绑定/记录/输出，避免超参）。
struct InCtx<'a> {
    binds: &'a [(String, BindSrc)],
    record: &'a VarName,
    filter_binds: &'a mut Vec<(String, BindSrc)>,
    parts: Vec<String>,
    correlations: Vec<Correlation>,
}

/// 单个内层谓词（不含尾随 AND 分隔）：形参 BETWEEN 两列、相关键等值
/// （Field 绑定）或静态字面量比较。BETWEEN 绑定并入外层 filter_binds。
fn in_where_pred(t: &mut LiteralTokens, ctx: &mut InCtx) -> Result<(), SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if t.peek().is_some_and(|w| w.starts_with("$v_"))
        && t.words.get(t.pos + 1).is_some_and(|w| w == "between")
    {
        let ph = t.peek().ok_or(bad)?.to_string();
        let Some((_, src)) = ctx.binds.iter().find(|(p, _)| *p == ph) else {
            return Err(bad);
        };
        let BindSrc::Var(_) = src else {
            return Err(bad);
        };
        t.take(&ph);
        t.take("between");
        let lo = t.column().ok_or(bad)?;
        if !t.take("and") {
            return Err(bad);
        }
        let hi = t.column().ok_or(bad)?;
        ctx.filter_binds.push((ph.clone(), src.clone()));
        ctx.parts
            .push(format!("{ph} BETWEEN {} AND {}", lo.sql(), hi.sql()));
        return Ok(());
    }
    let col = t.column().ok_or(bad)?;
    let op = t.peek().ok_or(bad)?.to_string();
    if !matches!(op.as_str(), "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
        return Err(bad);
    }
    t.take(&op);
    if op == "=" && t.peek().is_some_and(|w| w.starts_with("$v_")) {
        let word = t.peek().ok_or(bad)?.to_string();
        t.take(&word);
        let Some((_, BindSrc::Field { rec, field })) = ctx.binds.iter().find(|(p, _)| *p == word)
        else {
            return Err(bad);
        };
        if rec != ctx.record {
            return Err(bad);
        }
        ctx.correlations.push(Correlation {
            dim_column: col,
            cursor_field: Name(field.clone()),
            op: super::CorrelationOp::Eq,
        });
    } else {
        let lit = t.literal().ok_or(bad)?;
        ctx.parts.push(format!("{} {op} {lit}", col.sql()));
    }
    Ok(())
}

/// IN 值列表：`(项, ...)`，项为游标字段绑定（`$v_<rec>__<field>`）、
/// 字面量，或 concat/substring 字符串表达式（基字段须与维表列同 varchar）；
/// 至少一项。其余函数/表达式形态保守拒绝。
pub(super) fn parse_in_list(
    t: &mut LiteralTokens,
    column: Name,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<InList, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("(") {
        return Err(bad);
    }
    let mut items = Vec::new();
    loop {
        let w = t.peek().ok_or(bad)?.to_string();
        if w.starts_with("$v_") {
            t.take(&w);
            let Some((_, BindSrc::Field { rec, field })) = binds.iter().find(|(p, _)| *p == w)
            else {
                return Err(bad);
            };
            if rec != record {
                return Err(bad);
            }
            items.push(InListItem::Field(Name(field.clone())));
        } else if w == "concat" {
            let (sql, base_field) = parse_in_list_expr(t, binds, record)?;
            items.push(InListItem::Expr { sql, base_field });
        } else if let Some(lit) = t.literal() {
            items.push(InListItem::Lit(lit));
        } else {
            return Err(bad);
        }
        if !t.take(",") {
            break;
        }
    }
    if !t.take(")") || items.is_empty() {
        return Err(bad);
    }
    Ok(InList { column, items })
}

/// IN-list 表达式项：IR 把 `substr(字段,1,2)||'00'` 规范化为
/// `concat(substring($v_字段,1,2),'00')`（语料 stock_kind IN 形态）。仅接受
/// concat 串联 substring(游标字段,整,整) 与字符串字面量；逐字保留 concat/
/// substring 规范形（解释器与批写同经 DuckDB 求值，语义按构造一致），仅把
/// 字段绑定代入 sd_cur.字段。substring/concat 对 varchar 全定义不抛错且
/// 结果 varchar；渲染期再校验维表列亦 varchar。返回 SQL 与基字段（供
/// key_checks 类型对齐）；多基字段保守拒绝。
fn parse_in_list_expr(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<(String, Name), SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("concat") || !t.take("(") {
        return Err(bad);
    }
    let mut base: Option<Name> = None;
    let mut parts: Vec<String> = Vec::new();
    loop {
        if t.take("substring") {
            parts.push(parse_substring_item(t, binds, record, &mut base)?);
        } else {
            let lit = t.literal().ok_or(bad)?;
            if !lit.starts_with('\'') {
                return Err(bad);
            }
            parts.push(lit);
        }
        if !t.take(",") {
            break;
        }
    }
    if !t.take(")") {
        return Err(bad);
    }
    Ok((format!("concat({})", parts.join(", ")), base.ok_or(bad)?))
}

fn parse_substring_item(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
    base: &mut Option<Name>,
) -> Result<String, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("(") {
        return Err(bad);
    }
    let w = t.peek().ok_or(bad)?.to_string();
    if !w.starts_with("$v_") {
        return Err(bad);
    }
    t.take(&w);
    let Some((_, BindSrc::Field { rec, field })) = binds.iter().find(|(p, _)| *p == w) else {
        return Err(bad);
    };
    if rec != record {
        return Err(bad);
    }
    let f = Name(field.clone());
    if base.as_ref().is_some_and(|b| b != &f) {
        return Err(bad);
    }
    *base = Some(f.clone());
    if !t.take(",") {
        return Err(bad);
    }
    let a = t.literal().ok_or(bad)?;
    if !a.chars().all(|c| c.is_ascii_digit()) || !t.take(",") {
        return Err(bad);
    }
    let b = t.literal().ok_or(bad)?;
    if !b.chars().all(|c| c.is_ascii_digit()) || !t.take(")") {
        return Err(bad);
    }
    Ok(format!("substring(sd_cur.{}, {a}, {b})", f.sql()))
}
