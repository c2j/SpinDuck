//! 查找 SQL 外层 SELECT 列名提取（trap 折叠的 INTO 位置对齐用）。

/// 投影列原文列表与 FROM 关键字起始偏移：列文本保留限定前缀
/// （`t.col`），供按列重建单列 SELECT；形态约束：仅接受 `SELECT <标识符,…> FROM`，表达式、星号、字符串或 DISTINCT 一律 None。
pub(super) fn select_list_parts(sql: &str) -> Option<(Vec<String>, usize)> {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut from_pos = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_str = false;
                }
            }
        } else {
            match c {
                b'\'' => in_str = true,
                b'(' => depth += 1,
                b')' => depth = depth.checked_sub(1)?,
                _ => {
                    if depth == 0
                        && i > 0
                        && bytes[i - 1].is_ascii_whitespace()
                        && sql.is_char_boundary(i)
                        && sql[i..].len() >= 4
                        && sql[i..i + 4].eq_ignore_ascii_case("from")
                        && bytes.get(i + 4).is_some_and(|n| n.is_ascii_whitespace())
                    {
                        from_pos = Some(i);
                        break;
                    }
                }
            }
        }
        i += 1;
    }
    let head = sql[..from_pos?].trim_start();
    let list = head
        .get(6..)
        .filter(|_| head[..6].eq_ignore_ascii_case("select"))?
        .trim();
    if list.is_empty()
        || list
            .split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("distinct") || w.eq_ignore_ascii_case("all"))
    {
        return None;
    }
    let parts = split_top_commas(list)?
        .iter()
        .map(|part| {
            let text = part.trim();
            column_name(text)?;
            Some(text.to_string())
        })
        .collect::<Option<Vec<_>>>()?;
    // from_pos 是原串字节偏移；list 起点 = head 在原串中的位置 + 6 + 前导空白。
    // 直接用 from_pos 切片即可，无需反推 list 边界。
    Some((parts, from_pos?))
}

/// 顶层逗号切分（跟踪括号深度与单引号串）。
fn split_top_commas(list: &str) -> Option<Vec<String>> {
    let bytes = list.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_str = false;
                }
            }
        } else {
            match c {
                b'\'' => in_str = true,
                b'(' => depth += 1,
                b')' => depth = depth.checked_sub(1)?,
                b',' if depth == 0 => {
                    parts.push(list[start..i].to_string());
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if depth != 0 || in_str {
        return None;
    }
    parts.push(list[start..].to_string());
    Some(parts)
}

/// 投影列名：接受裸/限定标识符（`a.b.c` 取末段，即派生表输出列名）；
/// 表达式、星号、字符串等一律 None。
fn column_name(text: &str) -> Option<String> {
    let last = text.rsplit('.').next()?;
    let mut chars = last.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // 限定前缀各段也必须是裸标识符（`a.b` 合法，`a.*`/`a.b+c` 拒绝）。
    if text.len() != last.len() {
        let prefix = &text[..text.len() - last.len() - 1];
        if !prefix.split('.').all(|seg| {
            let mut sc = seg.chars();
            matches!(sc.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                && sc.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            return None;
        }
    }
    Some(last.to_string())
}
