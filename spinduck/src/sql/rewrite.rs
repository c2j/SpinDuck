//! SQL 改写（M3）：token 级 PL 变量替换为命名占位符 `$v_<名>`。
//! 基底是 SqlStatement 自带的原始 sql_text（Token::StringLiteral 是独立
//! token，字符串内不会被误替换）；方言函数映射自 M3.5 接入本管线。

use crate::error::EngineError;
use crate::ir::VarName;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::Token;
use std::collections::BTreeSet;

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
    fn should_resolve_placeholder_names_case_insensitively() {
        let u = rewrite_tokens("SELECT * FROM t WHERE d = P_I_DATE", &vars(&["p_i_date"])).unwrap();
        assert_eq!(u.binds[0].1, VarName("p_i_date".into()));
    }
}
