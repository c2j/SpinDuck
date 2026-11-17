//! Gate 2B 局部 DDL stub 执行辅助（自 `prepare.rs` 拆分，AGENTS §3）：
//! CTAS 子查询包裹、`WITH [NO] DATA` 剥离、残留 `$v_*` 占位符扫描。
//! 全部为纯文本变换，不触内核。

use std::collections::BTreeSet;

/// 词边界 + 跳过字符串/注释/引号标识符的关键字定位（大小写不敏感）。
/// 返回首个匹配的字节区间。
pub(super) fn find_keyword_ci(sql: &str, kw: &str) -> Option<(usize, usize)> {
    let b = sql.as_bytes();
    let kwb = kw.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            _ => {
                if i + kwb.len() <= b.len()
                    && b[i..i + kwb.len()].eq_ignore_ascii_case(kwb)
                    && (i == 0 || !ident(b[i - 1]))
                    && (i + kwb.len() >= b.len() || !ident(b[i + kwb.len()]))
                {
                    return Some((i, i + kwb.len()));
                }
                i += 1;
            }
        }
    }
    None
}

/// 剥掉 CTAS 末尾的 `WITH [NO] DATA` 子句（Oracle 语法，DuckDB 不认）。
pub(super) fn strip_ctas_with_data(body: &str) -> &str {
    let low = body.to_ascii_lowercase();
    for suf in ["with no data", "with data"] {
        if low.ends_with(suf) {
            let cut = body.len() - suf.len();
            let boundary = body[..cut]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
            if boundary {
                return body[..cut].trim_end();
            }
        }
    }
    body
}

/// CTAS → `head AS SELECT * FROM (<query>) _sd LIMIT 0`：查询整体包成
/// 子查询，原有 LIMIT/FETCH 不再与追加的 LIMIT 0 冲突；`WITH [NO] DATA`
/// 剥掉（建表只为 binder 发现列，不物化数据）。找不到 AS 关键字
/// （理论不达）返回 `None`。
pub(super) fn ctas_limit0_sql(sql: &str) -> Option<String> {
    let body = sql.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    let body = strip_ctas_with_data(body);
    let (as_start, as_end) = find_keyword_ci(body, "as")?;
    let head = body[..as_start].trim_end();
    let query = body[as_end..].trim();
    if head.is_empty() || query.is_empty() {
        return None;
    }
    Some(format!("{head} AS SELECT * FROM ({query}) _sd LIMIT 0"))
}

/// 文本中残留的 `$v_*` 占位符（跳过字符串/注释/引号标识符）：
/// `execute` 要求全部参数有绑定，未映射类型（%TYPE/记录字段/
/// 映射表外类型）留下的占位符在 stub 执行前绑 NULL。
pub(super) fn residual_placeholders(sql: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let b = sql.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b'$' if i + 2 < b.len()
                && b[i + 1] == b'v'
                && b[i + 2] == b'_'
                && (i == 0 || (!ident(b[i - 1]) && b[i - 1] != b'$')) =>
            {
                let start = i;
                i += 3;
                while i < b.len() && ident(b[i]) {
                    i += 1;
                }
                out.insert(sql[start..i].to_string());
            }
            _ => i += 1,
        }
    }
    out
}
