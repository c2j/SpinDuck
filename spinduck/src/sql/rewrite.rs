//! SQL 改写（M3）：token 级 PL 变量替换为命名占位符 `$v_<名>`。
//! 基底是 SqlStatement 自带的原始 sql_text（Token::StringLiteral 是独立
//! token，字符串内不会被误替换）；方言函数映射自 M3.5 接入本管线。

use crate::error::EngineError;
use crate::ir::{BindSrc, VarName};
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::Token;
use std::collections::{BTreeMap, BTreeSet};

/// 改写产物：占位符化 SQL 文本 + 按名绑定表（同名消重，保序）。
#[derive(Debug, Clone, PartialEq)]
pub struct SqlUnit {
    pub text: String,
    pub binds: Vec<(String, SqlBindSrc)>,
    /// SQL 上下文例程调用下沉计划（`sqlfn::plan_sites` 填充；默认空）
    pub fn_sites: Vec<crate::sql::FnSite>,
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
    let mut binds: Vec<(String, SqlBindSrc)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = 0usize;
    // 限定名保护：前一个 token 为 '.' 时当前 Ident 是限定名的后半段
    // （table.column / package.routine），不是 PL 变量，禁止替换
    let mut prev_was_dot = false;
    for tws in &tokens {
        let gap = sql_text
            .get(cursor..tws.span.start)
            .ok_or_else(|| EngineError::invariant(41, "token span out of order".to_string()))?;
        out.push_str(gap);
        match &tws.token {
            Token::Ident(value) => {
                let lower = value.to_ascii_lowercase();
                if var_names.contains(&lower) && !prev_was_dot {
                    let ph = format!("$v_{lower}");
                    out.push_str(&ph);
                    if seen.insert(ph.clone()) {
                        binds.push((ph, SqlBindSrc::Var(VarName(lower))));
                    }
                } else {
                    out.push_str(&sql_text[tws.span.start..tws.span.end]);
                }
            }
            Token::Dot => {
                out.push_str(&sql_text[tws.span.start..tws.span.end]);
                prev_was_dot = true;
                cursor = tws.span.end;
                continue;
            }
            _ => out.push_str(&sql_text[tws.span.start..tws.span.end]),
        }
        prev_was_dot = false;
        cursor = tws.span.end;
    }
    out.push_str(&sql_text[cursor..]);
    Ok(SqlUnit {
        text: out,
        binds,
        fn_sites: Vec::new(),
    })
}

/// 已知 PL 变量名集合（编译期预扫描收集：赋值目标 / PL 表达式引用）。
pub type VarScope = BTreeSet<String>;

/// SQL 单元绑定源（见 [`crate::ir::BindSrc`]；重导出避免调用方跨模块拼写）。
pub type SqlBindSrc = crate::ir::BindSrc;

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
        assert_eq!(u.binds[0].1, SqlBindSrc::Var(VarName("p_i_date".into())));
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
    fn should_not_substitute_qualified_name_parts_sharing_a_var_name() {
        // 回归（语料 FNC_GET_STOCK_KIND）：局部变量与列同名时，
        // 限定名 `t.stock_kind` 的列部分曾被替换为占位符 → 内核绑定错误
        let u = rewrite_tokens(
            "SELECT t.stock_kind FROM dic_stock_kind_sub t WHERE x = stock_kind",
            &vars(&["stock_kind"]),
        )
        .unwrap();
        assert_eq!(
            u.text,
            "SELECT t.stock_kind FROM dic_stock_kind_sub t WHERE x = $v_stock_kind"
        );
        assert_eq!(u.binds.len(), 1);
        assert_eq!(u.binds[0].1, SqlBindSrc::Var(VarName("stock_kind".into())));
    }

    #[test]
    fn should_resolve_placeholder_names_case_insensitively() {
        let u = rewrite_tokens("SELECT * FROM t WHERE d = P_I_DATE", &vars(&["p_i_date"])).unwrap();
        assert_eq!(u.binds[0].1, SqlBindSrc::Var(VarName("p_i_date".into())));
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
    let mut binds: Vec<(String, SqlBindSrc)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cursor = 0usize;
    // 限定名保护：前一个 token 为 '.' 时当前 Ident 是限定名的后半段
    // （table.column / package.routine），不是 PL 变量，禁止替换；
    // 例外：记录变量的限定字段（`rec.field`，rec ∈ 变量集）必须替换为字段绑定
    let mut prev_was_dot = false;
    let mut i = 0usize;
    while i < tokens.len() {
        let tws = &tokens[i];
        let gap = sql_text
            .get(cursor..tws.span.start)
            .ok_or_else(|| EngineError::invariant(41, "token span out of order".to_string()))?;
        out.push_str(gap);
        match &tws.token {
            Token::Ident(value) => {
                let lower = value.to_ascii_lowercase();
                if var_names.contains(&lower) && !prev_was_dot {
                    // 前瞻 `rec.field`：变量 + '.' + 标识符 → 记录字段绑定
                    if let (Some(dot), Some(field_tok)) = (tokens.get(i + 1), tokens.get(i + 2)) {
                        if matches!(dot.token, Token::Dot)
                            && matches!(field_tok.token, Token::Ident(_))
                        {
                            let Token::Ident(field) = &field_tok.token else {
                                unreachable!("上方已匹配 Ident");
                            };
                            let ph = BindSrc::placeholder(&lower, Some(field));
                            out.push_str(&ph);
                            if seen.insert(ph.clone()) {
                                binds.push((
                                    ph,
                                    SqlBindSrc::Field {
                                        rec: VarName(lower),
                                        field: field.to_ascii_lowercase(),
                                    },
                                ));
                            }
                            cursor = field_tok.span.end;
                            i += 3;
                            prev_was_dot = false;
                            continue;
                        }
                    }
                    let ph = BindSrc::placeholder(&lower, None);
                    out.push_str(&ph);
                    if seen.insert(ph.clone()) {
                        binds.push((ph, SqlBindSrc::Var(VarName(lower))));
                    }
                    cursor = tws.span.end;
                    i += 1;
                    continue;
                }
                apply_rename(&mut out, sql_text, tws.span.start, tws.span.end, functions);
            }
            // 关键字（如 SYSDATE）走 Keyword 变体，用源码切片做重命名判定
            Token::Keyword(_) => {
                apply_rename(&mut out, sql_text, tws.span.start, tws.span.end, functions);
            }
            Token::Dot => {
                out.push_str(&sql_text[tws.span.start..tws.span.end]);
                prev_was_dot = true;
                cursor = tws.span.end;
                i += 1;
                continue;
            }
            _ => out.push_str(&sql_text[tws.span.start..tws.span.end]),
        }
        prev_was_dot = false;
        cursor = tws.span.end;
        i += 1;
    }
    out.push_str(&sql_text[cursor..]);
    Ok(SqlUnit {
        text: out,
        binds,
        fn_sites: Vec::new(),
    })
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
                    // 名字与 '(' 之间允许空白（格式化器产出 `decode (` 形态）
                    let mut k = i;
                    while k < text.len() && (bytes[k] as char).is_whitespace() {
                        k += 1;
                    }
                    if names.contains(&upper.as_str()) && k < text.len() && bytes[k] == b'(' {
                        // 配平右括号
                        let mut depth = 0usize;
                        let mut j = k;
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
                        // 括号未配平（含未闭合字符串吞掉右括号）→ 不识别该调用，
                        // 交由下游显式报错；不得产出越界区间（回归守护见
                        // should_not_panic_on_unterminated_string）
                        if depth == 0 && j < text.len() {
                            let args = &text[k + 1..j];
                            let has_nested = names.iter().any(|n| upper_eq_find(args, n));
                            if !has_nested {
                                out.push((start, j + 1, upper, args.to_string()));
                            }
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
pub(crate) fn split_top_level_commas(s: &str) -> Vec<String> {
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
    // Oracle 格式令牌大小写不敏感（语料混用 'YYYYMMDD'/'yyyymmdd'）
    let mut out = fmt.to_ascii_uppercase();
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

/// `UPDATE <table> <alias> SET <alias>.c = …` → 去别名：
/// `UPDATE <table> SET c = …`，并把语句内 `<alias>.` 限定同步去除。
/// DuckDB 不支持 UPDATE .. SET 的限定列名（[patterns] update_strip_alias）。
pub fn strip_update_alias(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.to_ascii_uppercase().starts_with("UPDATE ") {
        return text.to_string();
    }
    let Ok(tokens) = Tokenizer::new(trimmed).tokenize() else {
        return text.to_string();
    };
    // 头部：UPDATE <table> [alias] SET …
    let mut header: Vec<usize> = Vec::new(); // token indexes of update/table/alias
    let mut set_idx = None;
    let mut depth = 0i32;
    for (i, tws) in tokens.iter().enumerate() {
        match tws.token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            _ if depth == 0 => {
                let w = trimmed[tws.span.start..tws.span.end].to_ascii_lowercase();
                if w == "set" {
                    set_idx = Some(i);
                    break;
                }
                header.push(i);
            }
            _ => {}
        }
    }
    let Some(set_idx) = set_idx else {
        return text.to_string();
    };
    if header.len() < 2 {
        return text.to_string();
    }
    // header[2] 存在且不是 SET → 别名
    let alias_idx = if header.len() >= 3 {
        Some(header[2])
    } else {
        None
    };
    let Some(alias_idx) = alias_idx else {
        return trimmed.to_string();
    };
    let alias = trimmed[tokens[alias_idx].span.start..tokens[alias_idx].span.end]
        .trim_matches('"')
        .to_string();
    let mut out = String::with_capacity(trimmed.len());
    let mut cursor = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let tws = &tokens[i];
        // 跳过别名定义本身（`UPDATE tbl t SET` 的第二个词）
        if i == alias_idx {
            out.push_str(&trimmed[cursor..tws.span.start]);
            cursor = tws.span.end;
            i += 1;
            continue;
        }
        // 跳过 `alias .`（限定前缀；点号前后空白一并交给后续切片保留）
        let is_alias_word = trimmed[tws.span.start..tws.span.end].trim_matches('"') == alias
            && matches!(tws.token, Token::Ident(_) | Token::QuotedIdent(_));
        if is_alias_word && i + 1 < tokens.len() && matches!(tokens[i + 1].token, Token::Dot) {
            out.push_str(&trimmed[cursor..tws.span.start]);
            cursor = tokens[i + 1].span.end;
            i += 2;
            continue;
        }
        i += 1;
    }
    out.push_str(&trimmed[cursor..]);
    let _ = set_idx;
    out
}

/// 解析级变换（文本拼接实现；AST 化为 M8+ 优化路径）：
/// DECODE(a,k1,v1,...,def) → CASE a WHEN k1 THEN v1 ... [ELSE def] END
/// TO_NUMBER(x) → CAST(x AS DECIMAL(38,6))
/// TO_CHAR(x,'fmt') → strftime(<x|strptime(d,fmt_in)>, fmt_out)
pub fn apply_ast_transforms(text: &str) -> Result<String, EngineError> {
    let names = ["DECODE", "TO_NUMBER", "TO_CHAR", "TO_DATE", "INTERVALTONUM"];
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
            "TO_DATE" => {
                // [functions] TO_DATE=@to_date：A 模式内建，DuckDB 缺失（V6）
                if args.len() != 2 {
                    return Err(EngineError::business(134, "TO_DATE requires 2 arguments"));
                }
                format!(
                    "strptime({}, '{}')",
                    args[0],
                    requote(&oracle_format_to_strftime(&unquote(&args[1])))
                )
            }
            "INTERVALTONUM" => {
                // [functions] INTERVALTONUM=@intertonum：A 模式日期差（天）。
                // 参数形态为 `日期A - 日期B`（语料 FNC_GET_PURCHASE_JS_DAYS）；
                // DuckDB 侧 date - date 原生得天数，这里显式 date_diff 保证可执行
                if args.len() != 1 {
                    return Err(EngineError::business(
                        135,
                        "INTERVALTONUM requires 1 argument",
                    ));
                }
                // Bin 渲染会包一层括号：剥掉整体包裹括号后按顶层 `-` 拆分
                let mut expr = args[0].trim().to_string();
                if expr.starts_with('(') && expr.ends_with(')') {
                    let inner = expr[1..expr.len() - 1].trim();
                    if split_top_level_minus(inner).len() == 2 {
                        expr = inner.to_string();
                    }
                }
                let parts = split_top_level_minus(&expr);
                if parts.len() != 2 {
                    return Err(EngineError::business(
                        135,
                        "INTERVALTONUM expects `<date_a> - <date_b>`",
                    ));
                }
                format!("date_diff('day', {}, {})", parts[1], parts[0])
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
                                requote(&oracle_format_to_strftime(&unquote(&dargs[1])))
                            );
                            format!("{}{}{}", &inner_expr[..s0], sp, &inner_expr[e0..])
                        }
                        None => inner_expr.to_string(),
                    };
                    format!("strftime({}, '{}')", inner, requote(&fmt_out))
                }
            }
            _ => unreachable!("names list"),
        };
        t = format!("{}{}{}", &t[..start], replaced, &t[end..]);
    }
    Ok(t)
}

/// 按顶层 `-`（非括号内/非字符串字面量内）拆分为两段；不是恰好一段时返回空。
fn split_top_level_minus(text: &str) -> Vec<String> {
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
fn requote(s: &str) -> String {
    s.replace('\'', "''")
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
    fn should_requote_embedded_format_mask() {
        // 回归守护：掩码内含单引号字面量时，unquote 后必须 requote 再嵌入，
        // 否则产出畸形 SQL（''y' 曾还原为 'y' 直接进入 strftime 字面量）
        // 掩码 x'y 经大写化后字面量 ' 必须以 '' 转义嵌入（requote），
        // 否则 'X'Y' 会让字符串提前闭合（畸形 SQL）
        assert_eq!(
            apply_ast_transforms("to_char(d, 'x''y')").unwrap(),
            "strftime(d, 'X''Y')"
        );
    }

    #[test]
    fn should_not_panic_on_unterminated_string() {
        // 未闭合字符串吞掉右括号：不得越界 panic，原样返回交由下游报错
        let out = apply_ast_transforms("to_char(d, 'x''y'yyyymmdd')").unwrap();
        assert_eq!(out, "to_char(d, 'x''y'yyyymmdd')");
    }

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
    fn should_transform_intertonum_with_wrapped_paren_expr() {
        // Bin 渲染带外层括号：intertonum((strptime(..) - strptime(..))) 也要可拆
        let r = apply_ast_transforms(
            "SELECT intervaltonum((strptime('20240110', '%Y%m%d') - strptime('20240101', '%Y%m%d'))) FROM sys_dummy",
        )
        .unwrap();
        assert!(
            r.contains(
                "date_diff('day', strptime('20240101', '%Y%m%d'), strptime('20240110', '%Y%m%d'))"
            ),
            "{r}"
        );
    }

    #[test]
    fn should_transform_to_date_to_strptime() {
        let r = apply_ast_transforms("SELECT to_date(d, 'YYYYMMDD') FROM t").unwrap();
        assert_eq!(r, "SELECT strptime(d, '%Y%m%d') FROM t");
    }

    #[test]
    fn should_transform_intertonum_to_date_diff() {
        // 语料 FNC_GET_PURCHASE_JS_DAYS：A - B → date_diff('day', B, A)
        let r = apply_ast_transforms(
            "SELECT intervaltonum(to_date('20240110', 'YYYYMMDD') - to_date('20240101', 'YYYYMMDD')) FROM sys_dummy",
        )
        .unwrap();
        // 内层 to_date 先行变换，intertonum 收到的操作数已是 strptime 形态
        assert_eq!(
            r,
            "SELECT date_diff('day', strptime('20240101', '%Y%m%d'), strptime('20240110', '%Y%m%d')) FROM sys_dummy",
            "{r}"
        );
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
