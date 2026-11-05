//! 语句上下文区间与参数元组查询构造（自 `sqlfn.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::sql::sqlfn::{CallPlan, Region};
use crate::sql::VarScope;
use ogsql_parser::token::{Token, TokenWithSpan};
use std::collections::BTreeMap;

pub(super) fn context_region(
    sql_text: &str,
    tokens: &[TokenWithSpan],
) -> Result<Region, EngineError> {
    let head = statement_head(sql_text);
    let kw = top_level_keywords(sql_text, tokens);
    match head.as_str() {
        "insert" => Ok(Region {
            has_context: false,
            ctx_start: 0,
            ctx_end: 0,
        }),
        "update" => {
            let start = kw
                .get("update")
                .map(|(_, e)| *e)
                .ok_or_else(|| EngineError::business(159, "UPDATE 缺关键字"))?;
            Ok(Region {
                has_context: true,
                ctx_start: start,
                ctx_end: sql_text.len(),
            })
        }
        "select" => {
            let Some((_, from_end)) = kw.get("from") else {
                return Ok(Region {
                    has_context: false,
                    ctx_start: 0,
                    ctx_end: 0,
                });
            };
            Ok(Region {
                has_context: true,
                ctx_start: *from_end,
                ctx_end: sql_text.len(),
            })
        }
        other => Err(EngineError::business(
            159,
            format!("SQL 上下文例程调用暂不支持 {other} 语句"),
        )),
    }
}

/// 为第 `site` 个调用构造参数元组查询。
///
/// 上下文取**该调用所在层级的 SELECT 块**的 FROM/WHERE（动态 SQL 的调用常位于
/// 派生表内部；用最外层 FROM 会把调用本身带进元组查询，形成自引用）：
/// - UPDATE（顶层）：表 + WHERE；
/// - INSERT：无上下文（单行元组）；
/// - SELECT：调用所在括号深度的最近 FROM/WHERE（边界为所在块）。
pub(super) fn distinct_query(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    region: &Region,
    plans: &[CallPlan],
    site: usize,
) -> Result<String, EngineError> {
    let c = &plans[site];
    let args = c.args.join(", ");
    if !region.has_context {
        return Ok(format!("SELECT {args}"));
    }
    let (site_start, _) = c.occurrences[0];
    let block = block_context(sql_text, tokens, site_start);
    let from_seg = match &block.from_seg_override {
        Some(seg) => format!(" FROM {seg}"),
        None => match block.from {
            Some((_, f_end)) => format!(" FROM {}", block.slice(sql_text, f_end, block.from_end)),
            None => String::new(),
        },
    };
    let where_seg = block
        .where_clause
        .map(|(w_after, w_end)| non_self_where(&sql_text[w_after..w_end], c))
        .filter(|w| !w.trim().is_empty());
    let mut q = format!("SELECT DISTINCT {args}{from_seg}");
    if let Some(w) = where_seg {
        q.push_str(" WHERE ");
        q.push_str(&w);
    }
    Ok(q)
}

/// 站点所在括号层级上的最近 SELECT 块上下文（FROM/WHERE 区间）。
pub(super) struct BlockCtx {
    from: Option<(usize, usize)>,
    from_end: usize,
    where_clause: Option<(usize, usize)>,
    /// UPDATE 的表段（无 FROM 关键字）
    from_seg_override: Option<String>,
}

impl BlockCtx {
    fn slice(&self, text: &str, start: usize, end: usize) -> String {
        text[start..end].trim().to_string()
    }
}

pub(super) fn block_context(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    site_start: usize,
) -> BlockCtx {
    // 每个 token 的括号深度（'(' 记为外层深度）
    let mut depths = Vec::with_capacity(tokens.len());
    let mut depth = 0i32;
    for t in tokens {
        match t.token {
            Token::LParen => {
                depths.push(depth);
                depth += 1;
            }
            Token::RParen => {
                depth -= 1;
                depths.push(depth);
            }
            _ => depths.push(depth),
        }
    }
    let site_idx = tokens
        .iter()
        .position(|t| t.span.start == site_start)
        .unwrap_or(0);
    let head = statement_head(sql_text);

    // UPDATE（顶层）：表段 + WHERE
    if head == "update" && depths[site_idx] == 0 {
        let kw = top_level_keywords(sql_text, tokens);
        let mut ctx = BlockCtx {
            from: None,
            from_end: 0,
            where_clause: None,
            from_seg_override: None,
        };
        if let (Some((_, u_end)), Some((set_start, _))) = (kw.get("update"), kw.get("set")) {
            ctx.from = Some((*u_end, *u_end));
            ctx.from_end = *set_start;
            ctx.from_seg_override = Some(sql_text[*u_end..*set_start].trim().to_string());
        }
        if let Some((_, w_end)) = kw.get("where") {
            ctx.where_clause = Some((*w_end, sql_text.len()));
        }
        return ctx;
    }

    // SELECT：自内向外找第一个带 FROM 的层级（调用可能在无 FROM 的标量子查询里，
    // 实参引用外层列的场合）
    for level in (0..=depths[site_idx]).rev() {
        let (block_start, block_end) = block_bounds(tokens, &depths, site_idx, level);
        let mut from = None;
        let mut where_kw = None;
        // 整个块内查找（SELECT 列表中的调用，其 FROM 位于调用之后）
        for (i, t) in tokens.iter().enumerate() {
            if depths[i] != level || t.span.start < block_start || t.span.start >= block_end {
                continue;
            }
            let w = sql_text[t.span.start..t.span.end].to_ascii_lowercase();
            match w.as_str() {
                "from" if from.is_none() => {
                    from = Some((t.span.start, t.span.end));
                }
                "where" if where_kw.is_none() => {
                    where_kw = Some((t.span.start, t.span.end));
                }
                _ => {}
            }
        }
        if let Some((_, f_end)) = from {
            let after_where_clause = where_kw.and_then(|(_, w_end)| {
                tokens
                    .iter()
                    .enumerate()
                    .filter(|(i, t)| {
                        depths[*i] == level && t.span.start >= w_end && t.span.start < block_end
                    })
                    .find(|(_, t)| {
                        let w = sql_text[t.span.start..t.span.end].to_ascii_lowercase();
                        matches!(w.as_str(), "group" | "order" | "having")
                    })
                    .map(|(_, t)| t.span.start)
            });
            let clause_end = where_kw
                .map(|(s, _)| s)
                .or(after_where_clause)
                .unwrap_or(block_end);
            // FROM 段可能是派生表 `(SELECT …)`：必须含配平右括号
            let from_end = match paren_extent(tokens, sql_text, f_end) {
                Some(close_end) => close_end,
                None => clause_end,
            };
            let where_clause = where_kw.map(|(_, e)| {
                (
                    e,
                    after_where_clause.filter(|c| *c > e).unwrap_or(block_end),
                )
            });
            return BlockCtx {
                from,
                from_end,
                where_clause,
                from_seg_override: None,
            };
        }
    }
    // UPDATE 的 SET 实参 / WHERE 中的调用可能位于括号层级，回退到语句级表+WHERE
    if head == "update" {
        let kw = top_level_keywords(sql_text, tokens);
        let mut ctx = BlockCtx {
            from: None,
            from_end: 0,
            where_clause: None,
            from_seg_override: None,
        };
        if let (Some((_, u_end)), Some((set_start, _))) = (kw.get("update"), kw.get("set")) {
            ctx.from_seg_override = Some(sql_text[*u_end..*set_start].trim().to_string());
        }
        if let Some((_, w_end)) = kw.get("where") {
            ctx.where_clause = Some((*w_end, sql_text.len()));
        }
        return ctx;
    }
    BlockCtx {
        from: None,
        from_end: 0,
        where_clause: None,
        from_seg_override: None,
    }
}

/// FROM 段若以 `(` 开头（派生表），返回其配平右括号的结束偏移。
pub(super) fn paren_extent(
    tokens: &[TokenWithSpan],
    sql_text: &str,
    from_after: usize,
) -> Option<usize> {
    let rest = sql_text.get(from_after..)?;
    let skip = rest.len() - rest.trim_start().len();
    if !rest[skip..].starts_with('(') {
        return None;
    }
    let open_pos = from_after + skip;
    let start_tok = tokens.iter().position(|t| t.span.start >= open_pos)?;
    let mut depth = 0i32;
    for t in &tokens[start_tok..] {
        match t.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(t.span.end);
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn block_bounds(
    tokens: &[TokenWithSpan],
    depths: &[i32],
    site_idx: usize,
    level: i32,
) -> (usize, usize) {
    let start = if level == 0 {
        0
    } else {
        tokens[..site_idx]
            .iter()
            .enumerate()
            .rev()
            .find(|(i, t)| matches!(t.token, Token::LParen) && depths[*i] + 1 == level)
            .map(|(_, t)| t.span.end)
            .unwrap_or(0)
    };
    let end = if level == 0 {
        tokens.last().map(|t| t.span.end).unwrap_or(0)
    } else {
        // '(' 记外层深度、')' 记外层深度：内容层级 L 的配平 ')' 深度为 L-1
        tokens[site_idx..]
            .iter()
            .enumerate()
            .find(|(i, t)| matches!(t.token, Token::RParen) && depths[site_idx + i] == level - 1)
            .map(|(_, t)| t.span.start)
            .unwrap_or_else(|| tokens.last().map(|t| t.span.end).unwrap_or(0))
    };
    (start, end)
}

/// 从 WHERE 片段中剔除"包含该调用自身"的顶层合取项。
pub(super) fn non_self_where(raw: &str, c: &CallPlan) -> String {
    let my_names: Vec<String> = c.occurrences.iter().map(|_| c.name.clone()).collect();
    let mut kept = Vec::new();
    for conj in split_top_level_and(raw) {
        let low = conj.to_ascii_lowercase();
        if my_names.iter().any(|n| low.contains(&format!("{n}("))) {
            continue;
        }
        if !conj.trim().is_empty() {
            kept.push(conj.trim().to_string());
        }
    }
    kept.join(" AND ")
}

pub(super) fn split_top_level_and(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote = false;
    let up = raw.to_ascii_uppercase();
    let bytes = up.as_bytes();
    let mut i = 0usize;
    while i < raw.len() {
        let ch = raw.as_bytes()[i] as char;
        if quote {
            if ch == '\'' {
                quote = false;
            }
            cur.push(ch);
            i += 1;
            continue;
        }
        match ch {
            '\'' => {
                quote = true;
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
            _ => {
                let at_and = depth == 0
                    && bytes.get(i) == Some(&b'A')
                    && up.get(i..i + 3) == Some("AND")
                    && (i == 0 || !raw.as_bytes()[i - 1].is_ascii_alphanumeric())
                    && (i + 3 >= raw.len() || !raw.as_bytes()[i + 3].is_ascii_alphanumeric());
                if at_and {
                    parts.push(cur.clone());
                    cur.clear();
                    i += 3;
                    continue;
                }
                cur.push(ch);
            }
        }
        i += 1;
    }
    parts.push(cur);
    parts
}

pub(super) fn statement_head(sql_text: &str) -> String {
    sql_text
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub(super) fn top_level_keywords(
    sql_text: &str,
    tokens: &[TokenWithSpan],
) -> BTreeMap<String, (usize, usize)> {
    const KW: [&str; 9] = [
        "update", "insert", "select", "set", "from", "where", "group", "order", "having",
    ];
    let mut depth = 0i32;
    let mut out = BTreeMap::new();
    for tws in tokens {
        match tws.token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            _ => {
                if depth == 0 {
                    let w = sql_text[tws.span.start..tws.span.end].to_ascii_lowercase();
                    if KW.contains(&w.as_str()) && !out.contains_key(&w) {
                        out.insert(w, (tws.span.start, tws.span.end));
                    }
                }
            }
        }
    }
    out
}

/// 语句文本 → 改写 + 方言变换 + 记录字段绑定（与 `ir::rewrite_sql` 同口径）。
pub(super) fn rewrite_sql_text(
    sql_text: &str,
    vars: &VarScope,
    funcs: &BTreeMap<String, String>,
) -> Result<crate::sql::SqlUnit, EngineError> {
    let stripped = crate::sql::strip_update_alias(sql_text);
    let unit = crate::sql::rewrite_tokens_with_rules(&stripped, vars, funcs)?;
    let text = crate::sql::apply_ast_transforms(&unit.text)?;
    Ok(crate::sql::SqlUnit {
        text,
        binds: unit.binds,
        fn_sites: Vec::new(),
    })
}
