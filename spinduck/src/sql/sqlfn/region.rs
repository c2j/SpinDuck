//! 语句上下文区间与参数元组查询构造（自 `sqlfn.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::sql::sqlfn::lower::{lower_spans, top_level_and_ranges};
use crate::sql::sqlfn::{CallPlan, Occurrence, Region};
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
        // INSERT INTO … SELECT：实参多取自源表列，元组查询必须落到 SELECT 侧上下文；
        // INSERT … VALUES 是单行求值（顶层无 SELECT 关键字），无表行上下文。
        "insert" => Ok(Region {
            has_context: kw.contains_key("select"),
        }),
        "update" => Ok(Region {
            has_context: kw.contains_key("update"),
        }),
        "select" => Ok(Region {
            has_context: kw.contains_key("from"),
        }),
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
/// - INSERT … VALUES：无上下文（单行元组）；
/// - SELECT / INSERT … SELECT：调用所在块（`UNION` 分支各自成块）的 FROM/WHERE；
/// - 片段内的其他调用点按其缓存子查询就地下沉（内层先物化）。
pub(super) fn distinct_query(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    region: &Region,
    plans: &[CallPlan],
    occs: &[Occurrence],
    site: usize,
) -> Result<String, EngineError> {
    let c = &plans[site];
    let args = c.lowered_args.join(", ");
    if !region.has_context {
        return Ok(format!("SELECT {args}"));
    }
    let block = block_context(sql_text, tokens, c.occ_span.0);
    let from_seg = match block.from {
        Some((_, f_end)) if f_end < block.from_end => format!(
            " FROM {}",
            lower_spans(sql_text, f_end, block.from_end, site, plans, occs)?.trim()
        ),
        _ => String::new(),
    };
    let where_seg = match block.where_clause {
        Some((w_after, w_end)) => {
            let mut kept: Vec<String> = Vec::new();
            for (s, e) in top_level_and_ranges(sql_text, w_after, w_end) {
                if sql_text[s..e].trim().is_empty() {
                    continue;
                }
                // 含本站点调用的合取项必须剔除（否则元组查询自引用）
                if occs
                    .iter()
                    .any(|o| o.plan == site && o.start >= s && o.end <= e)
                {
                    continue;
                }
                let seg = lower_spans(sql_text, s, e, site, plans, occs)?;
                let seg = seg.trim();
                if !seg.is_empty() {
                    kept.push(seg.to_string());
                }
            }
            if kept.is_empty() {
                None
            } else {
                Some(kept.join(" AND "))
            }
        }
        None => None,
    };
    let mut q = format!("SELECT DISTINCT {args}{from_seg}");
    if let Some(w) = where_seg {
        q.push_str(" WHERE ");
        q.push_str(&w);
    }
    Ok(q)
}

/// 站点所在括号层级上的最近 SELECT 块上下文（FROM/WHERE 区间）。
pub(super) struct BlockCtx {
    /// FROM 关键字区间；UPDATE 复用为表段起点（`from_end` 即 SET 起点）
    from: Option<(usize, usize)>,
    from_end: usize,
    where_clause: Option<(usize, usize)>,
}

impl BlockCtx {
    /// 元组查询取材的文本区域（表段 + WHERE）：其中的其他调用点须先物化。
    pub(super) fn regions(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if let Some((kw_start, _)) = self.from {
            out.push((kw_start, self.from_end));
        }
        out.extend(self.where_clause);
        out
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
        };
        if let (Some((_, u_end)), Some((set_start, _))) = (kw.get("update"), kw.get("set")) {
            ctx.from = Some((*u_end, *u_end));
            ctx.from_end = *set_start;
        }
        if let Some((_, w_end)) = kw.get("where") {
            ctx.where_clause = Some((*w_end, sql_text.len()));
        }
        return ctx;
    }

    // SELECT：自内向外找第一个带 FROM 的层级（调用可能在无 FROM 的标量子查询里，
    // 实参引用外层列的场合）
    for level in (0..=depths[site_idx]).rev() {
        let (block_start, block_end) = block_bounds(sql_text, tokens, &depths, site_idx, level);
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
        };
        if let (Some((_, u_end)), Some((set_start, _))) = (kw.get("update"), kw.get("set")) {
            ctx.from = Some((*u_end, *u_end));
            ctx.from_end = *set_start;
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

/// 站点所属查询块的边界。
///
/// 边界取"最近者"：同层配平括号，或同层集合运算关键字（`UNION`/`INTERSECT`/
/// `EXCEPT`）。后者必须参与切块——`INSERT … SELECT … UNION ALL SELECT …` 的每个
/// 分支各有源表，若不切块则后面的分支会借到第一个分支的 FROM/WHERE。
pub(super) fn block_bounds(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    depths: &[i32],
    site_idx: usize,
    level: i32,
) -> (usize, usize) {
    let at_level = |i: usize| depths[i] == level && is_set_op_word(sql_text, &tokens[i]);
    let mut start = 0usize;
    for i in (0..site_idx).rev() {
        if matches!(tokens[i].token, Token::LParen) && depths[i] + 1 == level {
            start = tokens[i].span.end;
            break;
        }
        if at_level(i) {
            start = tokens[i].span.end;
            break;
        }
    }
    let mut end = tokens.last().map(|t| t.span.end).unwrap_or(0);
    for i in site_idx..tokens.len() {
        if matches!(tokens[i].token, Token::RParen) && depths[i] == level - 1 {
            end = tokens[i].span.start;
            break;
        }
        if at_level(i) {
            end = tokens[i].span.start;
            break;
        }
    }
    (start, end)
}

fn is_set_op_word(sql_text: &str, tws: &TokenWithSpan) -> bool {
    if !matches!(tws.token, Token::Ident(_) | Token::Keyword(_)) {
        return false;
    }
    matches!(
        sql_text[tws.span.start..tws.span.end]
            .to_ascii_lowercase()
            .as_str(),
        "union" | "intersect" | "except"
    )
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
