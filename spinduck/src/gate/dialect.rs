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
//! 值操作数（可结束值表达式的词元）紧邻裸 `MOD`，且 MOD 后随值起始词元
//! （可开始值表达式的词元）→ 拒绝。两侧词元类覆盖所有值表达式边界形态
//! （V61 三轮复审：`null mod 3` / `7 mod null` / `'x' mod 3` / `7 mod case…`
//! 曾因 Keyword 与字符串字面量未覆盖而漏检——登记成功后静默返回 NULL 或
//! 落 ENG-0022 错码）。字符串字面量与注释（Comment/Hint）不是裸 MOD，
//! `QuotedIdent("MOD")` 是带引号标识符，均不误伤；合法尾随别名
//! （`SELECT 7 mod FROM t`、`SELECT 7 mod,`）因右侧是子句关键字/逗号而放行。

use crate::error::EngineError;
use ogsql_parser::token::{Keyword, Token, TokenWithSpan};

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

/// 字面量/参数/括号类词元：两侧共用（值表达式的原子边界）。
fn is_atom(t: &Token) -> bool {
    matches!(
        t,
        Token::Ident(_)
            | Token::QuotedIdent(_)
            | Token::Integer(_)
            | Token::Float(_)
            | Token::StringLiteral(_)
            | Token::BitString(_)
            | Token::HexString(_)
            | Token::NationalString(_)
            | Token::EscapeString(_)
            | Token::DollarString { .. }
            | Token::Param(_)
            | Token::JdbcParam
            | Token::MyBatisParam(_)
            | Token::MyBatisRawExpr(_)
            | Token::SetIdent(_)
    )
}

/// 值操作数词元（可出现在 MOD 左侧构成中缀运算）——覆盖所有可**结束**
/// 值表达式的词元形态：原子词元 + 右括号/右中括号 + 值形态关键字
/// （NULL/TRUE/FALSE/CURRENT_*/END 等，见 is_value_end_keyword）。
fn is_value_operand(t: &Token) -> bool {
    is_atom(t) || matches!(t, Token::RParen | Token::RBracket) || is_value_end_keyword(t)
}

/// 值起始词元（出现在 MOD 右侧说明 MOD 处于运算符位而非别名/函数名位）——
/// 覆盖所有可**开始**值表达式的词元形态：原子词元 + 左括号/左中括号 +
/// 一元符号 + 运算符 + 值起始关键字（见 is_value_start_keyword）。
/// 含 `+`/`-` 与全部运算符：参照 `7 mod +3`、`7 mod -3`、`7 mod * 3` 都是
/// 语法错误（运算符/一元符号不算别名起点）；子句关键字（FROM/WHERE/…）
/// 与逗号/右括号不在此列——`SELECT 7 mod FROM t` 的尾随别名不误伤。
fn starts_value(t: &Token) -> bool {
    is_atom(t)
        || matches!(
            t,
            Token::LParen
                | Token::LBracket
                | Token::Plus
                | Token::Minus
                | Token::Op(_)
                | Token::OpLe
                | Token::OpNe
                | Token::OpShiftL
                | Token::OpGe
                | Token::OpShiftR
                | Token::OpArrow
                | Token::OpJsonArrow
                | Token::OpNe2
                | Token::OpDblBang
                | Token::OpConcat
                | Token::Typecast
                | Token::DotDot
                | Token::ColonEquals
                | Token::ParamEquals
                | Token::PlusJoin
                | Token::Caret
                | Token::Lt
                | Token::Gt
                | Token::Eq
                | Token::Star
                | Token::Slash
                | Token::Percent
                | Token::At
        )
        || is_value_start_keyword(t)
}

/// 关键字本身是完整值或值表达式的收尾（可作 MOD 左操作数）：
/// NULL/TRUE/FALSE、CURRENT_*/LOCALTIME*/SYSDATE/SESSION_USER/USER、
/// CASE..END 的 END、INTERVAL 区间字段（`INTERVAL '1' DAY` 以 DAY 收尾）。
fn is_value_end_keyword(t: &Token) -> bool {
    let Token::Keyword(k) = t else {
        return false;
    };
    use Keyword as K;
    matches!(
        k,
        K::NULL_P
            | K::TRUE_P
            | K::FALSE_P
            | K::CURRENT_P
            | K::CURRENT_DATE
            | K::CURRENT_TIME
            | K::CURRENT_TIMESTAMP
            | K::CURRENT_USER
            | K::CURRENT_ROLE
            | K::CURRENT_CATALOG
            | K::CURRENT_SCHEMA
            | K::LOCALTIME
            | K::LOCALTIMESTAMP
            | K::SYSDATE
            | K::SESSION_USER
            | K::USER
            | K::END_P
            | K::YEAR_P
            | K::YEAR_MONTH_P
            | K::MONTH_P
            | K::DAY_P
            | K::DAY_HOUR_P
            | K::DAY_MINUTE_P
            | K::DAY_SECOND_P
            | K::HOUR_P
            | K::HOUR_MINUTE_P
            | K::HOUR_SECOND_P
            | K::MINUTE_P
            | K::MINUTE_SECOND_P
            | K::SECOND_P
            | K::TIMEZONE_HOUR_P
            | K::TIMEZONE_MINUTE_P
    )
}

/// 关键字可以**开始**一个值/表达式（可作 MOD 右操作数）：值字面量、
/// CASE/CAST/EXISTS/NOT、布尔与比较算符（AND/OR/IS/IN/LIKE/BETWEEN/SIMILAR
/// ——它们都不可能合法地跟在别名 MOD 之后）、类型字面量前缀
/// （DATE/TIME/TIMESTAMP/INTERVAL）、以及一批值函数关键字
/// （COALESCE/NULLIF/DECODE/NVL/GREATEST/LEAST/POSITION/SUBSTRING/TRIM/
/// EXTRACT/OVERLAY/REPLACE/ARRAY/JSON_EXISTS/XMLEXISTS）。
/// 子句/结构关键字（FROM/WHERE/GROUP/…/JOIN/ON/USING/AS/INTO/…）一律不在
/// 此列——它们是别名 MOD 的合法后继，列入即误伤。
fn is_value_start_keyword(t: &Token) -> bool {
    let Token::Keyword(k) = t else {
        return false;
    };
    use Keyword as K;
    matches!(
        k,
        K::NULL_P
            | K::TRUE_P
            | K::FALSE_P
            | K::CURRENT_P
            | K::CURRENT_DATE
            | K::CURRENT_TIME
            | K::CURRENT_TIMESTAMP
            | K::CURRENT_USER
            | K::CURRENT_ROLE
            | K::CURRENT_CATALOG
            | K::CURRENT_SCHEMA
            | K::LOCALTIME
            | K::LOCALTIMESTAMP
            | K::SYSDATE
            | K::SESSION_USER
            | K::USER
            | K::CASE
            | K::CAST
            | K::EXISTS
            | K::NOT
            | K::AND
            | K::OR
            | K::IS
            | K::IN_P
            | K::LIKE
            | K::ILIKE
            | K::BETWEEN
            | K::SIMILAR
            | K::THEN
            | K::ELSE
            | K::WHEN
            | K::END_P
            | K::DATE_P
            | K::TIME
            | K::TIMESTAMP
            | K::INTERVAL
            | K::COALESCE
            | K::NULLIF
            | K::DECODE
            | K::NVL
            | K::GREATEST
            | K::LEAST
            | K::POSITION
            | K::SUBSTRING
            | K::TRIM
            | K::EXTRACT
            | K::OVERLAY
            | K::REPLACE
            | K::ARRAY
            | K::JSON_EXISTS
            | K::XMLEXISTS
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
    fn should_reject_string_literal_as_operand() {
        // `'x' mod 3`：字符串是值词元——参照中缀 MOD 恒为语法错误（V61 复审：
        // 此前放行后落 ENG-0022 错码/静默 NULL，统一收敛 BIZ-0126）
        let r = scan(vec![
            Token::StringLiteral("x".into()),
            Token::Ident("MOD".into()),
            Token::Integer(3),
        ]);
        assert_eq!(r.unwrap_err().code.0, "BIZ-0126");
    }

    #[test]
    fn should_reject_keyword_value_operands() {
        use ogsql_parser::token::Keyword as K;
        // `null mod 3` / `7 mod null`：NULL 是 Keyword 词元的值（V61 复审漏检面）
        for (l, r_) in [
            (Token::Keyword(K::NULL_P), Token::Integer(3)),
            (Token::Integer(7), Token::Keyword(K::NULL_P)),
            (Token::Keyword(K::TRUE_P), Token::Integer(3)),
            (Token::Integer(7), Token::Keyword(K::FALSE_P)),
            (Token::Keyword(K::CURRENT_DATE), Token::Integer(3)),
            (Token::Integer(7), Token::Keyword(K::CASE)),
            (Token::Keyword(K::END_P), Token::Integer(3)),
        ] {
            let r = scan(vec![l.clone(), Token::Ident("mod".into()), r_.clone()]);
            assert_eq!(r.unwrap_err().code.0, "BIZ-0126", "{l:?} mod {r_:?}");
        }
    }

    #[test]
    fn should_reject_mod_before_operator_token() {
        // `7 mod * 3`：运算符不可能是别名后继，参照同为语法错误 → BIZ-0126
        let r = scan(vec![
            Token::Integer(7),
            Token::Ident("mod".into()),
            Token::Star,
            Token::Integer(3),
        ]);
        assert_eq!(r.unwrap_err().code.0, "BIZ-0126");
    }

    #[test]
    fn should_exempt_mod_alias_before_clause_keywords() {
        use ogsql_parser::token::Keyword as K;
        // `expr mod <子句关键字>` 是合法尾随别名：FROM/WHERE/INTO/JOIN/ON 等
        for k in [
            K::FROM,
            K::WHERE,
            K::GROUP_P,
            K::ORDER,
            K::HAVING,
            K::LIMIT,
            K::UNION,
            K::JOIN,
            K::ON,
            K::USING,
            K::INTO,
            K::VALUES,
            K::SET,
            K::AS,
            K::FOR,
            K::RETURNING,
        ] {
            assert!(
                scan(vec![
                    Token::Integer(7),
                    Token::Ident("mod".into()),
                    Token::Keyword(k)
                ])
                .is_ok(),
                "mod before {k:?} must stay alias"
            );
        }
    }

    #[test]
    fn should_not_flag_quoted_mod_identifier() {
        // `"MOD" 3`：带引号标识符不是裸 MOD
        assert!(scan(vec![Token::QuotedIdent("MOD".into()), Token::Integer(3),]).is_ok());
    }
}
