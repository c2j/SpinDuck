//! SchemaSnapshot（设计 V4 §4.1/§4.3 Stage 2）：项目 DDL AST →
//! 表/视图/序列符号表。Gate 1B 列检查与 Gate 2B Stub 库共用的唯一
//! schema 真源；类型映射统一走 `config/rewrite-rules.toml [types]`，
//! 本模块只保存原始 `DataType`，不做第二套转换。

use std::collections::{BTreeMap, BTreeSet};

use crate::error::EngineError;
use ogsql_parser::ast::{AlterTableAction, DataType, Ident, ObjectName, Statement};

/// 限定名归一化：未加引号部分小写，加引号部分保留原样（与 GaussDB
/// 折叠语义一致；SpinDuck 例程名同口径）。
fn object_key(name: &ObjectName) -> String {
    name.iter()
        .map(|i: &Ident| {
            if i.quote_style.is_some() {
                i.value.clone()
            } else {
                i.value.to_ascii_lowercase()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn column_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// 单表定义：列名（小写）→ 原始列类型。
#[derive(Debug, Clone, Default)]
pub struct TableDef {
    pub columns: BTreeMap<String, DataType>,
    /// CTAS：列集合由 Gate 2B 联合推导，Gate 1B 对其列引用延迟判定
    /// （设计 V4 §4.3.3 `DeferredToGate2B`）。
    pub columns_deferred: bool,
}

/// 离线视图定义：显式列清单可得时记录，否则 `Unknown`——V1 显式限制，
/// 不从 SELECT 投影臆造列（设计 V4 §4.3.4）。
#[derive(Debug, Clone)]
pub enum ViewDef {
    Known(BTreeSet<String>),
    Unknown,
}

/// DDL 符号快照：同一文件集内 CREATE/ALTER/DROP 的确定性演化结果。
#[derive(Debug, Default)]
pub struct SchemaSnapshot {
    pub tables: BTreeMap<String, TableDef>,
    pub views: BTreeMap<String, ViewDef>,
    pub sequences: BTreeSet<String>,
}

impl SchemaSnapshot {
    /// 从一组已解析顶层语句构建快照；DDL 语义冲突报 `ENG-0070`。
    pub fn from_statements(stmts: &[Statement]) -> Result<Self, EngineError> {
        let mut snap = Self::default();
        for st in stmts {
            snap.apply(st)?;
        }
        Ok(snap)
    }

    fn apply(&mut self, st: &Statement) -> Result<(), EngineError> {
        match st {
            Statement::CreateTable(t) => {
                let key = object_key(&t.name);
                if self.tables.contains_key(&key) && !t.if_not_exists {
                    return Err(EngineError::invariant(
                        70,
                        format!("duplicate CREATE TABLE {key}"),
                    ));
                }
                if t.if_not_exists && self.tables.contains_key(&key) {
                    return Ok(());
                }
                let mut def = TableDef::default();
                for c in &t.columns {
                    if def
                        .columns
                        .insert(column_key(&c.name), c.data_type.clone())
                        .is_some()
                    {
                        return Err(EngineError::invariant(
                            70,
                            format!("duplicate column {} in {key}", c.name),
                        ));
                    }
                }
                self.tables.insert(key, def);
            }
            Statement::CreateTableAs(t) => {
                // CTAS：列集合（含显式列清单的类型信息）由 Gate 2B 联合推导，
                // 此处只登记表存在性与延迟标记。
                self.tables.insert(
                    object_key(&t.name),
                    TableDef {
                        columns_deferred: true,
                        ..Default::default()
                    },
                );
            }
            Statement::CreateView(v) => {
                let key = object_key(&v.name);
                let def = if v.columns.is_empty() {
                    ViewDef::Unknown
                } else {
                    ViewDef::Known(v.columns.iter().map(|c| column_key(c)).collect())
                };
                self.views.insert(key, def);
            }
            Statement::CreateSequence(s) => {
                self.sequences.insert(object_key(&s.name));
            }
            Statement::AlterTable(a) => self.apply_alter(a)?,
            Statement::Drop(d) => self.apply_drop(d),
            _ => {}
        }
        Ok(())
    }

    fn apply_alter(
        &mut self,
        a: &ogsql_parser::ast::AlterTableStatement,
    ) -> Result<(), EngineError> {
        let key = object_key(&a.name);
        let def = self.tables.get_mut(&key).ok_or_else(|| {
            EngineError::invariant(70, format!("ALTER TABLE {key}: table not found"))
        })?;
        for action in &a.actions {
            match action {
                AlterTableAction::AddColumn(c) => {
                    if def
                        .columns
                        .insert(column_key(&c.name), c.data_type.clone())
                        .is_some()
                    {
                        return Err(EngineError::invariant(
                            70,
                            format!("ALTER TABLE {key}: duplicate column {}", c.name),
                        ));
                    }
                }
                AlterTableAction::DropColumn {
                    name, if_exists, ..
                } => {
                    if def.columns.remove(&column_key(name)).is_none() && !if_exists {
                        return Err(EngineError::invariant(
                            70,
                            format!("ALTER TABLE {key}: column {name} not found"),
                        ));
                    }
                }
                AlterTableAction::AlterColumn {
                    name,
                    action: ogsql_parser::ast::AlterColumnAction::SetDataType(dt),
                } => match def.columns.get_mut(&column_key(name)) {
                    Some(slot) => *slot = dt.clone(),
                    None => {
                        return Err(EngineError::invariant(
                            70,
                            format!("ALTER TABLE {key}: column {name} not found"),
                        ));
                    }
                },
                _ => {}
            }
        }
        Ok(())
    }

    fn apply_drop(&mut self, d: &ogsql_parser::ast::DropStatement) {
        use ogsql_parser::ast::ObjectType;
        for name in &d.names {
            let key = object_key(name);
            match d.object_type {
                ObjectType::Table => {
                    self.tables.remove(&key);
                }
                ObjectType::View => {
                    self.views.remove(&key);
                }
                ObjectType::Sequence => {
                    self.sequences.remove(&key);
                }
                _ => {}
            }
        }
    }

    /// 表列集合；`None` = 快照中不存在该表。CTAS 表返回 `None` 语义由
    /// `columns_deferred` 区分（调用方先查 `is_deferred`）。
    pub fn table_columns(&self, name: &str) -> Option<&BTreeMap<String, DataType>> {
        self.tables.get(&column_key(name)).map(|d| &d.columns)
    }

    pub fn is_table_deferred(&self, name: &str) -> bool {
        self.tables
            .get(&column_key(name))
            .map(|d| d.columns_deferred)
            .unwrap_or(false)
    }

    pub fn view_def(&self, name: &str) -> Option<&ViewDef> {
        self.views.get(&column_key(name))
    }

    pub fn has_sequence(&self, name: &str) -> bool {
        self.sequences.contains(&column_key(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogsql_parser::token::tokenizer::Tokenizer;

    fn parse(sql: &str) -> Vec<Statement> {
        let tokens = Tokenizer::new(sql).tokenize().unwrap();
        ogsql_parser::parser::Parser::new(tokens).parse()
    }

    #[test]
    fn should_collect_create_table_columns() {
        let snap = SchemaSnapshot::from_statements(&parse(
            "CREATE TABLE t1(a INT, b VARCHAR2(10)); CREATE SEQUENCE s1;",
        ))
        .unwrap();
        let cols = snap.table_columns("t1").unwrap();
        assert!(cols.contains_key("a") && cols.contains_key("b"));
        assert!(snap.has_sequence("s1"));
    }

    #[test]
    fn should_apply_alter_table_evolution() {
        let snap = SchemaSnapshot::from_statements(&parse(
            "CREATE TABLE t1(a INT); \
             ALTER TABLE t1 ADD COLUMN c NUMBER; \
             ALTER TABLE t1 DROP COLUMN a; \
             ALTER TABLE t1 ALTER COLUMN c SET DATA TYPE VARCHAR2;",
        ))
        .unwrap();
        let cols = snap.table_columns("t1").unwrap();
        assert!(!cols.contains_key("a"), "DROP COLUMN 应生效");
        assert!(cols.contains_key("c"), "ADD COLUMN 应生效");
    }

    #[test]
    fn should_report_duplicate_create_table_as_eng_0070() {
        let err = SchemaSnapshot::from_statements(&parse(
            "CREATE TABLE t1(a INT); CREATE TABLE t1(b INT);",
        ))
        .unwrap_err();
        assert_eq!(err.code.to_string(), "ENG-0070", "{err:?}");

        // IF NOT EXISTS 幂等放行
        SchemaSnapshot::from_statements(&parse(
            "CREATE TABLE t1(a INT); CREATE TABLE IF NOT EXISTS t1(b INT);",
        ))
        .unwrap();
    }

    #[test]
    fn should_defer_ctas_columns() {
        let snap =
            SchemaSnapshot::from_statements(&parse("CREATE TEMP TABLE tmp_x AS SELECT a FROM t1;"))
                .unwrap();
        assert!(snap.is_table_deferred("tmp_x"));
        assert!(snap.table_columns("tmp_x").unwrap().is_empty());
    }

    #[test]
    fn should_record_view_columns_or_unknown() {
        let snap = SchemaSnapshot::from_statements(&parse(
            "CREATE VIEW v1(x, y) AS SELECT 1, 2; CREATE VIEW v2 AS SELECT 1;",
        ))
        .unwrap();
        assert!(matches!(
            snap.view_def("v1"),
            Some(ViewDef::Known(cols)) if cols.contains("x") && cols.contains("y")
        ));
        assert!(matches!(snap.view_def("v2"), Some(ViewDef::Unknown)));
    }

    #[test]
    fn should_error_alter_on_missing_table() {
        let err = SchemaSnapshot::from_statements(&parse("ALTER TABLE ghost ADD COLUMN c INT;"))
            .unwrap_err();
        assert_eq!(err.code.to_string(), "ENG-0070", "{err:?}");
    }
}
