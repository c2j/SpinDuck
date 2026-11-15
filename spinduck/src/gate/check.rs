//! Gate 1B SQL 引用检查（设计 V4 §4.3）：在 `SchemaSnapshot` 上校验
//! SQL 语句引用的表/视图对象存在性。纯函数、不写诊断文件位置——
//! 位置与 `BIZ-0152` 归一由编排层（Stage 3A）负责。
//!
//! 本切片覆盖：SELECT 的 FROM 表引用树（JOIN/子查询/PIVOT/UNPIVOT/
//! CTE/集合操作）、INSERT 目标与 SELECT 源、UPDATE/DELETE 的
//! tables/from/using。表达式内子查询（EXISTS/IN/SOME/标量）与列级
//! 检查在后续切片接入；`Expr` 级遗漏在接线时必须补齐，不得静默放行。

use std::collections::BTreeSet;

use ogsql_parser::ast::{ObjectName, Statement, TableRef};

use super::schema::{object_key, SchemaSnapshot};

/// 单个缺失对象诊断（`BIZ-0152` 前的中间形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRef {
    /// 归一化限定名（`object_key` 口径）。
    pub object: String,
    /// 对象类别：`"table"` / `"view"`。
    pub kind: &'static str,
}

/// 检查一条顶层 SQL 语句的对象引用；返回全部缺失项（不 fail-fast，
/// 由调用方聚合排序输出）。
pub fn check_statement(st: &Statement, snap: &SchemaSnapshot) -> Vec<MissingRef> {
    let mut out = Vec::new();
    match st {
        Statement::Select(s) => check_select(s, snap, &mut BTreeSet::new(), &mut out),
        Statement::Insert(i) => {
            check_relation_target(&i.table, snap, &mut out);
            if let ogsql_parser::ast::InsertSource::Select(q) = &i.source {
                check_select(q, snap, &mut BTreeSet::new(), &mut out);
            }
        }
        Statement::Update(u) => {
            let ctes = cte_names(u.with.as_ref());
            for t in u.tables.iter().chain(u.from.iter()) {
                check_table_ref(t, snap, &ctes, &mut out);
            }
        }
        Statement::Delete(d) => {
            let ctes = cte_names(d.with.as_ref());
            for t in d.tables.iter().chain(d.using.iter()) {
                check_table_ref(t, snap, &ctes, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn cte_names(with: Option<&ogsql_parser::ast::WithClause>) -> BTreeSet<String> {
    with.map(|w| w.ctes.iter().map(|c| c.name.to_ascii_lowercase()).collect())
        .unwrap_or_default()
}

fn check_select(
    sel: &ogsql_parser::ast::SelectStatement,
    snap: &SchemaSnapshot,
    outer_ctes: &mut BTreeSet<String>,
    out: &mut Vec<MissingRef>,
) {
    // CTE 名对本层及嵌套子查询可见；先登记全部名字再查各 CTE 体
    let saved = outer_ctes.clone();
    outer_ctes.extend(cte_names(sel.with.as_ref()));
    if let Some(w) = &sel.with {
        for cte in &w.ctes {
            check_select(&cte.query, snap, outer_ctes, out);
        }
    }
    for t in &sel.from {
        check_table_ref(t, snap, outer_ctes, out);
    }
    if let Some(op) = &sel.set_operation {
        match op {
            ogsql_parser::ast::SetOperation::Union { right, .. }
            | ogsql_parser::ast::SetOperation::Intersect { right, .. }
            | ogsql_parser::ast::SetOperation::Except { right, .. } => {
                check_select(right, snap, outer_ctes, out);
            }
        }
    }
    *outer_ctes = saved;
}

fn check_table_ref(
    tr: &TableRef,
    snap: &SchemaSnapshot,
    ctes: &BTreeSet<String>,
    out: &mut Vec<MissingRef>,
) {
    match tr {
        TableRef::Table { name, .. } => {
            if !is_cte_or_known(name, snap, ctes) {
                out.push(MissingRef {
                    object: object_key(name),
                    kind: "table",
                });
            }
        }
        TableRef::Subquery { query, .. } => check_select(query, snap, &mut ctes.clone(), out),
        TableRef::Join { left, right, .. } => {
            check_table_ref(left, snap, ctes, out);
            check_table_ref(right, snap, ctes, out);
        }
        TableRef::Pivot { source, .. } | TableRef::Unpivot { source, .. } => {
            check_table_ref(source, snap, ctes, out);
        }
        TableRef::FunctionCall { .. } | TableRef::Values { .. } => {}
    }
}

/// INSERT/UPDATE 目标关系：表或可更新视图均可。
fn check_relation_target(name: &ObjectName, snap: &SchemaSnapshot, out: &mut Vec<MissingRef>) {
    let key = object_key(name);
    if snap.tables.contains_key(&key) || snap.views.contains_key(&key) || suffix_known(name, snap) {
        return;
    }
    out.push(MissingRef {
        object: key,
        kind: "table",
    });
}

fn is_cte_or_known(name: &ObjectName, snap: &SchemaSnapshot, ctes: &BTreeSet<String>) -> bool {
    // CTE 名是单段标识符；取 ObjectName 末段小写比较
    if let Some(last) = name.last() {
        if ctes.contains(&last.value.to_ascii_lowercase()) {
            return true;
        }
    }
    let key = object_key(name);
    snap.tables.contains_key(&key) || snap.views.contains_key(&key) || suffix_known(name, snap)
}

/// 单段未限定名的后缀匹配：`FROM t` 命中 `s.t` 形态的已知对象
/// （schema 限定歧义留给列检查切片细化）。
fn suffix_known(name: &ObjectName, snap: &SchemaSnapshot) -> bool {
    if name.len() != 1 {
        return false;
    }
    let want = name[0].value.to_ascii_lowercase();
    let hit = |k: &String| k.rsplit('.').next() == Some(want.as_str());
    snap.tables.keys().any(hit) || snap.views.keys().any(hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::schema::SchemaSnapshot;
    use ogsql_parser::token::tokenizer::Tokenizer;

    fn snap_of(ddl: &str) -> SchemaSnapshot {
        let tokens = Tokenizer::new(ddl).tokenize().unwrap();
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        SchemaSnapshot::from_statements(&stmts).unwrap()
    }

    fn stmt_of(sql: &str) -> Statement {
        let tokens = Tokenizer::new(sql).tokenize().unwrap();
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        assert_eq!(stmts.len(), 1, "{sql}");
        stmts.into_iter().next().unwrap()
    }

    fn missing(sql: &str, snap: &SchemaSnapshot) -> Vec<String> {
        check_statement(&stmt_of(sql), snap)
            .into_iter()
            .map(|m| m.object)
            .collect()
    }

    #[test]
    fn should_flag_missing_from_table() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        assert_eq!(missing("SELECT * FROM ghost", &snap), ["ghost"]);
        assert!(missing("SELECT * FROM t1", &snap).is_empty());
    }

    #[test]
    fn should_not_flag_cte_name_as_missing() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        assert!(
            missing(
                "WITH cte_x AS (SELECT a FROM t1) SELECT * FROM cte_x",
                &snap
            )
            .is_empty(),
            "CTE 名不应误报 Missing"
        );
    }

    #[test]
    fn should_recurse_subquery_and_join() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let got = missing(
            "SELECT * FROM (SELECT * FROM deep_missing) s JOIN t1 ON true",
            &snap,
        );
        assert_eq!(got, ["deep_missing"]);
    }

    #[test]
    fn should_flag_dml_targets_and_sources() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        assert_eq!(
            missing("INSERT INTO ghost_t SELECT * FROM t1", &snap),
            ["ghost_t"]
        );
        assert_eq!(missing("UPDATE ghost_u SET a = 1", &snap), ["ghost_u"]);
        assert!(missing("DELETE FROM t1 WHERE a IN (SELECT a FROM ghost_d)", &snap).is_empty());
        // DELETE 的 USING/子查询表达式内对象留待表达式 walker 切片
    }

    #[test]
    fn should_accept_view_and_suffix_match() {
        let snap = snap_of("CREATE TABLE s.t1(a INT); CREATE VIEW v1 AS SELECT 1");
        assert!(missing("SELECT * FROM v1", &snap).is_empty());
        assert!(
            missing("SELECT * FROM t1", &snap).is_empty(),
            "单段名后缀命中 s.t1"
        );
    }
}
