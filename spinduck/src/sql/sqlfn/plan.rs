//! 调用点扫描与下沉计划构造（自 `sqlfn.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::sql::sqlfn::region::{context_region, distinct_query, rewrite_sql_text};
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

pub(super) fn collect_occurrences(
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

pub(super) fn reject_nested(
    plans: &[CallPlan],
    routine_names: &BTreeSet<String>,
) -> Result<(), EngineError> {
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
