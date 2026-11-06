//! 解析级变换（DECODE/TO_* /INTERVALTONUM → DuckDB 等价写法）与 UPDATE 去别名。

use crate::error::EngineError;
use crate::sql::rewrite::scan::find_innermost_calls;
use crate::sql::rewrite::split::{requote, split_top_level_commas, split_top_level_minus, unquote};
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::Token;

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
