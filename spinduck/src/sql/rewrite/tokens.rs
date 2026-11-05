//! token 级改写遍历：PL 变量 → 命名占位符，`[functions]` 重命名（AGENTS §3 规模拆分）。

use crate::error::EngineError;
use crate::ir::BindSrc;
use crate::ir::VarName;
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
