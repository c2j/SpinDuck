//! verbatim 游标投影项的键源列证明（issue #65 R2）。
//!
//! `select_verbatim` 形态下游标字段与源列的对应关系只能从投影 SQL
//! 文本逐项复核：仅裸列/限定列（可带 `AS`）可证源列 kind；表达式
//! 投影（如 `security_id + 0.0`）输出类型不可由物理源列证明，整站
//! 禁用缓存。

use super::super::shape::{self, Name, Select};
use super::site::is_ident;

/// 游标键字段 → 源列名：普通游标用 `source_of` 输出→源映射；
/// verbatim 游标必须逐项复核投影——仅裸/限定列（可带 `AS`）可证，
/// 表达式键返回 `None`（R2：输出类型不可由物理源列 kind 证明）。
pub(super) fn cursor_key_source(cursor: &Select, field: &Name) -> Option<Name> {
    match &cursor.verbatim {
        None => cursor.source_of(field).cloned(),
        Some(raw) => verbatim_key_source(raw, cursor, field),
    }
}

/// verbatim 游标 SQL 中定位 `field` 对应投影项：返回其源列名；
/// 项为表达式（无裸列形态）→ `None`。
fn verbatim_key_source(sql: &str, cursor: &Select, field: &Name) -> Option<Name> {
    let t = shape::Tokens::new(sql)?;
    let w = t.words();
    // 顶层 FROM 位置（深度 0），投影项 = SELECT 与 FROM 间顶层逗号切分。
    let mut depth = 0i32;
    let mut from_at = None;
    for (i, tok) in w.iter().enumerate().skip(1) {
        match tok.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "from" if depth == 0 => {
                from_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    verbatim_key_source_depth(w, cursor, field, from_at?)
}

/// 深度感知的投影项切分与字段匹配。
fn verbatim_key_source_depth(
    w: &[String],
    cursor: &Select,
    field: &Name,
    end: usize,
) -> Option<Name> {
    let mut depth = 0i32;
    let mut start = 1;
    for i in 1..=end {
        match w[i].as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "," if depth == 0 => {
                if let Some(hit) = verbatim_item(&w[start..i], cursor, field) {
                    return hit;
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    verbatim_item(&w[start..end], cursor, field).flatten()
}

/// 单个投影项：若输出名 == `field`，返回 `Some(源列)`（表达式 →
/// `Some(None)` 由调用方转禁用）或 `None`（非本字段）。
fn verbatim_item(item: &[String], cursor: &Select, field: &Name) -> Option<Option<Name>> {
    // 顶层 `AS` 切分。
    let mut depth = 0i32;
    let mut as_at = None;
    for (i, tok) in item.iter().enumerate() {
        match tok.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "as" if depth == 0 => {
                as_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let (expr_part, out_name) = match as_at {
        Some(k) => {
            let out = ident_of(item.get(k + 1)?)?;
            (&item[..k], out)
        }
        None => {
            // 无 AS：仅 `[q.]col` 单形态可定名（verbatim_out_name 同口径）。
            let col = bare_column(item, cursor)?;
            return if col == *field { Some(Some(col)) } else { None };
        }
    };
    if out_name != *field {
        return None;
    }
    // 表达式部分仅当整体为裸/限定列时可证源列。
    Some(bare_column(expr_part, cursor))
}

/// 裸列形态 `[c]` / `[q,'.',c]` → 列名；限定符必须是游标表名或别名。
fn bare_column(item: &[String], cursor: &Select) -> Option<Name> {
    let col = match item {
        [c] => ident_of(c)?,
        [q, d, c] if d == "." => {
            let ok = *q == cursor.table.0 || cursor.alias.as_ref().is_some_and(|a| a.0 == *q);
            if !ok {
                return None;
            }
            ident_of(c)?
        }
        _ => return None,
    };
    Some(col)
}

/// 裸标识符 → `Name`（与 `is_ident` 同口径）。
fn ident_of(w: &str) -> Option<Name> {
    is_ident(w).then(|| Name(w.to_string()))
}
