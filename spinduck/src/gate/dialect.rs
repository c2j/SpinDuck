//! 词元层方言拒绝（Gate 2A 补充）：中缀 `MOD` 显式拒绝（BIZ-0126）。
//!
//! 参照实测（2026-11-28 openGauss 5.0.0 容器，A 模式）：中缀 `MOD` 在 SQL
//! 与 PL 里都是语法错误——`7 mod 3`、`7 MOD (3)`、`7 MOD(3)` 全部报错；
//! 尾随 `mod`（`SELECT 7 mod FROM t`）是别名不报错；`mod(a, b)` 函数形式可用。
//!
//! 拒绝点必须在**词元层**：ogsql-parser（公共 API，行为问题只记录）对中缀
//! MOD 是有损解析——`x := (v MOD 3)` / `RETURN (v MOD 3)` 的语句被整体丢弃、
//! `SELECT 7 MOD 3 INTO v` 丢成 `SELECT 7`，AST 无痕迹，IR 层无从检出
//! （守护：tests/guard/guard_pl_mod_reject.rs）。扫描规则：
//! 值操作数（标识符/数值/右括号）紧邻裸 `MOD`，且 MOD 后随值起始词元
//! （标识符/数值/`(`/`-`/参数）→ 拒绝。字符串字面量与注释（Comment/Hint）
//! 不是值词元，`QuotedIdent("MOD")` 是带引号标识符，均不误伤。

use crate::error::EngineError;
use ogsql_parser::token::{Token, TokenWithSpan};

/// 扫描词元流，命中中缀 MOD 即返回 BIZ-0126（附源位置）。
pub(crate) fn reject_infix_mod(tokens: &[TokenWithSpan]) -> Result<(), EngineError> {
    for (i, t) in tokens.iter().enumerate() {
        if !is_bare_mod(&t.token)
            || !prev_is_value_operand(tokens, i)
            || !next_starts_value(tokens, i)
        {
            continue;
        }
        return Err(EngineError::business(
            126,
            format!(
                "infix MOD is a syntax error in GaussDB (use mod(a, b) instead){}",
                t.location.format_if_real()
            ),
        ));
    }
    Ok(())
}

/// 裸 `MOD`：未加引号、大小写不敏感。
fn is_bare_mod(t: &Token) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("MOD"))
}

/// 值操作数词元（可出现在 MOD 左侧构成中缀运算）。
fn is_value_operand(t: &Token) -> bool {
    matches!(
        t,
        Token::Ident(_) | Token::Integer(_) | Token::Float(_) | Token::RParen
    )
}

/// 值起始词元（出现在 MOD 右侧说明 MOD 处于运算符位而非别名/函数名位）。
fn starts_value(t: &Token) -> bool {
    matches!(
        t,
        Token::Ident(_)
            | Token::QuotedIdent(_)
            | Token::Integer(_)
            | Token::Float(_)
            | Token::Param(_)
            | Token::LParen
            | Token::Minus
    )
}

/// MOD 左侧最近非注释词元是否值操作数；遇运算符/关键字/标点即停。
fn prev_is_value_operand(tokens: &[TokenWithSpan], i: usize) -> bool {
    for t in tokens[..i].iter().rev() {
        match &t.token {
            Token::Comment(_) | Token::Hint(_) => continue,
            tok => return is_value_operand(tok),
        }
    }
    false
}

/// MOD 右侧最近非注释词元是否值起始；到流尾视为否（尾随别名不误伤）。
fn next_starts_value(tokens: &[TokenWithSpan], i: usize) -> bool {
    for t in &tokens[i + 1..] {
        match &t.token {
            Token::Comment(_) | Token::Hint(_) => continue,
            tok => return starts_value(tok),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tws(token: Token) -> TokenWithSpan {
        TokenWithSpan {
            token,
            span: ogsql_parser::token::Span { start: 0, end: 0 },
            location: ogsql_parser::token::SourceLocation::default(),
        }
    }

    fn scan(tokens: Vec<Token>) -> Result<(), EngineError> {
        reject_infix_mod(&tokens.into_iter().map(tws).collect::<Vec<_>>())
    }

    #[test]
    fn should_reject_value_mod_value() {
        let r = scan(vec![
            Token::Integer(7),
            Token::Ident("MOD".into()),
            Token::Integer(3),
        ]);
        assert_eq!(r.unwrap_err().code.0, "BIZ-0126");
    }

    #[test]
    fn should_reject_ident_mod_ident() {
        let r = scan(vec![
            Token::Ident("v".into()),
            Token::Ident("mod".into()),
            Token::Ident("x".into()),
        ]);
        assert_eq!(r.unwrap_err().code.0, "BIZ-0126");
    }

    #[test]
    fn should_exempt_function_call_after_assign_operator() {
        // `x := mod(7, 3)`：MOD 左侧是 := 非值词元 → 放行
        assert!(scan(vec![
            Token::Ident("x".into()),
            Token::ColonEquals,
            Token::Ident("mod".into()),
            Token::LParen,
            Token::Integer(7),
        ])
        .is_ok());
    }

    #[test]
    fn should_exempt_trailing_mod_before_keyword_or_comma() {
        // `SELECT 7 mod FROM t`：参照里是别名不报错
        assert!(scan(vec![
            Token::Integer(7),
            Token::Ident("mod".into()),
            Token::Keyword(ogsql_parser::token::Keyword::FROM),
        ])
        .is_ok());
        assert!(scan(vec![
            Token::Ident("a".into()),
            Token::Ident("MOD".into()),
            Token::Comma,
        ])
        .is_ok());
    }

    #[test]
    fn should_skip_comments_when_resolving_neighbors() {
        // `7 /* c */ MOD 3`：注释隔开仍构成中缀
        let r = scan(vec![
            Token::Integer(7),
            Token::Comment(" c ".into()),
            Token::Ident("MOD".into()),
            Token::Integer(3),
        ]);
        assert_eq!(r.unwrap_err().code.0, "BIZ-0126");
    }

    #[test]
    fn should_not_treat_string_literal_as_operand() {
        // `'MOD' MOD 3`：左操作数是字符串不是值词元 → 本层放行（后续解析处置）
        assert!(scan(vec![
            Token::StringLiteral("MOD".into()),
            Token::Ident("MOD".into()),
            Token::Integer(3),
        ])
        .is_ok());
    }

    #[test]
    fn should_not_flag_quoted_mod_identifier() {
        // `"MOD" 3`：带引号标识符不是裸 MOD
        assert!(scan(vec![Token::QuotedIdent("MOD".into()), Token::Integer(3),]).is_ok());
    }
}
