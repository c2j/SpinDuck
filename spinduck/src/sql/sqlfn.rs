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

/// 为第 `site` 个调用构造参数元组查询：
/// - 上下文（FROM/WHERE）中**自身**调用所在合取项被剔除（避免自引用）；
/// - 其它调用（依赖）替换为其子查询（拓扑序保证已规划）。
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
    let head = statement_head(sql_text);
    let kw = top_level_keywords(sql_text, tokens);
    let from_seg = match head.as_str() {
        "update" => {
            let (_, u_end) = kw.get("update").expect("update kw");
            let (set_start, _) = kw.get("set").expect("set kw");
            let table = sql_text[*u_end..*set_start].trim();
            format!(" FROM {table}")
        }
        _ => {
            let (_, f_end) = kw.get("from").expect("from kw");
            let f_end2 = clause_end(&kw, "from", *f_end, sql_text);
            format!(" FROM {}", sql_text[*f_end..f_end2].trim())
        }
    };
    let where_seg = match kw.get("where") {
        Some((_, w_after)) => {
            let w_end = if head == "update" {
                sql_text.len()
            } else {
                clause_end(&kw, "where", *w_after, sql_text)
            };
            Some(non_self_where(&sql_text[*w_after..w_end], c))
        }
        None => None,
    };
    let mut q = format!("SELECT DISTINCT {args}{from_seg}");
    if let Some(w) = where_seg {
        if !w.trim().is_empty() {
            q.push_str(" WHERE ");
            q.push_str(&w);
        }
    }
    // 依赖调用已在 from/where 文本中以子查询形式存在（拓扑序）
    Ok(q)
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

fn clause_end(
    kw: &BTreeMap<String, (usize, usize)>,
    cur: &str,
    cur_end: usize,
    text: &str,
) -> usize {
    let mut end = text.len();
    for name in ["where", "group", "order", "having"] {
        if name == cur {
            continue;
        }
        if let Some((start, _)) = kw.get(name) {
            if *start > cur_end && *start < end {
                end = *start;
            }
        }
    }
    end
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
