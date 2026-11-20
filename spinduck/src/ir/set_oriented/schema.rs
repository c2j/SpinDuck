//! 当前事务内的表结构证据：原生标量表，解析 DEFAULT 子句为常量/非常量证据
//! （省略列准入与漂移比对），拒绝复杂约束（PK/CHECK/COLLATE 等）。

use super::metadata::SchemaSnapshot;
use super::shape::{Name, Tokens};
use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor};
use crate::rules::RuleRegistry;
use crate::value::SpinValue;

/// DEFAULT 子句证据（设计 §4.1 A3）：省略列由 DuckDB 施加默认值，仅纯常量
/// 默认（字面量/NULL/常量 CAST）可证批写与逐行写同值；非常量默认（序列、
/// 时间戳、函数）省略即改语义。携带规范化文本供运行期漂移逐字比对。
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Default {
    PureConst(String),
    Opaque(String),
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Column {
    pub name: Name,
    pub kind: String,
    pub nullable: bool,
    pub default: Option<Default>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Table {
    pub qualified: String,
    pub columns: Vec<Column>,
}

pub(super) enum Source<'a> {
    Runtime(&'a dyn SqlExecutor),
    Static(&'a SchemaSnapshot),
}

impl Source<'_> {
    pub(super) fn load(&self, name: &Name) -> Result<Option<Table>, EngineError> {
        match self {
            Self::Runtime(ex) => load(*ex, name),
            Self::Static(snapshot) => Ok(static_table(snapshot, name)),
        }
    }

    /// 视图定义原文（§3.5 V3-5 全定义证明输入）；非视图 = None。
    pub(super) fn view_sql(&self, name: &Name) -> Result<Option<String>, EngineError> {
        match self {
            Self::Runtime(ex) => view_sql(*ex, name),
            Self::Static(_) => Ok(None),
        }
    }

    pub(super) fn sys_dummy_ok(&self) -> Result<bool, EngineError> {
        match self {
            Self::Runtime(ex) => sys_dummy_ok(*ex),
            // 数据基数不是静态事实，必须延迟到区域入口检查。
            Self::Static(_) => Ok(true),
        }
    }

    /// verbatim 游标输出列 schema 探测：`DESCRIBE` 改写后（绑定占位符与
    /// fn_site 缓存子查询置 NULL）的原文查询。Static 快照无执行器 → None，
    /// 调用方按不可证处理。探测失败同样返回 None（保守回退而非报错）。
    pub(super) fn probe_columns(&self, sql: &str) -> Result<Option<Vec<Column>>, EngineError> {
        match self {
            Self::Runtime(ex) => probe_columns(*ex, sql),
            Self::Static(_) => Ok(None),
        }
    }

    pub(super) fn matches_snapshot(
        &self,
        snapshot: &SchemaSnapshot,
        name: &Name,
        rules: &RuleRegistry,
    ) -> Result<bool, EngineError> {
        match self {
            Self::Runtime(ex) => matches_snapshot(*ex, snapshot, name, rules),
            Self::Static(_) => Ok(static_table(snapshot, name).is_some()),
        }
    }
}

fn static_table(snapshot: &SchemaSnapshot, name: &Name) -> Option<Table> {
    if !snapshot.diagnostics.is_empty() {
        return None;
    }
    let matches: Vec<_> = snapshot
        .tables
        .iter()
        .filter(|table| table.name.last() == Some(&name.0))
        .collect();
    let [table] = matches.as_slice() else {
        return None;
    };
    if table.name.len() != 1
        || !table.indexes_complete
        || table.has_opaque_ddl
        || !table.constraints.is_empty()
        || !table.indexes.is_empty()
    {
        return None;
    }
    let mut fields = Vec::new();
    for column in &table.columns {
        let kind = match column.canonical_type.as_ref()? {
            crate::types::SpinType::Decimal { precision, scale } => {
                format!("DECIMAL({precision},{scale})")
            }
            ty => ty.duckdb_name().to_string(),
        };
        let default = column
            .default_expr_raw
            .as_ref()
            .map(|value| format!(" DEFAULT {value}"))
            .unwrap_or_default();
        let nullable = if column.nullable { "" } else { " NOT NULL" };
        fields.push(format!(
            "{} {kind}{default}{nullable}",
            Name(column.name.clone()).sql()
        ));
    }
    let ddl = format!("CREATE TABLE {} ({})", name.sql(), fields.join(", "));
    Some(Table {
        qualified: name.sql(),
        columns: columns(&ddl, name)?,
    })
}

/// 入口守卫：只对无同名视图遮蔽的当前库/模式普通表核验恰一行，其他情况回退。
pub(super) fn sys_dummy_ok(ex: &dyn SqlExecutor) -> Result<bool, EngineError> {
    let binds = BindMap::from([("$table".into(), SpinValue::Str("sys_dummy".into()))]);
    let mut stream = ex.query(
        "SELECT COUNT(*) FROM duckdb_tables() WHERE lower(table_name) = $table \
         AND database_name = current_database() AND schema_name = current_schema() \
         AND NOT EXISTS (SELECT 1 FROM duckdb_views() WHERE lower(view_name) = $table)",
        &binds,
    )?;
    let present = matches!(
        stream.fetch_many(1).first().and_then(|r| r.first()),
        Some(SpinValue::Int(1))
    );
    if !present {
        return Ok(false);
    }
    let mut count = ex.query("SELECT COUNT(*) FROM sys_dummy", &BindMap::new())?;
    Ok(matches!(
        count.fetch_many(1).first().and_then(|r| r.first()),
        Some(SpinValue::Int(1))
    ))
}

/// 编译期快照漂移复核（设计 §4.1/§4.5）：运行期自 duckdb_columns() /
/// duckdb_constraints() / duckdb_indexes() 构建与编译期同口径的结构键；
/// 表缺失或键不一致 = 漂移，调用方无损退回。
pub(super) fn matches_snapshot(
    ex: &dyn SqlExecutor,
    snapshot: &SchemaSnapshot,
    name: &Name,
    rules: &RuleRegistry,
) -> Result<bool, EngineError> {
    let Some(table) = snapshot.table_by_last(&name.0) else {
        return Ok(false);
    };
    Ok(runtime_structural_key(ex, name, rules)? == table.structural_key())
}

fn runtime_structural_key(
    ex: &dyn SqlExecutor,
    name: &Name,
    rules: &RuleRegistry,
) -> Result<String, EngineError> {
    let binds = BindMap::from([("$table".into(), SpinValue::Str(name.0.clone()))]);
    let mut stream = ex.query(
        "SELECT column_name, data_type, is_nullable, column_default \
         FROM duckdb_columns() WHERE lower(table_name) = $table ORDER BY column_index",
        &binds,
    )?;
    let mut parts = Vec::new();
    for row in stream.fetch_many(1024) {
        let [SpinValue::Str(col), SpinValue::Str(ty), SpinValue::Bool(nullable), default] =
            row.as_slice()
        else {
            return Ok(String::new());
        };
        let default = match default {
            SpinValue::Str(s) => s.clone(),
            SpinValue::Null => String::new(),
            _ => return Ok(String::new()),
        };
        parts.push(format!(
            "{col}|{}|{nullable}|{}",
            canonical_runtime_type(ty, rules),
            super::defaults::key(&default)
        ));
    }
    if parts.is_empty() {
        return Ok(String::new());
    }
    let mut restraints = ex.query(
        "SELECT (SELECT COUNT(*) FROM duckdb_constraints() WHERE lower(table_name) = $table \
         AND constraint_type <> 'NOT NULL') \
         + (SELECT COUNT(*) FROM duckdb_indexes() WHERE lower(table_name) = $table)",
        &binds,
    )?;
    let has = matches!(
        restraints.fetch_many(1).first().and_then(|r| r.first()),
        Some(SpinValue::Int(n)) if *n > 0
    );
    parts.push(format!("restraints:{has}"));
    Ok(parts.join("\n"))
}

/// 运行期 data_type → 与编译期 type_key 逐字一致的规范串。
fn canonical_runtime_type(ty: &str, rules: &RuleRegistry) -> String {
    let upper = ty.trim().to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix("DECIMAL(") {
        let dims: Vec<&str> = rest
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .collect();
        if let [p, s] = dims.as_slice() {
            return format!("DECIMAL({p},{s})");
        }
    }
    match super::metadata::canonical_type(ty, rules) {
        Some(canonical) => match canonical {
            crate::types::SpinType::Decimal { precision, scale } => {
                format!("DECIMAL({precision},{scale})")
            }
            other => other.duckdb_name().to_string(),
        },
        None => format!("OPAQUE:{}", upper),
    }
}

/// 视图定义原文（duckdb_views().sql，限当前库/模式）；非视图或不存在 = None。
pub(super) fn view_sql(ex: &dyn SqlExecutor, name: &Name) -> Result<Option<String>, EngineError> {
    let binds = BindMap::from([("$table".into(), SpinValue::Str(name.0.clone()))]);
    let mut stream = ex.query(
        "SELECT sql FROM duckdb_views() WHERE lower(view_name) = $table \
         AND database_name = current_database() AND schema_name = current_schema()",
        &binds,
    )?;
    let rows = stream.fetch_many(2);
    let [row] = rows.as_slice() else {
        return Ok(None);
    };
    match row.as_slice() {
        [SpinValue::Str(sql)] => Ok(Some(sql.clone())),
        _ => Ok(None),
    }
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

/// verbatim 游标输出列探测：`DESCRIBE (<改写后原文>)` 返回
/// (column_name, column_type, null, …) 行；词法化/查询/行形态任一
/// 不符即 None（调用方按不可证回退）。列名/kind 小写归一与 Tokens
/// 口径一致；kind 原样保留 `decimal(p,s)` 供 Domain/守卫证明。
fn probe_columns(ex: &dyn SqlExecutor, sql: &str) -> Result<Option<Vec<Column>>, EngineError> {
    let Some(rewritten) = super::shape::probe_sql(sql) else {
        return Ok(None);
    };
    let mut stream = match ex.query(&format!("DESCRIBE ({rewritten})"), &BindMap::new()) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let mut columns = Vec::new();
    loop {
        let chunk = stream.fetch_many(1024);
        if chunk.is_empty() {
            break;
        }
        for row in chunk {
            let [SpinValue::Str(name), SpinValue::Str(kind), SpinValue::Str(nullable), ..] =
                row.as_slice()
            else {
                return Ok(None);
            };
            columns.push(Column {
                name: Name(name.to_ascii_lowercase()),
                kind: kind.to_ascii_lowercase(),
                nullable: nullable == "YES",
                default: None,
            });
        }
    }
    Ok(Some(columns))
}

fn column_name(t: &mut Tokens) -> Option<Name> {
    let word = t.peek()?.to_string();
    if let Some(inner) = word.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let name = Name(inner.replace("\"\"", "\""));
        t.take(&word);
        Some(name)
    } else {
        t.name()
    }
}

fn columns(ddl: &str, table: &Name) -> Option<Vec<Column>> {
    let mut t = Tokens::new(ddl)?;
    (t.take("create") && t.take("table")).then_some(())?;
    (column_name(&mut t)? == *table && t.take("(")).then_some(())?;
    let mut columns = Vec::new();
    loop {
        let name = column_name(&mut t)?;
        let mut kind = t.name()?.0;
        if t.take("(") {
            // 仅 DECIMAL 接受精度/标度参数（guard 的 TRY_CAST 目标类型证据）
            if kind != "decimal" {
                return None;
            }
            let p = t.number()?;
            let s = if t.take(",") { t.number()? } else { "0".into() };
            if !t.take(")") {
                return None;
            }
            kind = format!("decimal({p},{s})");
        }
        if !matches!(
            kind.as_str(),
            "bigint"
                | "integer"
                | "smallint"
                | "tinyint"
                | "varchar"
                | "boolean"
                | "double"
                | "timestamp"
                | "time"
        ) && !kind.starts_with("decimal(")
        {
            return None;
        }
        let default = if t.take("default") {
            Some(super::defaults::classify(&t.default_tokens()?))
        } else {
            None
        };
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
            default,
        });
        if !t.take(",") {
            break;
        }
    }
    (t.take(")") && t.done()).then_some(columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_load_static_purchase_tables_with_all_declared_columns() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("codeweb.toml"),
            "[project]\nname = 'purchase'\n[analysis]\npaths = ['schema.sql']\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("schema.sql"),
            include_str!("../../../../golden/case_corpus_purchase/sql/ddl/tables.sql"),
        )
        .unwrap();
        let export = codeweb::embed::EmbeddedProject::open_exact(tmp.path())
            .unwrap()
            .export_table_schemas()
            .unwrap();
        let snapshot = SchemaSnapshot::from_codeweb(&export, &RuleRegistry::default());
        for name in [
            "mid_yjqs_detail",
            "dat_trd_repurchase",
            "par_fund_partner",
            "par_fund_seat",
            "par_sys_purchase",
            "par_fund_seat_para",
            "swh_all_kind",
        ] {
            let expected = snapshot.table(&[name]).unwrap();
            let table = static_table(&snapshot, &Name(name.into()))
                .unwrap_or_else(|| panic!("{name}: {expected:?}"));
            assert_eq!(
                table
                    .columns
                    .iter()
                    .map(|c| c.name.0.as_str())
                    .collect::<Vec<_>>(),
                expected
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }
}
