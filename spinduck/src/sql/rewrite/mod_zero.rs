//! `[patterns] mod_zero_returns_dividend`：A 模式 `MOD(n, 0) = n`（Oracle 语义）。
//!
//! openGauss 5.0.0（A 模式）实测：`7 % 0` = 7、`7.5 % 0` = 7.5、`-7 % 0` = -7、
//! `0 % 0` = 0、`mod(7,0)` = 7、`mod(0,0)` = 0、PL `n := 7 % 0` = 7 无异常；
//! `7 % NULL` = NULL。DuckDB（项目内核）的 `%` / `mod()` 零除得 NULL，不改写即
//! 静默分歧。改写为操作数绑定的零模守卫：每层
//! `(SELECT CASE WHEN b = 0 THEN a ELSE a % b END FROM (SELECT a, b) t)`，
//! 操作数只出现一次——零模返回被除数，非零走内核 `%`；`b = NULL` 时 CASE
//! 条件非真 → ELSE 分支得 NULL，NULL 传播保持。BIZ-0124 只保留给 `/ 0`
//! （PL `/` 除零仍报错，V58 口径不变）。
//!
//! 改写产物**不含 `mod(` 调用**（ELSE 用 `%` 运算符）：`rewrite_sql` 在
//! patterns 之后还跑 `apply_ast_transforms`（MOD 是其变换名之一），产物含
//! `mod(` 会让 innermost 查找永不收敛——这是首版实现的实测挂死（V59.1）。
//!
//! 覆盖两条通道：
//! - 静态 SQL 语句文本（`apply_text_patterns`）：`a % b` 与 `mod(a, b)`；
//! - PL FuncCall/SqlFn 的 prepared 标量 SQL（`SELECT mod($p0, $p1)`，经
//!   `apply_ast_transforms` 的 MOD 分支）。
//!
//! PL 表达式求值（`eval_bin` → `numeric.rs`）不走文本通道，已在求值器单独对齐。
//!
//! 只做**局部文本替换**（concat 先例）：除被改写的表达式外与原文逐字节相同；
//! 递归只进入严格更小的子文本（函数实参/括号组/CASE 体），保证终止。

use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::{Token, TokenWithSpan};

use super::concat::{match_paren, primary_end, slice, splice};

/// 文本内 `%` 中缀与 `mod(a, b)` 调用 → 零模守卫形态；无法识别操作数形态时保持原文。
pub fn rewrite_mod_zero(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if !lower.contains('%') && !lower.contains("mod") {
        return text.to_string();
    }
    let Ok(toks) = Tokenizer::new(text).tokenize() else {
        return text.to_string();
    };
    let spans = mod_spans(text, &toks);
    if spans.is_empty() {
        return text.to_string();
    }
    splice(text, &spans)
}

/// 一条替换区间：(起, 止, 替换文本)，与 concat 的 Span 同构。
type Span = (usize, usize, String);

fn is_mod_ident(t: &Token) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("mod"))
}

fn is_percent(tws: Option<&TokenWithSpan>) -> bool {
    matches!(tws.map(|t| &t.token), Some(Token::Percent))
}

fn is_lparen(tws: Option<&TokenWithSpan>) -> bool {
    matches!(tws.map(|t| &t.token), Some(Token::LParen))
}

/// 扫描词元流，收集全部改写区间（按起点递增、互不重叠）。
fn mod_spans(text: &str, toks: &[TokenWithSpan]) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        // 二元 `+/-`（前一个词元是操作数结尾）：`%` 比算术运算绑得更紧，
        // 折叠区只可能是紧随其后的紧子表达式——跳过运算符继续扫，否则会把
        // `1 + (…) % 1000` 的左操作数误认成一元 `+ (…)`（FR.q3 首跑实测）
        if is_binary_plus_or_minus(text, toks, i) {
            i += 1;
            continue;
        }
        // `mod(` 函数调用：恰好两个实参 → 零守卫 CASE
        if is_mod_ident(&toks[i].token) && is_lparen(toks.get(i + 1)) {
            if let Some(sp) = mod_call_span(text, toks, i) {
                out.push(sp.span);
                i = sp.next;
                continue;
            }
        }
        let Some(first_end) = primary_end(text, toks, i) else {
            i += 1;
            continue;
        };
        if is_percent(toks.get(first_end)) {
            if let Some(sp) = chain_span(text, toks, i, first_end) {
                out.push(sp.span);
                i = sp.next;
                continue;
            }
        }
        // 非链主元：包装形态（调用/括号组/CASE）内部还可能藏着 mod()/%，递归。
        // 替换保留外壳（abs( … ) 的 abs 与括号），只换内层文本
        if first_end >= i + 2 {
            let (head_end, tail_start) = if is_lparen(toks.get(i + 1)) {
                (toks[i + 2].span.start, toks[first_end - 1].span.start)
            } else {
                (toks[i + 1].span.start, toks[first_end - 1].span.start)
            };
            if head_end < tail_start {
                let inner = &text[head_end..tail_start];
                if needs_rewrite(inner) {
                    let rewritten = rewrite_mod_zero(inner);
                    if rewritten != inner {
                        let new_text = format!(
                            "{}{}{}",
                            &text[toks[i].span.start..head_end],
                            rewritten,
                            &text[tail_start..toks[first_end - 1].span.end]
                        );
                        out.push((toks[i].span.start, toks[first_end - 1].span.end, new_text));
                    }
                }
            }
        }
        i = first_end;
    }
    out
}

/// 二元 `+/-`：词元是 Plus/Minus 且前一非注释词元是操作数结尾（一元场景的前
/// 一词元必然是运算符/关键字/左括号/行首）。
fn is_binary_plus_or_minus(_text: &str, toks: &[TokenWithSpan], i: usize) -> bool {
    if !matches!(toks[i].token, Token::Plus | Token::Minus) {
        return false;
    }
    for t in toks[..i].iter().rev() {
        match &t.token {
            Token::Comment(_) | Token::Hint(_) => continue,
            tok => {
                return matches!(
                    tok,
                    Token::Ident(_)
                        | Token::QuotedIdent(_)
                        | Token::Integer(_)
                        | Token::Float(_)
                        | Token::RParen
                );
            }
        }
    }
    false
}

/// mod_spans 的扫描游标：替换区间 + 下一个扫描位置。
struct ScanHit {
    span: Span,
    next: usize,
}

fn needs_rewrite(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains('%') || lower.contains("mod")
}

/// `mod(` 调用：顶层逗号拆出恰好两个实参才改写（否则保持原文交内核/报错）。
fn mod_call_span(text: &str, toks: &[TokenWithSpan], i: usize) -> Option<ScanHit> {
    let close = match_paren(toks, i + 1)?;
    let args = split_top_level_commas(toks, i + 2, close - 1);
    if args.len() != 2 {
        return None;
    }
    let a = range_rewritten(text, toks, args[0].0, args[0].1);
    let b = range_rewritten(text, toks, args[1].0, args[1].1);
    Some(ScanHit {
        span: (toks[i].span.start, toks[close - 1].span.end, fold(&[a, b])),
        next: close,
    })
}

/// `%` 中缀链（左结合）：`a % b % c` 折成嵌套守卫。
fn chain_span(
    text: &str,
    toks: &[TokenWithSpan],
    first: usize,
    first_end: usize,
) -> Option<ScanHit> {
    let mut operands = vec![range_rewritten(text, toks, first, first_end)];
    let mut op_at = first_end;
    let mut cursor = first_end;
    while let Some(next) = primary_end(text, toks, op_at + 1) {
        operands.push(range_rewritten(text, toks, op_at + 1, next));
        cursor = next;
        if !is_percent(toks.get(next)) {
            break;
        }
        op_at = next;
    }
    if operands.len() < 2 {
        return None;
    }
    Some(ScanHit {
        span: (
            toks[first].span.start,
            toks[cursor - 1].span.end,
            fold(&operands),
        ),
        next: cursor,
    })
}

/// 词元区间 [from, to) 的文本，并对区间内的 mod()/% 递归改写。
/// 实参/操作数文本严格小于全文（链与调用都需要后续词元），文本递归保证终止。
fn range_rewritten(text: &str, toks: &[TokenWithSpan], from: usize, to: usize) -> String {
    rewrite_mod_zero(&slice(text, toks, from, to))
}

/// `(a % b % c)` 左结合折成嵌套零模守卫（产物不含 `mod(`，见模块头注释）。
///
/// 每层用标量子查询把操作数绑定到派生表列：`acc`/`b` 各出现一次，
/// CASE 只引用别名——易变操作数（nextval/random/UDF）只求值一次，
/// 链式输出规模线性（旧实现文本双份复制 → 2^k 膨胀 + 副作用双求值，
/// 三轮复审阻断 2）。语义与 `CASE WHEN b = 0 THEN a ELSE a % b END` 等价：
/// `_b` 为 NULL 时条件非真走 ELSE，`_a % _b` 得 NULL，NULL 传播不变。
fn fold(operands: &[String]) -> String {
    let mut acc = operands[0].clone();
    for b in &operands[1..] {
        acc = format!(
            "(SELECT CASE WHEN __sp_mod_b = 0 THEN __sp_mod_a \
             ELSE __sp_mod_a % __sp_mod_b END \
             FROM (SELECT {acc} AS __sp_mod_a, {b} AS __sp_mod_b) AS __sp_mod_t)"
        );
    }
    acc
}

/// 拆 `[from, to)` 内顶层逗号分隔的实参词元区间（括号深度计）。
fn split_top_level_commas(toks: &[TokenWithSpan], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut start = from;
    let mut i = from;
    while i < to {
        match toks[i].token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                out.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < to {
        out.push((start, to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单层绑定形态（fold 输出），测试期望复用同一拼装避免长串漂移。
    fn bound(a: &str, b: &str) -> String {
        format!(
            "(SELECT CASE WHEN __sp_mod_b = 0 THEN __sp_mod_a \
             ELSE __sp_mod_a % __sp_mod_b END \
             FROM (SELECT {a} AS __sp_mod_a, {b} AS __sp_mod_b) AS __sp_mod_t)"
        )
    }

    #[test]
    fn should_rewrite_infix_percent_zero_guard() {
        assert_eq!(
            rewrite_mod_zero("SELECT 7 % 0"),
            format!("SELECT {}", bound("7", "0"))
        );
    }

    #[test]
    fn should_rewrite_mod_call() {
        assert_eq!(
            rewrite_mod_zero("SELECT mod(7, 0)"),
            format!("SELECT {}", bound("7", "0"))
        );
    }

    #[test]
    fn should_keep_nonzero_cases_semantics() {
        // 非零除数：CASE 条件不命中 → ELSE 分支，值语义不变
        assert_eq!(
            rewrite_mod_zero("SELECT 7 % 3"),
            format!("SELECT {}", bound("7", "3"))
        );
    }

    #[test]
    fn should_fold_chains_left_associative() {
        // ((a % b) % c)：内层先绑定，外层把内层整体作为左操作数再绑定一次
        let inner = bound("a", "b");
        let got = rewrite_mod_zero("SELECT a % b % c");
        assert_eq!(got, format!("SELECT {}", bound(&inner, "c")));
    }

    #[test]
    fn should_rewrite_nested_mod_inside_call_args() {
        let got = rewrite_mod_zero("SELECT abs(mod(x, 0)) FROM t");
        assert_eq!(got, format!("SELECT abs({}) FROM t", bound("x", "0")));
    }

    #[test]
    fn should_not_touch_like_percent_in_string() {
        assert_eq!(
            rewrite_mod_zero("SELECT c FROM t WHERE n LIKE 'A%'"),
            "SELECT c FROM t WHERE n LIKE 'A%'"
        );
    }

    #[test]
    fn should_keep_text_without_mod_or_percent() {
        assert_eq!(rewrite_mod_zero("SELECT 1 + 2"), "SELECT 1 + 2");
    }

    #[test]
    fn should_keep_null_operand_propagation_shape() {
        // NULL 除数：CASE 条件非真 → ELSE 分支 NULL 传播（内核 % 对 NULL 得 NULL）
        assert_eq!(
            rewrite_mod_zero("SELECT 7 % NULL"),
            format!("SELECT {}", bound("7", "NULL"))
        );
    }

    #[test]
    fn should_treat_plus_after_operand_as_binary_not_unary() {
        // `1 + (a * b) % 1000`：% 比算术紧，折叠区是紧子表达式；二元 + 不得
        // 被当成一元前缀吞进左操作数（FR.q3 首跑实测）
        assert_eq!(
            rewrite_mod_zero("SELECT 1 + (a * b) % 1000 FROM t"),
            format!("SELECT 1 + {} FROM t", bound("(a * b)", "1000"))
        );
    }

    #[test]
    fn should_handle_nested_percent_group_in_arithmetic() {
        let got = rewrite_mod_zero("SELECT (1 + (a * 7) % 1000) % 5 = 1 FROM t");
        assert!(
            got.contains(&format!("(1 + {})", bound("(a * 7)", "1000"))),
            "{got}"
        );
        assert!(got.contains("__sp_mod_b = 0"), "{got}");
    }

    #[test]
    fn should_keep_long_chain_rewrite_linear() {
        // 三轮复审阻断 2：16 操作数旧实现 acc/b 双份复制 → 2^k 膨胀（89B→1.24MB）。
        // 操作数绑定形态每层只加常数包装，输出规模线性。
        let expr = (1..=16)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" % ");
        let got = rewrite_mod_zero(&format!("SELECT {expr}"));
        assert!(got.len() < 20_000, "len={}", got.len());
    }

    #[test]
    fn should_keep_unary_minus_operand_in_chain() {
        // 行首/运算符后的 `-` 是一元：`-7 % 3` 的左操作数是 -7
        assert_eq!(
            rewrite_mod_zero("SELECT -7 % 3"),
            format!("SELECT {}", bound("-7", "3"))
        );
    }
}
