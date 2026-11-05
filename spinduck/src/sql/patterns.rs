//! `[patterns]` 规则的 AST/文本级改写：静态语句编译与动态 SQL 准备的**唯一实现点**。
//!
//! 两条路径曾各写一份 rownum/UPDATE 去别名逻辑（第三轮评审 Blocker：口径可漂移）。
//! 现统一由 `apply_text_patterns` 入口提供，改写产物差异只可能来自文本来源
//! （静态 DML 用 PL 解析器文本、动态用 SqlFormatter 文本），不再来自规则实现。

use crate::error::EngineError;
use crate::rules::RuleRegistry;
use ogsql_parser::ast::{Expr, InsertSource, Literal, SelectStatement, Statement};

/// 文本级 `[patterns]` 应用：`rownum = 1` → `LIMIT 1`、`UPDATE t SET t.c` 去别名。
///
/// rownum 改写需要 AST（谓词定位），仅在文本含 `rownum` 时回解析一次；
/// 回解析失败保持原文——由后续 token 改写与内核给出明确错误，不静默吞掉。
pub fn apply_text_patterns(sql_text: &str, rules: &RuleRegistry) -> Result<String, EngineError> {
    let mut text = sql_text.to_string();
    if rules.patterns_rownum_first_row && text.to_ascii_uppercase().contains("ROWNUM") {
        let (infos, errors) = ogsql_parser::parser::Parser::parse_sql(&text);
        if errors.is_empty() && !infos.is_empty() {
            let fmt = ogsql_parser::formatter::SqlFormatter::new();
            let parts: Vec<String> = infos
                .iter()
                .map(|si| {
                    let mut st = si.statement.clone();
                    apply_rownum_first_row(&mut st);
                    fmt.format_statement(&st)
                })
                .collect();
            text = parts.join(";\n");
        }
    }
    if rules.patterns_update_strip_alias {
        text = super::strip_update_alias(&text);
    }
    Ok(text)
}

/// 语句级 `rownum = 1` 首行改写；返回是否实际改写。
pub fn apply_rownum_first_row(stmt: &mut Statement) -> bool {
    use Statement as S;
    match stmt {
        S::Select(sel) => apply_on_select(&mut sel.node),
        S::Insert(ins) => match &mut ins.source {
            // 语料的 rownum 用法集中在 INSERT ... SELECT（42 处，validation V13）
            InsertSource::Select(q) => apply_on_select(q),
            // VALUES / DEFAULT VALUES / SET / RECORD 变量形态无 WHERE 子句，
            // 不存在 rownum 谓词位置
            _ => false,
        },
        // UPDATE/DELETE 的 rownum 谓词不改变结果集上限语义（DuckDB 无 rownum，
        // 改写为 LIMIT 需要子查询包裹，语料未出现该形态）；M8 若遇到再扩
        _ => false,
    }
}

/// SELECT 体：摘除 `rownum = 1` 谓词并补 `LIMIT 1`；已有 LIMIT 时保守还原条件。
pub fn apply_on_select(sel: &mut SelectStatement) -> bool {
    let Some(w) = sel.where_clause.take() else {
        return false;
    };
    let (rest, stripped) = strip_rownum_first(w.clone());
    if !stripped || sel.limit.is_some() {
        // 未命中，或已有 LIMIT（不可静默覆盖既有行数语义）：原样保留
        sel.where_clause = Some(w);
        return false;
    }
    sel.where_clause = rest;
    sel.limit = Some(Expr::Literal(Literal::Integer(1)));
    true
}

/// 递归剥除 AND 链中的 `rownum = 1` 谓词。返回 (剩余条件, 是否剥除)。
/// 仅处理 AND 链；OR/NOT 分支内的 rownum 保守保留（不改写）。
pub fn strip_rownum_first(e: Expr) -> (Option<Expr>, bool) {
    if is_rownum_first(&e) {
        return (None, true);
    }
    if let Expr::BinaryOp { op, left, right } = &e {
        if op.eq_ignore_ascii_case("AND") {
            let (l, ls) = strip_rownum_first((**left).clone());
            let (r, rs) = strip_rownum_first((**right).clone());
            let stripped = ls || rs;
            let rest = match (l, r) {
                (Some(l), Some(r)) => Some(Expr::BinaryOp {
                    op: op.clone(),
                    left: Box::new(l),
                    right: Box::new(r),
                }),
                (one, None) | (None, one) => one,
            };
            return (rest, stripped);
        }
    }
    (Some(e), false)
}

fn is_rownum_first(e: &Expr) -> bool {
    let Expr::BinaryOp { op, left, right } = e else {
        return false;
    };
    if !op.eq_ignore_ascii_case("=") {
        return false;
    }
    let is_one = |x: &Expr| matches!(x, Expr::Literal(Literal::Integer(1)));
    let is_rownum = |x: &Expr| {
        matches!(x, Expr::ColumnRef(id) | Expr::PlVariable(id)
        if id.len() == 1 && id[0].value.eq_ignore_ascii_case("rownum"))
    };
    (is_rownum(left) && is_one(right)) || (is_one(left) && is_rownum(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(sql: &str) -> Statement {
        let (infos, errors) = ogsql_parser::parser::Parser::parse_sql(sql);
        assert!(errors.is_empty(), "解析失败: {errors:?}");
        infos[0].statement.clone()
    }

    #[test]
    fn should_add_limit1_when_stripping_rownum_from_select() {
        let mut st = parse_one("SELECT a FROM src WHERE rownum = 1");
        assert!(apply_rownum_first_row(&mut st));
        let out = ogsql_parser::formatter::SqlFormatter::new().format_statement(&st);
        assert!(
            !out.to_ascii_uppercase().contains("ROWNUM"),
            "rownum 必须摘除，实际: {out}"
        );
        assert!(
            out.to_ascii_uppercase().contains("LIMIT"),
            "必须补 LIMIT 1，实际: {out}"
        );
    }

    #[test]
    fn should_strip_rownum_from_insert_select() {
        // 语料主形态：INSERT INTO t SELECT ... WHERE rownum = 1（曾只在顶层 SELECT 生效）
        let mut st = parse_one("INSERT INTO tgt SELECT a FROM src WHERE rownum = 1");
        assert!(apply_rownum_first_row(&mut st), "INSERT...SELECT 必须下钻");
        let out = ogsql_parser::formatter::SqlFormatter::new().format_statement(&st);
        assert!(!out.to_ascii_uppercase().contains("ROWNUM"), "实际: {out}");
    }

    #[test]
    fn should_keep_existing_limit_and_rownum_untouched() {
        let mut st = parse_one("SELECT a FROM src WHERE rownum = 1 LIMIT 5");
        assert!(!apply_rownum_first_row(&mut st), "已有 LIMIT：不得覆盖");
        let out = ogsql_parser::formatter::SqlFormatter::new().format_statement(&st);
        assert!(
            out.to_ascii_uppercase().contains("ROWNUM"),
            "保守还原条件，实际: {out}"
        );
    }

    #[test]
    fn should_text_pattern_honour_rule_switches() {
        let mut off = RuleRegistry::default();
        off.overlay("[patterns]\nrownum_first_row = false\nupdate_strip_alias = false")
            .unwrap();
        let kept = apply_text_patterns("SELECT a FROM src WHERE rownum = 1", &off).unwrap();
        assert!(
            kept.to_ascii_uppercase().contains("ROWNUM"),
            "关闭开关不得改写"
        );
        let on = RuleRegistry::default();
        let rewritten = apply_text_patterns("SELECT a FROM src WHERE rownum = 1", &on).unwrap();
        assert!(
            !rewritten.to_ascii_uppercase().contains("ROWNUM"),
            "默认开启必须改写: {rewritten}"
        );
    }
}
