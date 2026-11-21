//! codeweb `SchemaExport` 诊断的作用域化（设计 §4.1 快照完备性）。
//!
//! codeweb 对"任一未建模 schema 语句"采取全局降级：全部表
//! `has_opaque_ddl`/`indexes_complete=false`。但未建模语句分三类：
//! - 视图/例程/包等**非结构语句**：不改变基表结构证明，可按文件逐条
//!   复核后豁免；
//! - 点名表的诊断（`unmodeled table definition`/`duplicate`/`unresolved
//!   index`）：只污染被点名的表，按表标记即可；
//! - 解析错误等无法定性的诊断：保持全局拒绝（现状）。
//!
//! 复核口径与 codeweb `embed::schema::export` 一致：重新解析诊断指向的
//! 源文件，全部语句必须落在白名单（结构已建模或可证不改表结构）内，
//! 否则放弃修复、导出原样保留。

use codeweb::embed::SchemaExport;
use ogsql_parser::ast::Statement;
use ogsql_parser::parser::Parser;
use ogsql_parser::token::tokenizer::Tokenizer;
use std::collections::BTreeSet;
use std::path::Path;

/// 诊断中"点名表"的三种前缀（codeweb `embed::schema` 的格式串）。
const TABLE_DEF_MARKER: &str = "unmodeled table definition ";
const DUP_MARKER: &str = "duplicate schema definition: ";
const INDEX_MARKER: &str = "unresolved or ambiguous index on ";

/// 就地收窄 `export.diagnostics` 的全局降级；返回是否完成修复
/// （`false` = 存在无法定性的诊断，`export` 保持原样）。
pub(super) fn scope_export_diagnostics(export: &mut SchemaExport) -> bool {
    if export.diagnostics.is_empty() {
        return true;
    }
    let mut suspects = BTreeSet::new();
    for d in &export.diagnostics {
        if let Some(file) = d.strip_suffix(": unmodeled schema statement") {
            if !nonstructural_file(Path::new(file)) {
                return false;
            }
        } else if let Some(name) = suspect_name(d) {
            suspects.insert(name.to_string());
        } else {
            return false;
        }
    }
    export.diagnostics.clear();
    for table in &mut export.tables {
        // 点名表维持不完备；未点名表还原 codeweb 全局降级前的标志
        // （独立不完备的表必有 "unmodeled table definition" 点名诊断）。
        let opaque = suspects.contains(&table.name.join("."));
        table.has_opaque_ddl = opaque;
        table.indexes_complete = !opaque;
    }
    true
}

/// 点名表诊断 → 限定名；非点名形态返回 None。
fn suspect_name(d: &str) -> Option<&str> {
    for marker in [TABLE_DEF_MARKER, DUP_MARKER, INDEX_MARKER] {
        if let Some((_, name)) = d.rsplit_once(marker) {
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// 文件内全部语句均可证不改变基表结构时返回 true。
/// 白名单 = codeweb 已建模（CreateTable/CreateIndex）+ 非结构 DDL
/// （视图/例程/包）。触发器、物化视图、ALTER/DROP/CTAS、DML、
/// DO/CALL/EXECUTE 等一律否决——它们可能改数据或结构。
fn nonstructural_file(path: &Path) -> bool {
    let Ok(sql) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(tokens) = Tokenizer::new(&sql).tokenize() else {
        return false;
    };
    let mut parser = Parser::with_source(tokens, sql);
    let infos = parser.parse_with_text();
    if !parser.errors().is_empty() {
        return false;
    }
    infos.iter().all(|info| {
        matches!(
            &info.statement,
            Statement::Empty
                | Statement::Comment(_)
                | Statement::CreateTable(_)
                | Statement::CreateIndex(_)
                | Statement::CreateView(_)
                | Statement::CreateProcedure(_)
                | Statement::CreateFunction(_)
                | Statement::CreatePackage(_)
                | Statement::CreatePackageBody(_)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeweb::embed::{ColumnSchemaExport, TableSchemaExport};

    fn table(name: &str) -> TableSchemaExport {
        TableSchemaExport {
            name: name.split('.').map(str::to_string).collect(),
            columns: vec![ColumnSchemaExport {
                name: "a".into(),
                source_type: "integer".into(),
                nullable: true,
                default_expr_raw: None,
            }],
            constraints: Vec::new(),
            indexes: Vec::new(),
            indexes_complete: true,
            has_opaque_ddl: false,
        }
    }

    fn export_with(diags: Vec<String>) -> SchemaExport {
        let mut export = SchemaExport {
            tables: vec![table("t1"), table("t2")],
            diagnostics: Vec::new(),
        };
        for t in &mut export.tables {
            // 模拟 codeweb 全局降级：任一诊断 → 全部表不完备
            t.has_opaque_ddl = true;
            t.indexes_complete = false;
        }
        export.diagnostics = diags;
        export
    }

    fn write(tmp: &tempfile::TempDir, name: &str, sql: &str) -> String {
        let path = tmp.path().join(name);
        std::fs::write(&path, sql).unwrap();
        path.display().to_string()
    }

    #[test]
    fn should_repair_tables_when_unmodeled_statements_are_nonstructural() {
        let tmp = tempfile::tempdir().unwrap();
        let view = write(&tmp, "v.sql", "CREATE VIEW v_x AS SELECT 1 AS a");
        let pkg = write(
            &tmp,
            "p.sql",
            "CREATE PACKAGE pkg AS PROCEDURE p IS BEGIN NULL; END; END pkg;\n\
             CREATE PACKAGE BODY pkg AS PROCEDURE p IS BEGIN NULL; END; END pkg",
        );
        let mut export = export_with(vec![
            format!("{view}: unmodeled schema statement"),
            format!("{pkg}: unmodeled schema statement"),
        ]);
        assert!(scope_export_diagnostics(&mut export));
        assert!(export.diagnostics.is_empty());
        for t in &export.tables {
            assert!(!t.has_opaque_ddl && t.indexes_complete, "{:?}", t.name);
        }
    }

    #[test]
    fn should_keep_global_rejection_for_structural_unmodeled_statement() {
        let tmp = tempfile::tempdir().unwrap();
        let ddl = write(&tmp, "a.sql", "ALTER TABLE t1 ADD COLUMN b INT");
        let mut export = export_with(vec![format!("{ddl}: unmodeled schema statement")]);
        assert!(!scope_export_diagnostics(&mut export));
        assert_eq!(export.diagnostics.len(), 1);
        assert!(export.tables.iter().all(|t| t.has_opaque_ddl));
    }

    #[test]
    fn should_keep_global_rejection_for_trigger_and_matview() {
        for sql in [
            "CREATE TRIGGER trg BEFORE INSERT ON t1 FOR EACH ROW EXECUTE PROCEDURE p()",
            "CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a",
            "CREATE TABLE t3 AS SELECT 1 AS a",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let f = write(&tmp, "x.sql", sql);
            let mut export = export_with(vec![format!("{f}: unmodeled schema statement")]);
            assert!(!scope_export_diagnostics(&mut export), "{sql}");
        }
    }

    #[test]
    fn should_mark_only_named_tables_opaque() {
        let mut export = export_with(vec![
            "/proj/sql/ddl/tables.sql: unmodeled table definition t2".into(),
        ]);
        assert!(scope_export_diagnostics(&mut export));
        let t1 = export.tables.iter().find(|t| t.name == ["t1"]).unwrap();
        let t2 = export.tables.iter().find(|t| t.name == ["t2"]).unwrap();
        assert!(!t1.has_opaque_ddl && t1.indexes_complete);
        assert!(t2.has_opaque_ddl && !t2.indexes_complete);
    }

    #[test]
    fn should_keep_global_rejection_for_unscoped_diagnostics() {
        let mut export = export_with(vec!["/proj/x.sql: syntax error at or near".into()]);
        assert!(!scope_export_diagnostics(&mut export));
        assert_eq!(export.diagnostics.len(), 1);
        assert!(export.tables.iter().all(|t| t.has_opaque_ddl));
    }
}
