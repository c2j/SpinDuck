//! DEFAULT 常量证据与跨编译/运行阶段的表达式规范化。

use super::{schema::Default, shape::Tokens};

pub(super) fn key(sql: &str) -> String {
    match Tokens::new(sql) {
        Some(tokens) => normalized(tokens.words()),
        None => sql.to_string(),
    }
}

pub(super) fn classify(tokens: &[String]) -> Default {
    match constant(tokens) {
        Some(sql) => Default::PureConst(sql),
        None => Default::Opaque(strip_parens(tokens).join(" ")),
    }
}

fn normalized(tokens: &[String]) -> String {
    constant(tokens).unwrap_or_else(|| strip_parens(tokens).join(" "))
}

fn constant(tokens: &[String]) -> Option<String> {
    let t = strip_parens(tokens);
    match t {
        [literal] if is_literal(literal) => return Some(literal.clone()),
        [sign, number] if matches!(sign.as_str(), "+" | "-") && number.parse::<f64>().is_ok() => {
            return Some(format!("{sign}{number}"));
        }
        _ => {}
    }
    if let Some(pos) = top_position(t, "::") {
        let inner = constant(&t[..pos])?;
        // NULL 的 CAST 目标类型不影响写入值（恒为 NULL）：DuckDB 会把
        // `NULL::numeric` 物化成带精度的 `CAST(NULL AS DECIMAL(18,3))`，
        // 与快照原文 `cast(null as decimal)` 必然漂移——归一为 null。
        if inner == "null" {
            return Some("null".into());
        }
        return Some(format!(
            "cast ( {} as {} )",
            inner,
            cast_type(&t[pos + 1..])?
        ));
    }
    if t.first().is_some_and(|s| s == "cast")
        && t.get(1).is_some_and(|s| s == "(")
        && outer_balanced(&t[1..])
    {
        let inside = &t[2..t.len() - 1];
        let pos = top_position(inside, "as")?;
        let inner = constant(&inside[..pos])?;
        if inner == "null" {
            return Some("null".into());
        }
        return Some(format!(
            "cast ( {} as {} )",
            inner,
            cast_type(&inside[pos + 1..])?
        ));
    }
    None
}

fn cast_type(tokens: &[String]) -> Option<String> {
    let words: Vec<_> = tokens
        .iter()
        .map(|s| s.trim_matches('"').to_ascii_lowercase())
        .collect();
    let (base, rest) = match words.as_slice() {
        [character, varying, rest @ ..] if character == "character" && varying == "varying" => {
            ("varchar", rest)
        }
        [base, rest @ ..] => (
            match base.as_str() {
                "numeric" => "decimal",
                "int" | "int4" => "integer",
                "int8" => "bigint",
                "bool" => "boolean",
                other => other,
            },
            rest,
        ),
        [] => return None,
    };
    if !matches!(
        base,
        "varchar"
            | "decimal"
            | "integer"
            | "bigint"
            | "smallint"
            | "tinyint"
            | "boolean"
            | "double"
            | "date"
            | "timestamp"
            | "time"
    ) {
        return None;
    }
    match rest {
        [] => Some(base.to_string()),
        [open, size, close]
            if base == "varchar" && open == "(" && close == ")" && size.parse::<u32>().is_ok() =>
        {
            Some(base.to_string())
        }
        [open, precision, comma, scale, close]
            if base == "decimal"
                && open == "("
                && comma == ","
                && close == ")"
                && precision.parse::<u8>().is_ok()
                && scale.parse::<u8>().is_ok() =>
        {
            Some(format!("decimal ( {precision} , {scale} )"))
        }
        _ => None,
    }
}

fn top_position(tokens: &[String], target: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut result = None;
    for (index, token) in tokens.iter().enumerate() {
        match token.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            word if depth == 0 && word == target => result = Some(index),
            _ => {}
        }
    }
    result
}

fn strip_parens(mut tokens: &[String]) -> &[String] {
    while tokens.len() >= 2 && tokens[0] == "(" && outer_balanced(tokens) {
        tokens = &tokens[1..tokens.len() - 1];
    }
    tokens
}

fn outer_balanced(tokens: &[String]) -> bool {
    let mut depth = 0i32;
    for (index, token) in tokens.iter().enumerate() {
        match token.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return index == tokens.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn is_literal(token: &str) -> bool {
    matches!(token, "null" | "true" | "false")
        || token.starts_with('\'')
        || token.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_canonicalize_only_constant_casts_without_erasing_type_or_value() {
        for (source, runtime) in [
            ("'A B'::character varying", "CAST('A B' AS VARCHAR)"),
            (
                "((CAST(1.25 AS NUMERIC(10,2))))",
                "CAST(1.25 AS DECIMAL(10,2))",
            ),
            ("NULL::varchar(20)", "CAST(NULL AS VARCHAR)"),
            ("-7", "(-7)"),
        ] {
            assert_eq!(key(source), key(runtime));
            assert!(matches!(
                classify(Tokens::new(source).unwrap().words()),
                Default::PureConst(_)
            ));
        }
        assert_ne!(
            key("CAST(1 AS DECIMAL(10,2))"),
            key("CAST(1 AS DECIMAL(10,3))")
        );
        assert_ne!(key("'A'::VARCHAR"), key("'a'::VARCHAR"));
        for expr in [
            "current_timestamp",
            "nextval('seq')",
            "CAST(random() AS DECIMAL(10,2))",
            "CAST('now' AS TIMESTAMPTZ)",
            "(1)+(2)",
        ] {
            assert!(
                matches!(
                    classify(Tokens::new(expr).unwrap().words()),
                    Default::Opaque(_)
                ),
                "{expr}"
            );
        }
    }
}
