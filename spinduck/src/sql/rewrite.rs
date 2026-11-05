//! SQL 改写（M3）：token 级 PL 变量替换为命名占位符 `$v_<名>`。
//! 基底是 SqlStatement 自带的原始 sql_text（Token::StringLiteral 是独立
//! token，字符串内不会被误替换）；方言函数映射自 M3.5 接入本管线。

use crate::error::EngineError;
use crate::ir::VarName;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::Token;
use std::collections::{BTreeMap, BTreeSet};

/// 改写产物：占位符化 SQL 文本 + 按名绑定表（同名消重，保序）。
#[derive(Debug, Clone, PartialEq)]
pub struct SqlUnit {
    pub text: String,
    pub binds: Vec<(String, VarName)>,
}

/// token 级变量替换：裸标识符且命中 PL 变量集 → `$v_<小写名>`。
/// 引号标识符、字符串字面量、数字等 token 原样保留（字节切片重组）。
pub fn rewrite_tokens(
    sql_text: &str,
    var_names: &BTreeSet<String>,
) -> Result<SqlUnit, EngineError> {
    let tokens = Tokenizer::new(sql_text)
        .tokenize()
        .map_err(|e| EngineError::invariant(40, format!("tokenizer: {e}")))?;
    let mut out = String::with_capacity(sql_text.len());
    let mut binds: Vec<(String, VarName)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = 0usize;
    for tws in &tokens {
        let gap = sql_text
            .get(cursor..tws.span.start)
            .ok_or_else(|| EngineError::invariant(41, "token span out of order".to_string()))?;
        out.push_str(gap);
        match &tws.token {
            Token::Ident(value) => {
                let lower = value.to_ascii_lowercase();
                if var_names.contains(&lower) {
                    let ph = format!("$v_{lower}");
                    out.push_str(&ph);
                    if seen.insert(ph.clone()) {
                        binds.push((ph, VarName(lower)));
                    }
                } else {
                    out.push_str(&sql_text[tws.span.start..tws.span.end]);
                }
            }
            _ => out.push_str(&sql_text[tws.span.start..tws.span.end]),
        }
        cursor = tws.span.end;
    }
    out.push_str(&sql_text[cursor..]);
    Ok(SqlUnit { text: out, binds })
}

/// 已知 PL 变量名集合（编译期预扫描收集：赋值目标 / PL 表达式引用）。
pub type VarScope = BTreeSet<String>;

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn should_substitute_ident_tokens_with_named_placeholders() {
        let u = rewrite_tokens(
            "SELECT bcrq, scdm FROM mid_yjqs_detail WHERE bcrq = p_i_date AND scdm = '001'",
            &vars(&["p_i_date"]),
        )
        .unwrap();
        assert_eq!(
            u.text,
            "SELECT bcrq, scdm FROM mid_yjqs_detail WHERE bcrq = $v_p_i_date AND scdm = '001'"
        );
        assert_eq!(u.binds.len(), 1);
        assert_eq!(u.binds[0].1, VarName("p_i_date".into()));
    }

    #[test]
    fn should_dedupe_binds_for_repeated_variables() {
        let u = rewrite_tokens(
            "SELECT a FROM t WHERE bcrq = p_i_date AND end_date = p_i_date",
            &vars(&["p_i_date"]),
        )
        .unwrap();
        assert_eq!(u.text.matches("$v_p_i_date").count(), 2);
        assert_eq!(u.binds.len(), 1); // 同名消重，绑定一次
    }

    #[test]
    fn should_keep_string_literals_and_quoted_idents_unsubstituted() {
        let u = rewrite_tokens(
            "SELECT \"p_i_date\" FROM t WHERE note = 'p_i_date' AND x = p_i_date",
            &vars(&["p_i_date"]),
        )
        .unwrap();
        assert!(u.text.contains("'p_i_date'")); // 字符串字面量不动
        assert!(u.text.contains("\"p_i_date\"")); // 引号标识符不动
        assert_eq!(u.binds.len(), 1); // 只有裸标识符被替换
    }

    #[test]
    fn should_cast_single_arg_to_char_to_varchar() {
        // 回归（语料 BIZ-0132）：1 参 TO_CHAR 曾直接报错，语料大量使用
        let out = apply_ast_transforms("SELECT TO_CHAR(123.450) FROM dual").unwrap();
        assert_eq!(out, "SELECT CAST(123.450 AS VARCHAR) FROM dual");
    }

    #[test]
    fn should_rewrite_two_arg_to_char_with_oracle_format() {
        let out = apply_ast_transforms("SELECT TO_CHAR(d, 'YYYYMMDD') FROM dual").unwrap();
        assert!(out.contains("strftime(d, '%Y%m%d')"), "{out}");
    }

    #[test]
    fn should_resolve_placeholder_names_case_insensitively() {
        let u = rewrite_tokens("SELECT * FROM t WHERE d = P_I_DATE", &vars(&["p_i_date"])).unwrap();
        assert_eq!(u.binds[0].1, VarName("p_i_date".into()));
    }
}

// ── M3.5：规则应用器（函数映射；探针见 tests 与 probes） ──────────────

/// token 级重命名 + 命名占位符（一次遍历）。
/// `functions`：`[functions]` 映射；值以 `@` 开头的（@case/@to_number/@to_char）
/// 不在此处理，由 [`apply_ast_transforms`] 解析级完成。
pub fn rewrite_tokens_with_rules(
    sql_text: &str,
    var_names: &BTreeSet<String>,
    functions: &BTreeMap<String, String>,
) -> Result<SqlUnit, EngineError> {
    let tokens = Tokenizer::new(sql_text)
        .tokenize()
        .map_err(|e| EngineError::invariant(40, format!("tokenizer: {e}")))?;
    let mut out = String::with_capacity(sql_text.len() + 64);
    let mut binds: Vec<(String, VarName)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = 0usize;
    for tws in &tokens {
        let gap = sql_text
            .get(cursor..tws.span.start)
            .ok_or_else(|| EngineError::invariant(41, "token span out of order".to_string()))?;
        out.push_str(gap);
        match &tws.token {
            Token::Ident(value) => {
                let lower = value.to_ascii_lowercase();
                if var_names.contains(&lower) {
                    let ph = format!("$v_{lower}");
                    out.push_str(&ph);
                    if seen.insert(ph.clone()) {
                        binds.push((ph, VarName(lower)));
                    }
                    cursor = tws.span.end;
                    continue;
                }
                apply_rename(&mut out, sql_text, tws.span.start, tws.span.end, functions);
            }
            // 关键字（如 SYSDATE）走 Keyword 变体，用源码切片做重命名判定
            Token::Keyword(_) => {
                apply_rename(&mut out, sql_text, tws.span.start, tws.span.end, functions);
            }
            _ => out.push_str(&sql_text[tws.span.start..tws.span.end]),
        }
        cursor = tws.span.end;
    }
    out.push_str(&sql_text[cursor..]);
    Ok(SqlUnit { text: out, binds })
}

/// 简单重命名（同参数序；`@` 前缀的解析级变换在此跳过）。
fn apply_rename(
    out: &mut String,
    sql_text: &str,
    start: usize,
    end: usize,
    functions: &BTreeMap<String, String>,
) {
    let upper = sql_text[start..end].to_ascii_uppercase();
    if let Some(mapped) = functions.get(&upper) {
        if !mapped.starts_with('@') {
            out.push_str(mapped);
            return;
        }
    }
    out.push_str(&sql_text[start..end]);
}

/// 括号配平 + 引号感知的函数调用扫描：返回 (名称起点, 右括号终点, 名称, 参数文本)。
/// 只返回"最内层"调用（参数文本中不再包含待变换函数名），保证拼接正确。
fn find_innermost_calls(text: &str, names: &[&str]) -> Vec<(usize, usize, String, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut in_string = false;
    while i < text.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\'' {
                if i + 1 < text.len() && bytes[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_string = false;
                }
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_string = true;
                i += 1;
            }
            _ => {
                if c.is_ascii_alphabetic() || c == '_' {
                    let start = i;
                    while i < text.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                        i += 1;
                    }
                    let word = &text[start..i];
                    let upper = word.to_ascii_uppercase();
                    if names.contains(&upper.as_str()) && i < text.len() && bytes[i] == b'(' {
                        // 配平右括号
                        let mut depth = 0usize;
                        let mut j = i;
                        let mut s2 = in_string;
                        while j < text.len() {
                            let ch = bytes[j] as char;
                            if s2 {
                                if ch == '\'' {
                                    if j + 1 < text.len() && bytes[j + 1] == b'\'' {
                                        j += 1;
                                    } else {
                                        s2 = false;
                                    }
                                }
                            } else {
                                match ch {
                                    '\'' => s2 = true,
                                    '(' => depth += 1,
                                    ')' => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            j += 1;
                        }
                        let args = &text[i + 1..j];
                        let has_nested = names.iter().any(|n| upper_eq_find(args, n));
                        if !has_nested {
                            out.push((start, j + 1, upper, args.to_string()));
                        }
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    out
}

fn upper_eq_find(haystack: &str, name: &str) -> bool {
    let up = haystack.to_ascii_uppercase();
    let pat = format!("{name}(");
    up.contains(&pat)
}

/// 顶层逗号切分（括号与字符串感知）。
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut cur = String::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        let c = bytes[i] as char;
        if in_string {
            cur.push(c);
            if c == '\'' {
                if i + 1 < s.len() && bytes[i + 1] == b'\'' {
                    cur.push('\'');
                    i += 1;
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
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Oracle 日期格式串 → strftime 格式串（已知 token 最长匹配，其余原样保留）。
pub fn oracle_format_to_strftime(fmt: &str) -> String {
    let mut out = fmt.to_string();
    for (o, p) in [
        ("YYYYMMDDHH24MISS", "%Y%m%d%H%M%S"),
        ("YYYY-MM-DD", "%Y-%m-%d"),
        ("YYYYMMDD", "%Y%m%d"),
        ("HH24", "%H"),
        ("YYYY", "%Y"),
        ("MM", "%m"),
        ("DD", "%d"),
        ("MI", "%M"),
        ("SS", "%S"),
    ] {
        out = out.replace(o, p);
    }
    out
}

/// 解析级变换（文本拼接实现；AST 化为 M8+ 优化路径）：
/// DECODE(a,k1,v1,...,def) → CASE a WHEN k1 THEN v1 ... [ELSE def] END
/// TO_NUMBER(x) → CAST(x AS DECIMAL(38,6))
/// TO_CHAR(x,'fmt') → strftime(<x|strptime(d,fmt_in)>, fmt_out)
pub fn apply_ast_transforms(text: &str) -> Result<String, EngineError> {
    let names = ["DECODE", "TO_NUMBER", "TO_CHAR"];
    let mut t = text.to_string();
    loop {
        let calls = find_innermost_calls(&t, &names);
        let Some((start, end, name, args_raw)) = calls.into_iter().min_by_key(|c| c.1 - c.0) else {
            break;
        };
        let args = split_top_level_commas(&args_raw);
        let replaced = match name.as_str() {
            "DECODE" => {
                if args.len() < 3 {
                    return Err(EngineError::business(
                        130,
                        "DECODE requires at least 3 arguments",
                    ));
                }
                let mut cs = format!("CASE {} ", args[0]);
                let mut i = 1usize;
                while i + 1 < args.len() {
                    cs.push_str(&format!("WHEN {} THEN {} ", args[i], args[i + 1]));
                    i += 2;
                }
                if i < args.len() {
                    cs.push_str(&format!("ELSE {} ", args[i]));
                }
                cs.push_str("END");
                cs
            }
            "TO_NUMBER" => {
                if args.len() != 1 {
                    return Err(EngineError::business(131, "TO_NUMBER requires 1 argument"));
                }
                format!("CAST({} AS DECIMAL(38,6))", args[0])
            }
            "TO_CHAR" => {
                // 1 参形式（无数值/日期掩码）：GaussDB 按默认格式转字符串，
                // DuckDB 无对应默认格式 → CAST AS VARCHAR（已知限制：默认格式差异，
                // 见 validation.md V9 与规则文件 [functions] 注释）
                if args.len() == 1 {
                    format!("CAST({} AS VARCHAR)", args[0])
                } else if args.len() != 2 {
                    return Err(EngineError::business(
                        132,
                        "TO_CHAR requires 1 or 2 arguments",
                    ));
                } else {
                    let fmt_out = oracle_format_to_strftime(&unquote(&args[1]));
                    let inner_expr = &args[0];
                    let inner = match find_innermost_calls(inner_expr, &["TO_DATE"])
                        .into_iter()
                        .next()
                    {
                        Some((s0, e0, _, a0)) => {
                            // 原位替换 TO_DATE 调用，保留调用点前后的算术表达式（如 + v_days）
                            let dargs = split_top_level_commas(&a0);
                            if dargs.len() != 2 {
                                return Err(EngineError::business(
                                    133,
                                    "TO_DATE requires 2 arguments",
                                ));
                            }
                            let sp = format!(
                                "strptime({}, '{}')",
                                dargs[0],
                                oracle_format_to_strftime(&unquote(&dargs[1]))
                            );
                            format!("{}{}{}", &inner_expr[..s0], sp, &inner_expr[e0..])
                        }
                        None => inner_expr.to_string(),
                    };
                    format!("strftime({}, '{}')", inner, fmt_out)
                }
            }
            _ => unreachable!("names list"),
        };
        t = format!("{}{}{}", &t[..start], replaced, &t[end..]);
    }
    Ok(t)
}

fn unquote(literal: &str) -> String {
    let t = literal.trim();
    if t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2 {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    #[test]
    fn should_transform_decode_to_case_with_default() {
        let r =
            apply_ast_transforms("SELECT decode(t.bs, '1', 'B', '-1', 'S', '0') FROM t").unwrap();
        assert!(
            r.contains("CASE t.bs WHEN '1' THEN 'B' WHEN '-1' THEN 'S' ELSE '0' END"),
            "{r}"
        );
    }

    #[test]
    fn should_transform_nested_decode_innermost_first() {
        let r = apply_ast_transforms("SELECT decode(m, '001', decode(v, '0', 0, f), f) FROM t")
            .unwrap();
        assert_eq!(r.matches("CASE").count(), 2, "{r}");
        assert!(!r.to_uppercase().contains("DECODE("), "{r}");
    }

    #[test]
    fn should_not_touch_function_like_text_inside_string_literals() {
        // 字符串里的 to_char(...) 不是函数调用，必须原样保留
        let sql = "SELECT 'to_char(x, ''YYYY'')' AS s FROM t";
        let r = apply_ast_transforms(sql).unwrap();
        assert_eq!(r, sql, "{r}");
    }

    #[test]
    fn should_transform_every_occurrence_in_one_statement() {
        let r = apply_ast_transforms("SELECT to_number(a), to_number(b) FROM t").unwrap();
        assert_eq!(r.matches("CAST(").count(), 2, "{r}");
        assert!(!r.to_ascii_uppercase().contains("TO_NUMBER("), "{r}");
    }

    #[test]
    fn should_keep_commas_inside_literal_arguments() {
        // 字面量里的逗号不能当作参数分隔符
        let r = apply_ast_transforms("SELECT decode(k, ',', 'x', 'y') FROM t").unwrap();
        assert!(r.contains("WHEN ',' THEN 'x'"), "{r}");
        assert!(r.contains("ELSE 'y'"), "{r}");
    }

    #[test]
    fn should_transform_to_number_to_cast() {
        let r = apply_ast_transforms("SELECT to_number(t.bs) * 2 FROM t").unwrap();
        assert!(r.contains("CAST(t.bs AS DECIMAL(38,6))"), "{r}");
    }

    #[test]
    fn should_transform_to_char_with_oracle_format() {
        let r = apply_ast_transforms("SELECT to_char(now(), 'YYYYMMDD') FROM t").unwrap();
        assert!(r.contains("strftime(now(), '%Y%m%d')"), "{r}");
    }

    #[test]
    fn should_transform_to_char_of_to_date_with_interval() {
        let r = apply_ast_transforms(
            "SELECT to_char(to_date(d, 'YYYYMMDD') + v_days, 'YYYYMMDD') FROM t",
        )
        .unwrap();
        assert!(
            r.contains("strftime(strptime(d, '%Y%m%d') + v_days, '%Y%m%d')"),
            "{r}"
        );
    }

    #[test]
    fn should_rename_simple_functions_via_token_pass() {
        let u = rewrite_tokens_with_rules(
            "SELECT nvl(a, sysdate), substr(b, 1, 2) FROM t WHERE x = p_i_date",
            &["p_i_date".to_string()].into_iter().collect(),
            &[
                ("NVL", "COALESCE"),
                ("SYSDATE", "CURRENT_TIMESTAMP"),
                ("SUBSTR", "SUBSTRING"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        )
        .unwrap();
        assert!(
            u.text.contains("COALESCE(a, CURRENT_TIMESTAMP)"),
            "{}",
            u.text
        );
        assert!(u.text.contains("SUBSTRING(b, 1, 2)"), "{}", u.text);
        assert!(u.text.contains("$v_p_i_date"), "{}", u.text);
    }

    #[test]
    fn should_convert_oracle_format_tokens() {
        assert_eq!(oracle_format_to_strftime("YYYYMMDD"), "%Y%m%d");
        assert_eq!(
            oracle_format_to_strftime("YYYYMMDDHH24MISS"),
            "%Y%m%d%H%M%S"
        );
    }
}
