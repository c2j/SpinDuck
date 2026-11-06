//! 调用点扫描与下沉计划构造（自 `sqlfn.rs` 拆分，AGENTS §3）。
//!
//! 一次语句的下沉分四步：递归扫描调用点（含实参内的嵌套）→ 按 `name+实参文本`
//! 去重成计划 → 计算物化依赖（内层先于外层；上下文区域里的调用先物化）→
//! 按拓扑序逐计划生成参数元组查询与缓存子查询文本，最后只把**最外层**调用点
//! 替换回语句（嵌套实参随外层文本一并下沉）。

use crate::error::EngineError;
use crate::sql::sqlfn::lower::{arg_spans, lower_spans};
use crate::sql::sqlfn::region::{block_context, context_region, distinct_query, rewrite_sql_text};
use crate::sql::sqlfn::{CallPlan, FnSite, Occurrence};
use crate::sql::VarScope;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::{Token, TokenWithSpan};
use std::collections::{BTreeMap, BTreeSet};

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
    let mut occ = collect_occurrences(sql_text, &tokens, routine_names)?;
    if occ.is_empty() {
        return Ok((sql_text.to_string(), Vec::new()));
    }
    let region = context_region(sql_text, &tokens)?;

    // 去重：同 name + 同实参文本只物化一次
    let mut plans: Vec<CallPlan> = Vec::new();
    let mut dedup: BTreeMap<String, usize> = BTreeMap::new();
    for o in occ.iter_mut() {
        let key = format!("{}\u{0}{}", o.name, o.args_key);
        let p = match dedup.get(&key) {
            Some(&i) => i,
            None => {
                dedup.insert(key.clone(), plans.len());
                plans.push(CallPlan {
                    name: o.name.clone(),
                    args: o.args.clone(),
                    arg_spans: o.arg_spans.clone(),
                    occ_span: (o.start, o.end),
                    lowered_args: Vec::new(),
                    distinct_sql: String::new(),
                    binds: Vec::new(),
                    sub_orig: String::new(),
                    depends_on: Vec::new(),
                });
                plans.len() - 1
            }
        };
        o.plan = p;
    }
    let deps = materialize_deps(sql_text, &tokens, &occ, &plans);
    for (i, d) in deps.into_iter().enumerate() {
        plans[i].depends_on = d;
    }
    let order = topo_order(&plans)?;

    for &i in &order {
        plans[i].lowered_args = lowered_args(sql_text, i, &plans, &occ)?;
        let tuple_orig = distinct_query(sql_text, &tokens, &region, &plans, &occ, i)?;
        let unit = rewrite_sql_text(&tuple_orig, var_names, funcs)?;
        plans[i].distinct_sql = unit.text;
        plans[i].binds = unit.binds;
        plans[i].sub_orig = cache_subquery(i, &plans[i].lowered_args);
    }

    // 语句文本只替换最外层调用点：嵌套调用已包含在外层文本里一并消失
    let mut spans: Vec<(usize, usize, usize)> = occ
        .iter()
        .filter(|o| o.parent.is_none())
        .map(|o| (o.start, o.end, o.plan))
        .collect();
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

/// 递归收集调用点：外层调用及其实参内的嵌套调用各一条，`parent` 指向外层。
pub(super) fn collect_occurrences(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    routine_names: &BTreeSet<String>,
) -> Result<Vec<Occurrence>, EngineError> {
    let mut occ = Vec::new();
    scan_calls(
        sql_text,
        tokens,
        routine_names,
        0,
        tokens.len(),
        None,
        &mut occ,
    )?;
    Ok(occ)
}

fn scan_calls(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    routine_names: &BTreeSet<String>,
    lo: usize,
    hi: usize,
    parent: Option<usize>,
    occ: &mut Vec<Occurrence>,
) -> Result<(), EngineError> {
    let mut i = lo;
    while i < hi {
        if let Some((name, head, start)) = call_candidate(sql_text, tokens, i, hi, routine_names) {
            if matches!(tokens.get(head + 1), Some(t) if matches!(t.token, Token::LParen)) {
                let close = matching_rparen(tokens, head + 1)?;
                let spans = arg_spans(sql_text, tokens, head + 1, close);
                let args: Vec<String> = spans
                    .iter()
                    .map(|(s, e)| sql_text[*s..*e].to_string())
                    .collect();
                let self_idx = occ.len();
                occ.push(Occurrence {
                    start,
                    end: tokens[close].span.end,
                    name,
                    args_key: args.join("\u{0}"),
                    args,
                    arg_spans: spans,
                    parent,
                    plan: usize::MAX,
                });
                // 实参区间内的调用点：单独成点，按区间就地下沉到本调用的元组查询
                scan_calls(
                    sql_text,
                    tokens,
                    routine_names,
                    head + 2,
                    close,
                    Some(self_idx),
                    occ,
                )?;
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    Ok(())
}

/// 本 token 起是否为目录内例程的调用名：返回（目录键, 末段下标, 语句字节起点）。
///
/// 点号全名优先（`package.procedure` 是目录键）；全名未命中而末段裸名命中时，
/// 整段（含包前缀）一起下沉——否则替换后残留 `pkg.` 前缀，语句直接不可解析。
fn call_candidate(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    i: usize,
    hi: usize,
    routine_names: &BTreeSet<String>,
) -> Option<(String, usize, usize)> {
    if !identish(tokens, i) {
        return None;
    }
    let (name, head) = match qualified_name(tokens, i, hi) {
        Some(last) => {
            let q = parts(sql_text, tokens, i, last);
            if routine_names.contains(&q) {
                (q, last)
            } else {
                (word(sql_text, tokens, last), last)
            }
        }
        None => (word(sql_text, tokens, i), i),
    };
    routine_names
        .contains(&name)
        .then_some((name, head, tokens[i].span.start))
}

fn identish(tokens: &[TokenWithSpan], k: usize) -> bool {
    matches!(
        tokens.get(k),
        Some(t) if matches!(t.token, Token::Ident(_) | Token::Keyword(_) | Token::QuotedIdent(_))
    )
}

/// 从 `i` 起连续 `Ident . Ident` 序列的末段下标；单段返回 `None`。
fn qualified_name(tokens: &[TokenWithSpan], i: usize, hi: usize) -> Option<usize> {
    let mut last = i;
    let mut k = i + 1;
    while k + 1 < hi
        && matches!(tokens.get(k), Some(t) if matches!(t.token, Token::Dot))
        && identish(tokens, k + 1)
    {
        last = k + 1;
        k += 2;
    }
    (last != i).then_some(last)
}

/// `tokens[i..=last]` 的点号全名（小写）；下标由 [`qualified_name`] 保证连续。
fn parts(sql_text: &str, tokens: &[TokenWithSpan], i: usize, last: usize) -> String {
    let mut out = Vec::new();
    let mut k = i;
    while k <= last {
        out.push(word(sql_text, tokens, k));
        k += if k < last { 2 } else { 1 };
    }
    out.join(".")
}

/// token 文本（小写）。
fn word(sql_text: &str, tokens: &[TokenWithSpan], k: usize) -> String {
    sql_text[tokens[k].span.start..tokens[k].span.end].to_ascii_lowercase()
}

pub(super) fn matching_rparen(
    tokens: &[TokenWithSpan],
    open_idx: usize,
) -> Result<usize, EngineError> {
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

/// 物化依赖（按计划）：`plans[i]` 依赖 `plans[j]` 表示 j 必须先物化。
///
/// 两类来源：
/// - 嵌套：外层调用的实参区间内含内层调用点；
/// - 上下文：本调用点所在查询块的取材区域（FROM 段 + WHERE）内含其他调用点
///   ——元组查询会把它们替换成各自的缓存子查询，故缓存表必须先建。
///   祖先调用点除外（方向相反，否则会与本语句的嵌套依赖拼成假环）。
fn materialize_deps(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    occs: &[Occurrence],
    plans: &[CallPlan],
) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); plans.len()];
    for o in occs.iter() {
        if let Some(p) = o.parent {
            // 外层依赖内层：内层的缓存表是外层元组查询的取材来源
            push_dep(&mut out, occs[p].plan, o.plan);
        }
        for (rs, re) in block_context(sql_text, tokens, o.start).regions() {
            for other in occs.iter().filter(|c| c.start >= rs && c.end <= re) {
                if other.start <= o.start && o.end <= other.end {
                    continue;
                }
                push_dep(&mut out, o.plan, other.plan);
            }
        }
    }
    out
}

fn push_dep(deps: &mut [Vec<usize>], from: usize, on: usize) {
    if from != on && !deps[from].contains(&on) {
        deps[from].push(on);
        deps[from].sort_unstable();
    }
}

/// 实参下沉文本：把各实参区间内的其他调用点替换为其缓存子查询。
fn lowered_args(
    sql_text: &str,
    site: usize,
    plans: &[CallPlan],
    occs: &[Occurrence],
) -> Result<Vec<String>, EngineError> {
    let mut out = Vec::with_capacity(plans[site].arg_spans.len());
    for (s, e) in &plans[site].arg_spans {
        out.push(lower_spans(sql_text, *s, *e, site, plans, occs)?);
    }
    Ok(out)
}

/// 参数缓存相关子查询（原文形态；随语句一起改写变量绑定）。
///
/// 列名与表名同用 `sd_` 保留前缀：下沉产物要再经一次变量绑定改写，裸 `v`/`a0`
/// 会命中同名 PL 变量（语料 `DECLARE v …` 常见）并被替换成绑定占位符。
fn cache_subquery(site: usize, args: &[String]) -> String {
    let cond: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(j, a)| format!("sd_a{j} IS NOT DISTINCT FROM ({a})"))
        .collect();
    format!(
        "(SELECT sd_v FROM sd_fn{site} WHERE {})",
        cond.join(" AND ")
    )
}

/// 拓扑排序：嵌套/上下文依赖的调用先物化；互相引用 → 显式拒绝。
pub(super) fn topo_order(plans: &[CallPlan]) -> Result<Vec<usize>, EngineError> {
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
