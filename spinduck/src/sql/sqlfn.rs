//! SQL 上下文例程调用下沉（§8.3 路径②）：语句中出现**已注册 PL 例程**的调用时，
//! 编译期构造 `SELECT DISTINCT <args> …` 参数元组查询，并把每个调用点替换为
//! 针对参数缓存的相关子查询；运行期逐元组调用例程填充缓存表。
//!
//! 语义前提（文档化限制）：
//! - 例程在语句执行期间**确定性**（结果只依赖入参）——语料中为查表类函数；
//! - 同一语句中重复的相同调用（同 name+实参文本）合并为一次物化；
//! - 参数元组上界 [`MAX_TUPLES`]；
//! - 上下文互相引用的调用（A 的 WHERE 调 B、B 的 WHERE 调 A）显式拒绝。

use crate::error::EngineError;
use crate::ir::BindSrc;
use crate::sql::VarScope;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::{Token, TokenWithSpan};
use std::collections::{BTreeMap, BTreeSet};

/// 单次下沉允许的参数元组上界（超出即显式拒绝，避免无界临时表）。
pub const MAX_TUPLES: usize = 100_000;

/// 一个去重后的调用点及其下沉计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnSite {
    /// 缓存表名（运行期 `CREATE OR REPLACE TEMP TABLE`）
    pub table: String,
    /// 被调例程名（小写）
    pub name: String,
    /// 调用实参文本（原始形态，供诊断）
    pub args: Vec<String>,
    /// 参数元组查询（`SELECT DISTINCT … [FROM …] [WHERE …]`，已改写）
    pub distinct_sql: String,
    /// 元组查询所需绑定
    pub binds: Vec<(String, BindSrc)>,
}

struct Occurrence {
    start: usize,
    end: usize,
    name: String,
    args: Vec<String>,
    args_key: String,
}

struct CallPlan {
    name: String,
    args: Vec<String>,
    distinct_sql: String,
    binds: Vec<(String, BindSrc)>,
    sub_orig: String,
    occurrences: Vec<(usize, usize)>,
    depends_on: Vec<usize>,
}

/// 语句的上下文区间：FROM/WHERE（SELECT）、表/WHERE（UPDATE）；
/// INSERT 无上下文（参数元组即单行）。
struct Region {
    has_context: bool,
    ctx_start: usize,
    ctx_end: usize,
}

impl Region {
    fn contains(&self, pos: usize) -> bool {
        self.has_context && pos >= self.ctx_start && pos < self.ctx_end
    }
}

/// 扫描语句文本中的已注册例程调用并生成下沉计划。
///
/// 返回（已内联子查询的语句文本, 计划列表）。调用点为空时原样返回。
pub fn plan_sites(
    sql_text: &str,
    var_names: &VarScope,
    funcs: &BTreeMap<String, String>,
    routine_names: &BTreeSet<String>,
) -> Result<(String, Vec<FnSite>), EngineError> {
    if routine_names.is_empty() {
        return Ok((sql_text.to_string(), Vec::new()));
    }
    let tokens = Tokenizer::new(sql_text)
        .tokenize()
        .map_err(|e| EngineError::invariant(46, format!("tokenizer: {e}")))?;
    let occ = collect_occurrences(sql_text, &tokens, routine_names)?;
    if occ.is_empty() {
        return Ok((sql_text.to_string(), Vec::new()));
    }
    let region = context_region(sql_text, &tokens)?;

    // 去重：同 name + 同实参文本只物化一次
    let mut plans: Vec<CallPlan> = Vec::new();
    let mut dedup: BTreeMap<String, usize> = BTreeMap::new();
    for o in &occ {
        let key = format!("{}\u{0}{}", o.name, o.args_key);
        match dedup.get(&key) {
            Some(&i) => plans[i].occurrences.push((o.start, o.end)),
            None => {
                dedup.insert(key, plans.len());
                plans.push(CallPlan {
                    name: o.name.clone(),
                    args: o.args.clone(),
                    distinct_sql: String::new(),
                    binds: Vec::new(),
                    sub_orig: String::new(),
                    occurrences: vec![(o.start, o.end)],
                    depends_on: Vec::new(),
                });
            }
        }
    }
    reject_nested(&plans, routine_names)?;
    let ctx_sites: Vec<Vec<usize>> = (0..plans.len())
        .map(|i| {
            (0..plans.len())
                .filter(|&j| {
                    j != i
                        && plans[j]
                            .occurrences
                            .iter()
                            .any(|(s, _)| region.contains(*s))
                })
                .collect()
        })
        .collect();
    for (i, deps) in ctx_sites.into_iter().enumerate() {
        plans[i].depends_on = deps;
    }
    let order = topo_order(&plans)?;

    for &i in &order {
        let c = &plans[i];
        let distinct_orig = distinct_query(sql_text, &tokens, &region, &plans, i)?;
        let unit = rewrite_sql_text(&distinct_orig, var_names, funcs)?;
        let cond: Vec<String> = c
            .args
            .iter()
            .enumerate()
            .map(|(j, a)| format!("a{j} IS NOT DISTINCT FROM ({a})"))
            .collect();
        plans[i].distinct_sql = unit.text;
        plans[i].binds = unit.binds;
        plans[i].sub_orig = format!("(SELECT v FROM sd_fn{i} WHERE {})", cond.join(" AND "));
    }

    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (i, c) in plans.iter().enumerate() {
        for (s, e) in &c.occurrences {
            spans.push((*s, *e, i));
        }
    }
    spans.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(sql_text.len() + 128);
    let mut cursor = 0usize;
    for (s, e, i) in spans {
        out.push_str(&sql_text[cursor..s]);
        out.push_str(&plans[i].sub_orig);
        cursor = e;
    }
    out.push_str(&sql_text[cursor..]);

    let sites = order
        .iter()
        .map(|&i| FnSite {
            table: format!("sd_fn{i}"),
            name: plans[i].name.clone(),
            args: plans[i].args.clone(),
            distinct_sql: plans[i].distinct_sql.clone(),
            binds: plans[i].binds.clone(),
        })
        .collect();
    Ok((out, sites))
}

fn collect_occurrences(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    routine_names: &BTreeSet<String>,
) -> Result<Vec<Occurrence>, EngineError> {
    let mut occ = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let tws = &tokens[i];
        let word = sql_text[tws.span.start..tws.span.end].to_ascii_lowercase();
        let identish = matches!(
            tws.token,
            Token::Ident(_) | Token::Keyword(_) | Token::QuotedIdent(_)
        );
        if identish && routine_names.contains(&word) {
            if let Some(open) = tokens.get(i + 1) {
                if matches!(open.token, Token::LParen) {
                    let close = matching_rparen(tokens, i + 1)?;
                    let args_text = &sql_text[open.span.end..tokens[close].span.start];
                    let args: Vec<String> = crate::sql::rewrite::split_top_level_commas(args_text)
                        .into_iter()
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty())
                        .collect();
                    occ.push(Occurrence {
                        start: tws.span.start,
                        end: tokens[close].span.end,
                        name: word,
                        args_key: args.join("\u{0}"),
                        args,
                    });
                    i = close + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    Ok(occ)
}

fn matching_rparen(tokens: &[TokenWithSpan], open_idx: usize) -> Result<usize, EngineError> {
    let mut depth = 0i32;
    for (j, t) in tokens.iter().enumerate().skip(open_idx) {
        match t.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Ok(j);
                }
            }
            _ => {}
        }
    }
    Err(EngineError::invariant(
        45,
        "unbalanced parentheses in call site",
    ))
}

fn reject_nested(plans: &[CallPlan], routine_names: &BTreeSet<String>) -> Result<(), EngineError> {
    for c in plans {
        for a in &c.args {
            let low = a.to_ascii_lowercase();
            for n in routine_names {
                if low.contains(&format!("{n}(")) {
                    return Err(EngineError::business(
                        158,
                        format!("SQL 上下文例程调用不支持嵌套（{}({:?})）", c.name, a),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 拓扑排序：上下文里出现的调用先物化；互相引用 → 显式拒绝。
fn topo_order(plans: &[CallPlan]) -> Result<Vec<usize>, EngineError> {
    let n = plans.len();
    let mut indeg = vec![0usize; n];
    let mut out_edges = vec![Vec::new(); n];
    for (i, c) in plans.iter().enumerate() {
        for &j in &c.depends_on {
            out_edges[j].push(i);
            indeg[i] += 1;
        }
    }
    let mut ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    ready.sort_unstable();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = ready.pop() {
        order.push(i);
        for &k in &out_edges[i] {
            indeg[k] -= 1;
            if indeg[k] == 0 {
                ready.push(k);
            }
        }
        ready.sort_unstable();
    }
    if order.len() != n {
        return Err(EngineError::business(
            160,
            "SQL 上下文例程调用存在循环依赖（上下文互相引用）".to_string(),
        ));
    }
    Ok(order)
}

fn context_region(sql_text: &str, tokens: &[TokenWithSpan]) -> Result<Region, EngineError> {
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
fn distinct_query(
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
struct BlockCtx {
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

fn block_context(sql_text: &str, tokens: &[TokenWithSpan], site_start: usize) -> BlockCtx {
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
fn paren_extent(tokens: &[TokenWithSpan], sql_text: &str, from_after: usize) -> Option<usize> {
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

fn block_bounds(
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
fn non_self_where(raw: &str, c: &CallPlan) -> String {
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

fn split_top_level_and(raw: &str) -> Vec<String> {
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

fn statement_head(sql_text: &str) -> String {
    sql_text
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn top_level_keywords(
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
fn rewrite_sql_text(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn should_inline_update_call_as_parameter_cache_subquery() {
        let sql = "UPDATE mid_yjqs_detail t SET t.fund_code = fnc_com_get_fund_code(t.gddm, t.sub_gddm, p_i_date, t.xwdm) WHERE t.bcrq = p_i_date AND scdm = '001'";
        let vars: VarScope = ["p_i_date".to_string()].into_iter().collect();
        let (out, sites) = plan_sites(
            sql,
            &vars,
            &BTreeMap::new(),
            &names(&["fnc_com_get_fund_code"]),
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].name, "fnc_com_get_fund_code");
        assert_eq!(sites[0].table, "sd_fn0");
        assert_eq!(
            sites[0].distinct_sql,
            "SELECT DISTINCT t.gddm, t.sub_gddm, $v_p_i_date, t.xwdm FROM mid_yjqs_detail t WHERE t.bcrq = $v_p_i_date AND scdm = '001'"
        );
        assert!(
            out.contains(
                "SET t.fund_code = (SELECT v FROM sd_fn0 WHERE a0 IS NOT DISTINCT FROM (t.gddm)"
            ),
            "{out}"
        );
        assert!(!out.contains("fnc_com_get_fund_code("));
    }

    #[test]
    fn should_strip_own_predicate_from_distinct_context() {
        // WHERE 中调用的例程：参数元组查询不得包含该谓词（否则自引用）
        let sql = "SELECT a.gddm FROM mid_yjqs_detail a WHERE a.bcrq = '20240105' AND (SELECT fnc_get_fund_data_mode(a.bcrq, a.gddm, a.sub_gddm, a.xwdm)) = '1' AND a.cjsl > 0";
        let (out, sites) = plan_sites(
            sql,
            &VarScope::new(),
            &BTreeMap::new(),
            &names(&["fnc_get_fund_data_mode"]),
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(
            sites[0].distinct_sql,
            "SELECT DISTINCT a.bcrq, a.gddm, a.sub_gddm, a.xwdm FROM mid_yjqs_detail a WHERE a.bcrq = '20240105' AND a.cjsl > 0"
        );
        assert!(out.contains("(SELECT v FROM sd_fn0"));
    }

    #[test]
    fn should_dedupe_identical_calls_and_inline_each_occurrence() {
        let sql = "SELECT fnc_x(a), fnc_x(a) FROM t GROUP BY fnc_x(a)";
        let (out, sites) =
            plan_sites(sql, &VarScope::new(), &BTreeMap::new(), &names(&["fnc_x"])).unwrap();
        assert_eq!(sites.len(), 1, "同 name+实参只物化一次");
        assert_eq!(out.matches("(SELECT v FROM sd_fn0").count(), 3);
    }

    #[test]
    fn should_plan_insert_values_as_single_row_context() {
        let sql = "INSERT INTO dat_fund_cjqs (a, b) VALUES (1, fnc_get_rate_info2(f, d, '3', k))";
        let (out, sites) = plan_sites(
            sql,
            &VarScope::new(),
            &BTreeMap::new(),
            &names(&["fnc_get_rate_info2"]),
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].distinct_sql, "SELECT f, d, '3', k");
        assert!(out.contains("(SELECT v FROM sd_fn0"));
    }

    #[test]
    fn should_plan_call_nested_in_derived_table_with_inner_context() {
        // 语料 prc_trd_hz_byfund 动态 SQL 形态：调用在两层派生表的 SELECT 列表，
        // 且同名调用亦出现在 GROUP BY（去重）；参数元组必须取自**内层** FROM/WHERE，
        // 不能把调用本身带进元组查询（自引用）。
        let sql = "SELECT SUM(FXJ), ROUND(NVL(SUM(fxj2), 0), 2), FUND_CODE, XWDM, TRADE_TYPE, VENTURE_TYPE FROM (SELECT SUM(NVL(ROUND(fxj, 2), 0)) FXJ, NVL(SUM(fxj), 0) FXJ2, fund_code, bs, xwdm, trade_type, venture_type FROM (SELECT zqdm, fund_code, bs, xwdm, trade_type, SUM(NVL(fxj, 0)) FXJ, fnc_com_get_seat_fee_type(BCRQ, SCDM, FUND_CODE, XWDM, STOCK_KIND) VENTURE_TYPE FROM mid_yjqs_detail WHERE SCDM = '001' AND BCRQ = '20250715' AND (substr(stock_kind, 1, 2) = '01') AND NOT (is_yysg = '1' AND ywlb IN ('ES', '77')) GROUP BY zqdm, bs, fund_code, xwdm, trade_type, fnc_com_get_seat_fee_type(BCRQ, SCDM, FUND_CODE, XWDM, STOCK_KIND)) GROUP BY fund_code, bs, XWDM, TRADE_TYPE, venture_type) GROUP BY fund_code, XWDM, TRADE_TYPE, venture_type";
        let (out, sites) = plan_sites(
            sql,
            &VarScope::new(),
            &BTreeMap::new(),
            &names(&["fnc_com_get_seat_fee_type"]),
        )
        .unwrap();
        assert_eq!(sites.len(), 1, "同名调用去重");
        assert!(
            !sites[0]
                .distinct_sql
                .to_ascii_lowercase()
                .contains("fnc_com_get_seat_fee_type("),
            "元组查询不得自引用：{}",
            sites[0].distinct_sql
        );
        assert!(
            sites[0].distinct_sql.contains("FROM mid_yjqs_detail"),
            "上下文应取内层 FROM：{}",
            sites[0].distinct_sql
        );
        assert_eq!(out.matches("(SELECT v FROM sd_fn0").count(), 2);
    }

    #[test]
    fn should_reject_nested_routine_call_in_arguments() {
        let e = plan_sites(
            "SELECT fnc_a(fnc_b(x)) FROM t",
            &VarScope::new(),
            &BTreeMap::new(),
            &names(&["fnc_a", "fnc_b"]),
        )
        .unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0158");
    }

    #[test]
    fn should_reject_unsupported_statement_shape() {
        let e = plan_sites(
            "MERGE INTO t USING s ON (1=1) WHEN MATCHED THEN UPDATE SET a = fnc_x(b)",
            &VarScope::new(),
            &BTreeMap::new(),
            &names(&["fnc_x"]),
        )
        .unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0159");
    }
}
