//! Gate 1B SQL 引用检查（设计 V4 §4.3）：在 `SchemaSnapshot` 上校验
//! SQL 语句引用的表/视图对象存在性。纯函数、不写诊断文件位置——
//! 位置与 `BIZ-0152` 归一由编排层（Stage 3A）负责。
//!
//! 遍历复用 `ogsql-parser` 的 `Visitor`/`walk_statement`：覆盖 SELECT
//! FROM 引用树、INSERT/UPDATE/DELETE 目标与源、表达式内子查询
//! （EXISTS/IN/标量/SOME）、以及 PL 体内 `SqlStatement` 的内嵌 SQL。
//! CTE 名采用**整条语句级**集合做遮蔽（V1 近似：不同 SELECT 分支的
//! 同名 CTE 会相互遮蔽，仅影响"CTE 名与真实缺失表同名"的边角场景，
//! 已在设计评审中记录待精化）。

use std::collections::BTreeSet;

use ogsql_parser::ast::visitor::{walk_statement, Visitor, VisitorResult};
use ogsql_parser::ast::{
    DeleteStatement, InsertStatement, ObjectName, SelectStatement, Spanned, Statement, TableRef,
    UpdateStatement,
};

use super::schema::{object_key, SchemaSnapshot};

/// 单个缺失对象诊断（`BIZ-0152` 前的中间形态）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MissingRef {
    /// 归一化限定名（`object_key` 口径）。
    pub object: String,
    /// 对象类别：`"table"` / `"view"`。
    pub kind: &'static str,
}

/// 检查一条顶层语句（含 PL 体内嵌 SQL）的对象引用；返回去重排序后的
/// 全部缺失项（不 fail-fast，调用方聚合输出）。
pub fn check_statement(st: &Statement, snap: &SchemaSnapshot) -> Vec<MissingRef> {
    // Pass 1：收集整条语句的 CTE 名（供 TableRef 遮蔽判定）
    let mut ctes = CteNames(BTreeSet::new());
    walk_statement(&mut ctes, st);
    // Pass 2：引用检查
    let mut chk = RefChecker {
        snap,
        ctes: &ctes.0,
        out: Vec::new(),
    };
    walk_statement(&mut chk, st);
    chk.out.sort();
    chk.out.dedup();
    chk.out
}

/// Pass 1：CTE 名收集器（语句级并集，见模块头注释）。
struct CteNames(BTreeSet<String>);

impl Visitor for CteNames {
    fn visit_select(&mut self, sel: &SelectStatement) -> VisitorResult {
        if let Some(w) = &sel.with {
            for c in &w.ctes {
                self.0.insert(c.name.to_ascii_lowercase());
            }
        }
        VisitorResult::Continue
    }
}

/// Pass 2：FROM 表引用与 INSERT 目标的存在性检查。
struct RefChecker<'a> {
    snap: &'a SchemaSnapshot,
    ctes: &'a BTreeSet<String>,
    out: Vec<MissingRef>,
}

impl RefChecker<'_> {
    fn known(&self, name: &ObjectName) -> bool {
        // CTE 名是单段标识符；取 ObjectName 末段小写比较
        if let Some(last) = name.last() {
            if self.ctes.contains(&last.value.to_ascii_lowercase()) {
                return true;
            }
        }
        let key = object_key(name);
        self.snap.tables.contains_key(&key)
            || self.snap.views.contains_key(&key)
            || self.engine_provided(name)
            || self.suffix_known(name)
    }

    /// 引擎自建伪表（`sys_dummy`，口径同 `verify::ENGINE_PROVIDED_TABLES`）
    /// 与 Oracle 兼容 `dual`：闭包内无 DDL 但运行期存在，不算 Missing。
    fn engine_provided(&self, name: &ObjectName) -> bool {
        if name.len() != 1 {
            return false;
        }
        let want = name[0].value.to_ascii_lowercase();
        want == "dual"
            || crate::verify::ENGINE_PROVIDED_TABLES
                .iter()
                .any(|t| *t == want)
    }

    /// 单段未限定名的后缀匹配：`FROM t` 命中 `s.t` 形态的已知对象
    /// （schema 限定歧义留给列检查切片细化）。
    fn suffix_known(&self, name: &ObjectName) -> bool {
        if name.len() != 1 {
            return false;
        }
        let want = name[0].value.to_ascii_lowercase();
        let hit = |k: &String| k.rsplit('.').next() == Some(want.as_str());
        self.snap.tables.keys().any(hit) || self.snap.views.keys().any(hit)
    }
}

impl Visitor for RefChecker<'_> {
    fn visit_table_ref(&mut self, tr: &TableRef) -> VisitorResult {
        if let TableRef::Table { name, .. } = tr {
            if !self.known(name) {
                self.out.push(MissingRef {
                    object: object_key(name),
                    kind: "table",
                });
            }
        }
        // Continue：walk_table_ref 继续递归 JOIN/子查询/PIVOT 等子节点
        VisitorResult::Continue
    }

    fn visit_insert(&mut self, i: &InsertStatement) -> VisitorResult {
        // INSERT 目标是 ObjectName 而非 TableRef，需单独判存在性
        // （表或可更新视图均可）。
        if !self.known(&i.table) {
            self.out.push(MissingRef {
                object: object_key(&i.table),
                kind: "table",
            });
        }
        VisitorResult::Continue
    }

    // walk_update/walk_delete 不遍历 tables/from/using 的 TableRef
    // （ogsql-parser visitor 缺口），此处手工补齐；子查询经
    // Spanned 包装回到 walk_statement 获得完整递归。
    fn visit_update(&mut self, u: &UpdateStatement) -> VisitorResult {
        for t in u.tables.iter().chain(u.from.iter()) {
            self.check_dml_table_ref(t);
        }
        VisitorResult::Continue
    }

    fn visit_delete(&mut self, d: &DeleteStatement) -> VisitorResult {
        for t in d.tables.iter().chain(d.using.iter()) {
            self.check_dml_table_ref(t);
        }
        VisitorResult::Continue
    }
}

impl RefChecker<'_> {
    /// DML 字段内的 TableRef 手工递归（Table/Subquery/Join/Pivot/Unpivot）；
    /// FunctionCall/Values 内的表达式子查询为已知残留缺口（V1）。
    fn check_dml_table_ref(&mut self, tr: &TableRef) {
        match tr {
            TableRef::Table { name, .. } => {
                if !self.known(name) {
                    self.out.push(MissingRef {
                        object: object_key(name),
                        kind: "table",
                    });
                }
            }
            TableRef::Subquery { query, .. } => {
                let stmt = Statement::Select(Spanned::without_span(query.as_ref().clone()));
                walk_statement(self, &stmt);
            }
            TableRef::Join { left, right, .. } => {
                self.check_dml_table_ref(left);
                self.check_dml_table_ref(right);
            }
            TableRef::Pivot { source, .. } | TableRef::Unpivot { source, .. } => {
                self.check_dml_table_ref(source);
            }
            TableRef::FunctionCall { .. } | TableRef::Values { .. } => {}
        }
    }
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
    fn should_recurse_subquery_join_and_expr_subquery() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let got = missing(
            "SELECT * FROM (SELECT * FROM deep_missing) s JOIN t1 ON true",
            &snap,
        );
        assert_eq!(got, ["deep_missing"]);
        // 表达式内子查询（IN/EXISTS）同样覆盖
        let got = missing(
            "SELECT * FROM t1 WHERE a IN (SELECT a FROM expr_missing)",
            &snap,
        );
        assert_eq!(got, ["expr_missing"]);
    }

    #[test]
    fn should_flag_dml_targets_and_sources() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        assert_eq!(
            missing("INSERT INTO ghost_t SELECT * FROM t1", &snap),
            ["ghost_t"]
        );
        assert_eq!(missing("UPDATE ghost_u SET a = 1", &snap), ["ghost_u"]);
        assert_eq!(
            missing("DELETE FROM t1 WHERE a IN (SELECT a FROM ghost_d)", &snap),
            ["ghost_d"]
        );
    }

    #[test]
    fn should_flag_sql_inside_pl_body() {
        // Gate 1B 核心场景：例程体内嵌 SQL 的缺失对象
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let got = missing(
            "CREATE OR REPLACE PROCEDURE p IS BEGIN INSERT INTO ghost_pl VALUES (1); END;",
            &snap,
        );
        assert_eq!(got, ["ghost_pl"]);
    }

    #[test]
    fn should_accept_engine_provided_and_dual() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        assert!(missing("SELECT * FROM sys_dummy", &snap).is_empty());
        assert!(missing("SELECT * FROM dual", &snap).is_empty());
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
