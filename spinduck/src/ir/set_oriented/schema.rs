//! 当前事务内的表结构证据，仅接受无默认值和复杂约束的原生标量表。

use super::shape::{Name, Tokens};
use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor};
use crate::value::SpinValue;

pub(super) struct Column {
    pub name: Name,
    pub kind: String,
    pub nullable: bool,
}

pub(super) struct Table {
    pub qualified: String,
    pub columns: Vec<Column>,
}

pub(super) fn load(ex: &dyn SqlExecutor, name: &Name) -> Result<Option<Table>, EngineError> {
    let binds = BindMap::from([("$table".into(), SpinValue::Str(name.0.clone()))]);
    let mut stream = ex.query(
        "SELECT database_name, schema_name, table_name, sql, \
         database_name = current_database() AND schema_name = current_schema() \
         AND NOT EXISTS (SELECT 1 FROM duckdb_indexes() i \
         WHERE i.database_name = t.database_name AND i.table_oid = t.table_oid AND i.is_unique) AS visible \
         FROM duckdb_tables() t WHERE lower(table_name) = $table \
         UNION ALL SELECT database_name, schema_name, view_name, NULL, FALSE \
         FROM duckdb_views() WHERE lower(view_name) = $table",
        &binds,
    )?;
    let rows = stream.fetch_many(2);
    let [row] = rows.as_slice() else {
        return Ok(None);
    };
    let [SpinValue::Str(db), SpinValue::Str(schema), SpinValue::Str(table), SpinValue::Str(ddl), SpinValue::Bool(true)] =
        row.as_slice()
    else {
        return Ok(None);
    };
    let Some(columns) = columns(ddl, name) else {
        return Ok(None);
    };
    let qualified = [db, schema, table]
        .iter()
        .map(|n| Name((*n).clone()).sql())
        .collect::<Vec<_>>()
        .join(".");
    Ok(Some(Table { qualified, columns }))
}

fn columns(ddl: &str, table: &Name) -> Option<Vec<Column>> {
    let mut t = Tokens::new(ddl)?;
    (t.take("create") && t.take("table")).then_some(())?;
    (t.name()? == *table && t.take("(")).then_some(())?;
    let mut columns = Vec::new();
    loop {
        let name = t.name()?;
        let kind = t.name()?.0;
        if !matches!(
            kind.as_str(),
            "bigint" | "integer" | "smallint" | "tinyint" | "varchar" | "boolean"
        ) {
            return None;
        }
        let nullable = if t.take("not") {
            t.take("null").then_some(())?;
            false
        } else {
            true
        };
        columns.push(Column {
            name,
            kind,
            nullable,
        });
        if !t.take(",") {
            break;
        }
    }
    (t.take(")") && t.done()).then_some(columns)
}
