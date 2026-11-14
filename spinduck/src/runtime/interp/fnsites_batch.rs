//! fn_sites 入批：非嵌套单值 INSERT VALUES 的缓存子查询内联（分支 3）。
//!
//! 语义边界（与即时回落等价，opt-in `batch_commit`）：
//! - 仅「INSERT … VALUES」且**非嵌套**（元组查询不引用其他 `sd_fn` 缓存表）
//!   形态可内联：此时每个调用点的元组查询是单行 `SELECT <实参>`，例程返回值
//!   唯一确定。嵌套下沉与 INSERT … SELECT 回落即时下发（`materialize_fn_sites`
//!   在 `sqlstmts::exec` 执行，拓扑序/缓存表语义一字不改）。
//! - 例程在 push 时（语句边界）经 `call_routine_value` 求值——与即时路径同一
//!   接缝、同一时机；错误在此传播（不回落，避免重复求值副作用）。
//! - 取**原始 `SpinValue`** 经 `sql_literal` 内联，而非从 VARCHAR 缓存表回读：
//!   缓存表 `sd_v` 存的是字面量文本，回读会丢失「数值 vs 字符串」的引号信息
//!   （`'001'` 与 `1` 不可区分），直接内联原始值才能保留类型。
//! - 读自己写沿用即时路径口径：仅按 INSERT 目标表刷盘（`flush_if_touches`），
//!   例程读取其他缓冲表的可见性与即时路径一致（同为已知边界）。

use crate::error::EngineError;
use crate::runtime::interp::dynamic::sql_literal;
use crate::runtime::interp::Env;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::{Token, TokenWithSpan};

/// 尝试把 fn_sites 的缓存子查询内联为字面量，返回可攒批的纯 INSERT 文本。
///
/// `Ok(None)` = 不可内联（非 VALUES 形态 / 嵌套下沉），调用方回落即时下发；
/// 此时**尚未求值任何例程**，回落路径的 `materialize_fn_sites` 不会重复副作用。
pub(super) fn inline_fn_sites(
    env: &mut Env<'_>,
    sql: &str,
    sites: &[crate::sql::FnSite],
) -> Result<Option<String>, EngineError> {
    if !sql.to_ascii_lowercase().contains(" values") {
        return Ok(None);
    }
    if sites
        .iter()
        .any(|s| s.distinct_sql.to_ascii_lowercase().contains("sd_fn"))
    {
        return Ok(None);
    }
    let mut text = sql.to_string();
    for site in sites {
        let binds = crate::runtime::interp::resolve_binds(&site.binds, env.frame)?;
        let mut st = env.ex.query(&site.distinct_sql, &binds)?;
        let rows = st.fetch_many(2);
        // INSERT VALUES 单行元组是构造保证；越界属内核异常，显式报错而非 panic（§5）
        if rows.len() != 1 {
            return Err(EngineError::invariant(
                33,
                format!("fn_sites 入批：{} 元组查询非单行", site.name),
            ));
        }
        let v = crate::runtime::interp::routine::call_routine_value(env, &site.name, &rows[0])?;
        let lit = sql_literal(&v);
        text = replace_cache_subquery(&text, &site.table, &lit).ok_or_else(|| {
            EngineError::invariant(
                33,
                format!("fn_sites 入批：未找到缓存子查询 {}", site.table),
            )
        })?;
    }
    Ok(Some(text))
}

/// 把文本中所有 `(SELECT sd_v FROM <table> WHERE …)` 缓存子查询替换为 `lit`。
///
/// token 级配平括号定位（复用 ogsql 分词，字符串字面量内的括号不参与计数），
/// 替换字节区间 `[open.start, close.end]`。未命中返回 `None`。
fn replace_cache_subquery(text: &str, table: &str, lit: &str) -> Option<String> {
    let tokens = Tokenizer::new(text).tokenize().ok()?;
    let lower = text.to_ascii_lowercase();
    let table_l = table.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len() + lit.len());
    let mut cursor = 0usize;
    let mut found = false;
    let mut k = 0;
    while k < tokens.len() {
        if matches!(tokens[k].token, Token::LParen) && subquery_head(&lower, &tokens, k, &table_l) {
            let close = match_rparen(&tokens, k)?;
            let s = tokens[k].span.start;
            let e = tokens[close].span.end;
            out.push_str(&text[cursor..s]);
            out.push_str(lit);
            cursor = e;
            found = true;
            k = close + 1;
            continue;
        }
        k += 1;
    }
    if !found {
        return None;
    }
    out.push_str(&text[cursor..]);
    Some(out)
}

/// `tokens[k]` 的 `(` 是否开启 `(SELECT sd_v FROM <table>` 缓存子查询。
fn subquery_head(lower: &str, tokens: &[TokenWithSpan], k: usize, table: &str) -> bool {
    for (j, w) in ["select", "sd_v", "from", table].iter().enumerate() {
        let Some(t) = tokens.get(k + 1 + j) else {
            return false;
        };
        if &lower[t.span.start..t.span.end] != *w {
            return false;
        }
    }
    true
}

/// 从 `open`（LParen）起配平右括号的下标（token 级，字符串内括号不计）。
fn match_rparen(tokens: &[TokenWithSpan], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (j, t) in tokens.iter().enumerate().skip(open) {
        match t.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::replace_cache_subquery;

    #[test]
    fn should_replace_single_cache_subquery_with_literal() {
        let sql = "insert into t (n) values ((select sd_v from sd_fn0 where sd_a0 is not distinct from ($v_i)))";
        let out = replace_cache_subquery(sql, "sd_fn0", "2").unwrap();
        assert_eq!(out, "insert into t (n) values (2)");
    }

    #[test]
    fn should_not_miscount_parens_inside_string_literal_arg() {
        // 实参字符串含括号：字节级括号计数会误匹配，token 级须正确配平
        let sql = "insert into t (s) values ((select sd_v from sd_fn0 where sd_a0 is not distinct from ('a(b)c')))";
        let out = replace_cache_subquery(sql, "sd_fn0", "'x'").unwrap();
        assert_eq!(out, "insert into t (s) values ('x')");
    }

    #[test]
    fn should_replace_every_occurrence_of_deduped_site() {
        let sql = "insert into t (a, b) values ((select sd_v from sd_fn0 where sd_a0 is not distinct from (1)), (select sd_v from sd_fn0 where sd_a0 is not distinct from (1)))";
        let out = replace_cache_subquery(sql, "sd_fn0", "9").unwrap();
        assert_eq!(out, "insert into t (a, b) values (9, 9)");
    }

    #[test]
    fn should_replace_only_matching_table_and_keep_other_site() {
        let sql = "insert into t (a, b) values ((select sd_v from sd_fn0 where sd_a0 is not distinct from (1)), (select sd_v from sd_fn1 where sd_a0 is not distinct from (2)))";
        let out = replace_cache_subquery(sql, "sd_fn0", "9").unwrap();
        assert_eq!(
            out,
            "insert into t (a, b) values (9, (select sd_v from sd_fn1 where sd_a0 is not distinct from (2)))"
        );
    }

    #[test]
    fn should_return_none_when_subquery_absent() {
        let sql = "insert into t (n) values (1)";
        assert!(replace_cache_subquery(sql, "sd_fn0", "2").is_none());
    }
}
