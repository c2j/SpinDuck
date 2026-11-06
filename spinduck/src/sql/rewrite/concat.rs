//! `[patterns]` concat_null_as_empty：A 模式 `||` 的「NULL 视作空串」语义。
//!
//! openGauss 5.0.0（A 模式）实测 `SELECT 'A' || NULL` → `'A'`、`1 || 2` → `'12'`，
//! DuckDB 的 `||` 传播 NULL（语料 292 处 `||`，其中日志/错误串大量拼接可能为 NULL 的
//! OUT 参与量）。改写为内核 `concat(...)`：concat 忽略 NULL 参数，只要有一个非 NULL
//! 实参就得非 NULL 结果；**全 NULL 仍得 NULL**，与基线 `NULL || NULL = NULL` 一致
//! （形态断言 `string_concat` 实测，validation V21）。
//!
//! 只做**局部文本替换**（不回解析整句、不重排格式）：改写产物除被拼接的表达式外
//! 与原文逐字节相同，避免 `||` 高频形态下的格式漂移风险。

use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::{Token, TokenWithSpan};

/// 文本内 `||` 拼接链 → `concat(...)`；无法识别操作数形态时保持原文。
pub fn rewrite_concat(text: &str) -> String {
    if !text.contains("||") {
        return text.to_string();
    }
    let Ok(toks) = Tokenizer::new(text).tokenize() else {
        return text.to_string();
    };
    let spans = chain_spans(text, &toks);
    if spans.is_empty() {
        return text.to_string();
    }
    splice(text, &spans)
}

/// 一条链的替换区间：(起, 止, 替换文本)。
type Span = (usize, usize, String);

fn chain_spans(text: &str, toks: &[TokenWithSpan]) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let Some(first_end) = primary_end(text, toks, i) else {
            i += 1;
            continue;
        };
        if !is_concat(toks.get(first_end)) {
            // 只前进一步：主元**内部**（函数实参、括号组）还可能藏着另一条链
            i += 1;
            continue;
        }
        let (tail, operands) = collect_chain(text, toks, i, first_end);
        if operands.len() < 2 {
            i += 1;
            continue;
        }
        let args: Vec<String> = operands.iter().map(|o| rewrite_concat(o)).collect();
        out.push((
            toks[i].span.start,
            toks[tail - 1].span.end,
            format!("concat({})", args.join(", ")),
        ));
        i = tail;
    }
    out
}

/// 从首个操作数（起于 `first`，止于 `first_end`）向右收集 `||` 链。
/// 返回 (链尾后一下标, 操作数文本)；操作数无法识别时只带回已解析的部分。
fn collect_chain(
    text: &str,
    toks: &[TokenWithSpan],
    first: usize,
    first_end: usize,
) -> (usize, Vec<String>) {
    let mut operands = vec![slice(text, toks, first, first_end)];
    let mut op_at = first_end;
    let mut cursor = first_end;
    loop {
        let Some(next) = primary_end(text, toks, op_at + 1) else {
            return (cursor, operands);
        };
        operands.push(slice(text, toks, op_at + 1, next));
        cursor = next;
        if !is_concat(toks.get(next)) {
            return (cursor, operands);
        }
        op_at = next;
    }
}

fn slice(text: &str, toks: &[TokenWithSpan], from: usize, to: usize) -> String {
    let start = toks[from].span.start;
    let end = toks.get(to - 1).map_or(text.len(), |t| t.span.end);
    text[start..end].to_string()
}

fn is_concat(tws: Option<&TokenWithSpan>) -> bool {
    match tws.map(|t| &t.token) {
        Some(Token::OpConcat) => true,
        Some(Token::Op(s)) => s == "||",
        _ => false,
    }
}

/// 关键字在原文里的大写文本（不依赖 Keyword 枚举的变体命名）。
fn word(text: &str, tws: &TokenWithSpan) -> String {
    text[tws.span.start..tws.span.end].to_ascii_uppercase()
}

/// 从 `i` 起解析一个主元表达式，返回其后一个 token 下标；形态不认识则返回 None。
fn primary_end(text: &str, toks: &[TokenWithSpan], i: usize) -> Option<usize> {
    let tws = toks.get(i)?;
    let j = match &tws.token {
        Token::Integer(_)
        | Token::Float(_)
        | Token::StringLiteral(_)
        | Token::NationalString(_)
        | Token::EscapeString(_)
        | Token::BitString(_)
        | Token::HexString(_)
        | Token::DollarString { .. }
        | Token::Param(_)
        | Token::JdbcParam
        | Token::MyBatisParam(_)
        | Token::MyBatisRawExpr(_)
        | Token::SetIdent(_) => i + 1,
        Token::Ident(_) | Token::QuotedIdent(_) => qualified_name(toks, i),
        Token::LParen => match_paren(toks, i)?,
        Token::Minus | Token::Plus => primary_end(text, toks, i + 1)?,
        Token::Keyword(_) => keyword_primary(text, toks, i)?,
        _ => return None,
    };
    maybe_typecast(toks, j)
}

/// `a.b.c` 限定名；紧跟 `(` 时为函数调用（含 `f(x)` 的括号配对）。
fn qualified_name(toks: &[TokenWithSpan], i: usize) -> usize {
    let mut j = i + 1;
    while matches!(toks.get(j).map(|t| &t.token), Some(Token::Dot))
        && matches!(
            toks.get(j + 1).map(|t| &t.token),
            // 限定名右段可以是关键字（`d.name`、`t.type` 在词法层是 Keyword）
            Some(Token::Ident(_) | Token::QuotedIdent(_) | Token::Star | Token::Keyword(_))
        )
    {
        j += 2;
    }
    if matches!(toks.get(j).map(|t| &t.token), Some(Token::LParen)) {
        return match_paren(toks, j).unwrap_or(j);
    }
    j
}

/// 主元位置上的关键字形态：常量、`CASE…END`、`CAST(…)` 等括号关键字函数。
fn keyword_primary(text: &str, toks: &[TokenWithSpan], i: usize) -> Option<usize> {
    let w = word(text, &toks[i]);
    match w.as_str() {
        "NULL" | "TRUE" | "FALSE" => Some(i + 1),
        "CASE" => match_case_end(text, toks, i),
        "CAST" | "EXTRACT" | "TRIM" | "SUBSTRING" | "POSITION" | "OVERLAY" | "INTERVAL"
            if matches!(toks.get(i + 1).map(|t| &t.token), Some(Token::LParen)) =>
        {
            match_paren(toks, i + 1)
        }
        _ => None,
    }
}

/// `::type` 后缀（如 `x::varchar`、`x::decimal(18,2)`）并入主元。
fn maybe_typecast(toks: &[TokenWithSpan], mut j: usize) -> Option<usize> {
    while matches!(toks.get(j).map(|t| &t.token), Some(Token::Typecast)) {
        j += 1;
        match toks.get(j).map(|t| &t.token) {
            Some(Token::Ident(_)) | Some(Token::QuotedIdent(_)) | Some(Token::Keyword(_)) => {
                j += 1;
            }
            _ => return None,
        }
        if matches!(toks.get(j).map(|t| &t.token), Some(Token::LParen)) {
            j = match_paren(toks, j)?;
        }
    }
    Some(j)
}

/// 括号配对：`open` 处为 `(`，返回匹配 `)` 的**后一个**下标。
fn match_paren(toks: &[TokenWithSpan], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut k = open;
    while k < toks.len() {
        match toks[k].token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// `CASE … END`：按 CASE/END 计数配对，返回 END 后一个下标。
fn match_case_end(text: &str, toks: &[TokenWithSpan], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, tok) in toks.iter().enumerate().skip(start) {
        match word(text, tok).as_str() {
            "CASE" => depth += 1,
            "END" => {
                depth -= 1;
                if depth <= 0 {
                    return Some(k + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// 原文切片替换（与 `patterns.rs` 同口径；区间须升序不重叠）。
fn splice(text: &str, spans: &[Span]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (s, e, rep) in spans {
        out.push_str(&text[cursor..*s]);
        out.push_str(rep);
        cursor = *e;
    }
    out.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_rewrite_concat_of_column_and_literal_to_concat_call() {
        assert_eq!(
            rewrite_concat("SELECT 'A' || s FROM t_form WHERE id = 3"),
            "SELECT concat('A', s) FROM t_form WHERE id = 3"
        );
    }

    #[test]
    fn should_fold_multi_operand_chain_into_single_concat() {
        assert_eq!(
            rewrite_concat("SELECT 'rows=' || n || ',amt=' || s FROM t"),
            "SELECT concat('rows=', n, ',amt=', s) FROM t"
        );
    }

    #[test]
    fn should_not_touch_concat_inside_string_literal() {
        assert_eq!(
            rewrite_concat("SELECT 'a||b' FROM t WHERE s = 'x||y'"),
            "SELECT 'a||b' FROM t WHERE s = 'x||y'"
        );
    }

    #[test]
    fn should_include_parenthesised_and_qualified_and_call_operands() {
        assert_eq!(
            rewrite_concat("SELECT (a + 1) || f(x, y) || d.name FROM t"),
            "SELECT concat((a + 1), f(x, y), d.name) FROM t"
        );
    }

    #[test]
    fn should_stop_operand_at_comparison_boundary() {
        assert_eq!(
            rewrite_concat("SELECT a || 'x' = 'ab' FROM t WHERE s || 'z' = 'q'"),
            "SELECT concat(a, 'x') = 'ab' FROM t WHERE concat(s, 'z') = 'q'"
        );
    }

    #[test]
    fn should_rewrite_concat_nested_inside_call_arguments() {
        assert_eq!(
            rewrite_concat("SELECT SUBSTR(a || b, 1, 2) FROM t"),
            "SELECT SUBSTR(concat(a, b), 1, 2) FROM t"
        );
    }

    #[test]
    fn should_keep_text_when_operand_is_unparsable() {
        // 链尾缺失操作数（截断文本）：不改写，交给内核给出明确错误
        assert_eq!(rewrite_concat("SELECT a ||"), "SELECT a ||");
        assert_eq!(rewrite_concat("SELECT 1 FROM t"), "SELECT 1 FROM t");
    }

    #[test]
    fn should_handle_case_when_and_loop_log_form() {
        assert_eq!(
            rewrite_concat("UPDATE t SET msg = '组合申购赎回ETF子过程调用错误:' || p_o_succeed"),
            "UPDATE t SET msg = concat('组合申购赎回ETF子过程调用错误:', p_o_succeed)"
        );
    }

    #[test]
    fn should_fold_numeric_concat_operands_into_concat() {
        assert_eq!(
            rewrite_concat("SELECT 1 || 2 FROM t"),
            "SELECT concat(1, 2) FROM t"
        );
    }
}
