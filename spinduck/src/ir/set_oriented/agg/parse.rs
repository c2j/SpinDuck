//! 聚合查表 SQL 解析：SELECT/FROM/WHERE 顶层切分与合取分类
//! （关联键 / 静态过滤 / 游标字段门）。

use super::super::shape::Name;
use super::super::{BindSrc, SkipReason, VarName};
use super::{AggCursor, AggKey, AggQuery};
use crate::value::SpinValue;

// ---------- SQL 解析 ----------

/// 顶层切分（引号/括号感知；`''` 转义）。
fn split_top<'a>(s: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_quote(bytes, i),
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && s[i..].to_ascii_uppercase().starts_with(needle) => {
                return Some((&s[..i], &s[i + needle.len()..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_quote(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i;
        }
        i += 1;
    }
    i
}

/// 顶层关键字切分（大小写不敏感、词边界、空白不限；引号/括号感知）。
fn split_kw<'a>(s: &'a str, kw: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_quote(bytes, i),
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0
                && is_word_byte(bytes[i])
                && s[i..].len() >= kw.len()
                && s[i..i + kw.len()].eq_ignore_ascii_case(kw)
                && !is_word_byte(bytes.get(i + kw.len()).copied().unwrap_or(b' '))
                && (i == 0 || !is_word_byte(bytes[i - 1])) =>
            {
                return Some((&s[..i], &s[i + kw.len()..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// WHERE 顶层 AND 切分（词边界）。
fn split_ands(where_sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = where_sql.as_bytes();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_quote(bytes, i),
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0
                && is_word_byte(bytes[i])
                && where_sql[i..].len() >= 3
                && where_sql[i..i + 3].eq_ignore_ascii_case("and")
                && !is_word_byte(bytes.get(i + 3).copied().unwrap_or(b' '))
                && (i == 0 || !is_word_byte(bytes[i - 1])) =>
            {
                out.push(where_sql[start..i].trim().to_string());
                i += 3;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(where_sql[start..].trim().to_string());
    out.retain(|s| !s.is_empty());
    out
}

/// 逗号顶层切分（先试 ", " 再试 ","）。
fn split_comma(s: &str) -> Option<(&str, &str)> {
    split_top(s, ", ").or_else(|| split_top(s, ","))
}

pub(in crate::ir::set_oriented) fn parse_agg(
    sql: &str,
    binds: &[(String, BindSrc)],
    record: &VarName,
    var: &VarName,
    handler_default: &SpinValue,
) -> Result<Option<AggQuery>, SkipReason> {
    let unproven = || SkipReason::UnprovenQueryTotalness;
    let Some((select_part, rest)) = split_kw(sql, "from") else {
        return Ok(None);
    };
    // select_part 含 `SELECT ` 前缀；聚合形态判定只看投影表达式。
    let Some((_, select_expr)) = split_kw(select_part, "select") else {
        return Ok(None);
    };
    let select_part = select_expr;
    let (from_part, where_part) = match split_kw(rest, "where") {
        Some((f, w)) => (f.trim(), w.trim()),
        None => (rest.trim(), ""),
    };
    let Ok((value_col, inner_default, outer_default)) = parse_select(select_part.trim()) else {
        // SELECT 非聚合形态 → 不是本特性，交回兜底组识别。
        return Ok(None);
    };
    let (table, alias) = parse_from(from_part)?;
    let mut acc = ConjAcc::default();
    if !where_part.is_empty() {
        for conj in split_ands(where_part) {
            classify_conjunct(&conj, &alias, binds, record, &mut acc)?;
        }
    }
    if acc.keys.is_empty() {
        return Err(unproven());
    }
    Ok(Some(AggQuery {
        var: var.clone(),
        table,
        alias,
        keys: acc.keys,
        value_col,
        inner_default,
        outer_default,
        static_filter: acc.static_parts.join(" AND "),
        static_binds: acc.static_binds,
        gate_preds: acc.gate_preds,
        cursor_fields: acc.cursor_fields,
        handler_default: handler_default.clone(),
    }))
}

#[derive(Default)]
struct ConjAcc {
    keys: Vec<AggKey>,
    seen_dims: Vec<Name>,
    static_parts: Vec<String>,
    static_binds: Vec<(String, BindSrc)>,
    gate_preds: Vec<String>,
    cursor_fields: Vec<Name>,
}

impl ConjAcc {
    fn push_field(&mut self, f: Name) {
        if !self.cursor_fields.contains(&f) {
            self.cursor_fields.push(f);
        }
    }
}

fn parse_from(from_part: &str) -> Result<(Name, String), SkipReason> {
    let unproven = || SkipReason::UnprovenQueryTotalness;
    let tokens: Vec<&str> = from_part.split_whitespace().collect();
    if tokens.is_empty() || tokens.iter().any(|t| matches!(*t, "," | "(" | ")")) {
        return Err(unproven());
    }
    let table = Name(tokens[0].to_string());
    let alias = match tokens.as_slice() {
        [_] => tokens[0].to_string(),
        [_, "AS", a] => (*a).to_string(),
        [_, a] if !a.eq_ignore_ascii_case("AS") => (*a).to_string(),
        _ => return Err(unproven()),
    };
    Ok((table, alias))
}

/// `coalesce(sum(coalesce(<alias>.<col>, <d0>)), <d1>)`。
fn parse_select(select: &str) -> Result<(Name, String, String), SkipReason> {
    let unproven = || SkipReason::UnprovenQueryTotalness;
    let inner = select
        .strip_prefix("coalesce(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(unproven)?;
    let (sum_part, outer_default) = split_comma(inner).ok_or_else(unproven)?;
    let sum_inner = sum_part
        .trim()
        .strip_prefix("sum(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(unproven)?;
    let sum_inner = sum_inner.trim();
    let (col_ref, inner_default) = if let Some(rest) = sum_inner
        .strip_prefix("coalesce(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let (c, d) = split_comma(rest).ok_or_else(unproven)?;
        (c.trim(), d.trim().to_string())
    } else {
        (sum_inner, "NULL".to_string())
    };
    let col = col_ref.rsplit('.').next().ok_or_else(unproven)?.trim();
    if col.is_empty() || !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(unproven());
    }
    if !outer_default
        .trim()
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.')
    {
        return Err(unproven());
    }
    Ok((
        Name(col.to_string()),
        inner_default,
        outer_default.trim().to_string(),
    ))
}

fn classify_conjunct(
    conj: &str,
    alias: &str,
    binds: &[(String, BindSrc)],
    record: &VarName,
    acc: &mut ConjAcc,
) -> Result<(), SkipReason> {
    let unproven = || SkipReason::UnprovenQueryTotalness;
    let Some(eq) = top_level_eq(conj) else {
        return Err(unproven());
    };
    let (lhs, rhs) = (conj[..eq].trim(), conj[eq + 1..].trim());
    // 门谓词：$v_r__<f> = <字面量>（游标字段，不进 CTE）。
    if let Some(field) = record_field(lhs, binds, record) {
        if is_literal(rhs) {
            acc.gate_preds
                .push(format!("sd_cur.{} = {}", field.sql(), rhs));
            acc.push_field(field);
            return Ok(());
        }
        return Err(unproven());
    }
    if let Some(dim) = col_name(lhs, alias) {
        // 关联键：维表列 = 游标字段 / CASE 模板。
        if let Ok(cursor) = cursor_side(rhs, binds, record, &mut acc.cursor_fields) {
            if acc.seen_dims.contains(&dim) {
                return Err(unproven());
            }
            acc.seen_dims.push(dim.clone());
            acc.keys.push(AggKey { dim, cursor });
            return Ok(());
        }
        // 静态过滤：维表列 = $v_<var> / 字面量。
        if is_static_rhs(rhs, binds) {
            push_static(conj, binds, acc)?;
            return Ok(());
        }
        return Err(unproven());
    }
    // 反向静态过滤：$v_<var> = <col>。
    if col_name(rhs, alias).is_some() && is_static_rhs(lhs, binds) {
        push_static(conj, binds, acc)?;
        return Ok(());
    }
    Err(unproven())
}

fn push_static(
    conj: &str,
    binds: &[(String, BindSrc)],
    acc: &mut ConjAcc,
) -> Result<(), SkipReason> {
    let unproven = || SkipReason::UnprovenQueryTotalness;
    for ph in placeholders(conj) {
        let Some((_, src)) = binds.iter().find(|(n, _)| *n == ph) else {
            return Err(unproven());
        };
        if matches!(src, BindSrc::Field { .. }) {
            return Err(unproven());
        }
        if !acc.static_binds.iter().any(|(n, _)| *n == ph) {
            acc.static_binds.push((ph, src.clone()));
        }
    }
    acc.static_parts.push(conj.to_string());
    Ok(())
}

/// 顶层 `=` 位置（引号/括号感知；`<=` `>=` `<>` `!=` `==` 不算）。
fn top_level_eq(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_quote(bytes, i),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                let next = bytes.get(i + 1).copied();
                if matches!(prev, Some(b'<' | b'>' | b'!')) || next == Some(b'>') {
                    // <= >= <> != => 均非等值
                } else if next == Some(b'=') {
                    i += 1;
                } else {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `<alias>.<col>` 或裸 `<col>` → 列名；否则 None。
fn col_name(side: &str, alias: &str) -> Option<Name> {
    let side = side.trim();
    let col = if let Some(rest) = side.strip_prefix(&format!("{alias}.")) {
        rest
    } else if side.contains('.') {
        return None;
    } else {
        side
    };
    if col.is_empty() || !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(Name(col.to_string()))
}

/// `$v_r__<f>` 精确占位符 → 记录字段名（经 binds 权威映射）。
fn record_field(side: &str, binds: &[(String, BindSrc)], record: &VarName) -> Option<Name> {
    let (_, src) = binds.iter().find(|(n, _)| n.as_str() == side.trim())?;
    match src {
        BindSrc::Field { rec, field } if rec == record => Some(Name(field.clone())),
        _ => None,
    }
}

/// 游标侧 RHS：`$v_r__<f>` → Field；`CASE ... END` → Case（占位符全为记录字段）。
fn cursor_side(
    rhs: &str,
    binds: &[(String, BindSrc)],
    record: &VarName,
    fields: &mut Vec<Name>,
) -> Result<AggCursor, SkipReason> {
    let unproven = || SkipReason::UnprovenQueryTotalness;
    if let Some(field) = record_field(rhs, binds, record) {
        fields.push(field.clone());
        return Ok(AggCursor::Field(field));
    }
    if rhs.starts_with("CASE ") && rhs.ends_with(" END") {
        let mut out = String::with_capacity(rhs.len());
        let mut rest = rhs;
        while let Some(pos) = rest.find("$v_") {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 3..];
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            let ph = format!("$v_{}", &after[..end]);
            let Some((_, src)) = binds.iter().find(|(n, _)| *n == ph) else {
                return Err(unproven());
            };
            match src {
                BindSrc::Field { rec, field } if rec == record => {
                    fields.push(Name(field.clone()));
                    out.push_str(&format!("sd_cur.{}", Name(field.clone()).sql()));
                }
                _ => return Err(unproven()),
            }
            rest = &after[end..];
        }
        out.push_str(rest);
        return Ok(AggCursor::Case(out));
    }
    Err(unproven())
}

fn is_static_rhs(rhs: &str, binds: &[(String, BindSrc)]) -> bool {
    let rhs = rhs.trim();
    if is_literal(rhs) {
        return true;
    }
    binds
        .iter()
        .any(|(n, src)| n.as_str() == rhs && matches!(src, BindSrc::Var(_)))
}

fn is_literal(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
        || s.parse::<f64>().is_ok()
        || s.eq_ignore_ascii_case("null")
}

/// 提取片段中全部 `$v_<name>` 占位符。
fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(pos) = rest.find("$v_") {
        let after = &rest[pos + 3..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        out.push(format!("$v_{}", &after[..end]));
        rest = &after[end..];
    }
    out
}
