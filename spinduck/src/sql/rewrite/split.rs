//! 顶层分隔符切分与字面量引号处理（括号/字符串感知）。

/// 顶层逗号切分（括号与字符串感知，字符级遍历：多字节实参必须逐字保留）。
pub(crate) fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut cur = String::new();
    let mut it = s.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        if in_string {
            cur.push(c);
            if c == '\'' {
                if matches!(it.peek(), Some((_, '\''))) {
                    it.next();
                    cur.push('\'');
                } else {
                    in_string = false;
                }
            }
        } else {
            match c {
                '\'' => {
                    in_string = true;
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    out.push(cur.trim().to_string());
                    cur = String::new();
                }
                _ => cur.push(c),
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// 按顶层 `-`（非括号内/非字符串字面量内）拆分为两段；不是恰好一段时返回空。
pub(super) fn split_top_level_minus(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut cur = String::new();
    for ch in text.chars() {
        match ch {
            '\'' => {
                in_quote = !in_quote;
                cur.push(ch);
            }
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            '-' if depth == 0 && !in_quote => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    parts.push(cur.trim().to_string());
    if parts.len() == 2 {
        parts
    } else {
        Vec::new()
    }
}

/// 再转义：反转义后的文本重新嵌入单引号字面量前必须还原 `'` → `''`
/// （unquote 与 requote 必须成对使用，否则掩码含引号即产出畸形 SQL）。
pub(super) fn requote(s: &str) -> String {
    s.replace('\'', "''")
}

pub(super) fn unquote(literal: &str) -> String {
    let t = literal.trim();
    if t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2 {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    }
}
