//! 编译期 codeweb 结构快照与规范化指纹；本模块不触碰 DuckDB。

use crate::rules::RuleRegistry;
use crate::types::SpinType;
use codeweb::embed::{ConstraintSchemaExport, IndexSchemaExport, SchemaExport, TableSchemaExport};

/// 一次编译期结构证据；`diagnostics` 非空或表标志不完备时不得生成集合化计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSnapshot {
    /// 按源文件顺序导出的表。
    pub tables: Vec<TableMetadata>,
    /// codeweb 保留的读/解析/未建模 DDL 诊断。
    pub diagnostics: Vec<String>,
    /// 规范化结构指纹；运行期漂移复核使用同一口径。
    pub fingerprint: String,
}

/// 单表结构证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMetadata {
    /// 限定名分量（未引用标识符已折叠小写）。
    pub name: Vec<String>,
    /// 有序列。
    pub columns: Vec<ColumnMetadata>,
    /// 内联及表级约束原文。
    pub constraints: Vec<ConstraintSchemaExport>,
    /// 独立索引原文。
    pub indexes: Vec<IndexSchemaExport>,
    /// 独立索引是否完整枚举。
    pub indexes_complete: bool,
    /// 是否存在未建模结构语义。
    pub has_opaque_ddl: bool,
}

/// 列的声明与规范化类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMetadata {
    /// 列名。
    pub name: String,
    /// 源声明类型（保留精度/长度）。
    pub source_type: String,
    /// 经规则归一化后的引擎类型；None 表示未证明，编译期必须拒绝。
    pub canonical_type: Option<SpinType>,
    /// 是否允许 NULL。
    pub nullable: bool,
    /// 默认表达式原文。
    pub default_expr_raw: Option<String>,
}

impl SchemaSnapshot {
    /// 从 codeweb 静态导出构建快照；不读取图存储、不连接内核。
    pub fn from_codeweb(export: &SchemaExport, rules: &RuleRegistry) -> Self {
        let tables: Vec<_> = export
            .tables
            .iter()
            .map(|t| table_metadata(t, rules))
            .collect();
        let fingerprint = fingerprint(&tables, &export.diagnostics, rules);
        Self {
            tables,
            diagnostics: export.diagnostics.clone(),
            fingerprint,
        }
    }

    /// 按限定名分量精确查找表。
    pub fn table(&self, name: &[&str]) -> Option<&TableMetadata> {
        self.tables
            .iter()
            .find(|t| t.name.iter().map(String::as_str).eq(name.iter().copied()))
    }

    /// 按尾段名查找表（运行期漂移复核用；编译期导出未带库/模式分量）。
    pub(crate) fn table_by_last(&self, name: &str) -> Option<&TableMetadata> {
        self.tables
            .iter()
            .find(|t| t.name.last().map(String::as_str) == Some(name))
    }
}

impl TableMetadata {
    /// 跨阶段结构键：编译期（codeweb）与运行期（duckdb_*）同口径生成。
    /// 约束/索引只计存在性——受限形态本就零约束零索引，出现即漂移；
    /// 声明原文两侧渲染不同，逐字比对会误报（设计 §4.1 归一化契约）。
    pub(crate) fn structural_key(&self) -> String {
        let mut parts: Vec<String> = self
            .columns
            .iter()
            .map(|c| {
                format!(
                    "{}|{}|{}|{}",
                    c.name,
                    type_key(c),
                    c.nullable,
                    super::defaults::key(c.default_expr_raw.as_deref().unwrap_or_default())
                )
            })
            .collect();
        parts.push(format!(
            "restraints:{}",
            !self.constraints.is_empty() || !self.indexes.is_empty()
        ));
        parts.join("\n")
    }
}

fn table_metadata(table: &TableSchemaExport, rules: &RuleRegistry) -> TableMetadata {
    TableMetadata {
        name: table.name.clone(),
        columns: table
            .columns
            .iter()
            .map(|c| ColumnMetadata {
                name: c.name.clone(),
                source_type: c.source_type.clone(),
                canonical_type: canonical_type(&c.source_type, rules),
                nullable: c.nullable,
                default_expr_raw: c.default_expr_raw.clone(),
            })
            .collect(),
        constraints: table.constraints.clone(),
        indexes: table.indexes.clone(),
        indexes_complete: table.indexes_complete,
        has_opaque_ddl: table.has_opaque_ddl,
    }
}

pub(super) fn canonical_type(source: &str, rules: &RuleRegistry) -> Option<SpinType> {
    // 表 DDL 的裸 NUMERIC 使用内核默认精度，不是 PL 变量的 NUMBER(38,6)。
    if matches!(
        source.trim().to_ascii_uppercase().as_str(),
        "NUMERIC" | "DECIMAL"
    ) {
        return Some(SpinType::Decimal {
            precision: 18,
            scale: 3,
        });
    }
    if let Some(ty) = SpinType::from_gauss_name(source, &rules.types) {
        return Some(ty);
    }
    // DuckDB 原生类型名不在 GaussDB [types] 映射中，但规范化口径必须一致。
    let upper = source.trim().to_ascii_uppercase();
    if let Some(base) = upper.strip_suffix(" WITHOUT TIME ZONE") {
        if matches!(base.trim(), "TIMESTAMP" | "TIMESTAMP(6)") {
            return SpinType::from_gauss_name("TIMESTAMP", &rules.types);
        }
    }
    let head = upper.split('(').next().unwrap_or_default().trim();
    match head {
        "VARCHAR" | "CHARACTER VARYING" => Some(SpinType::Varchar),
        "INTEGER" | "INT" | "INT4" => Some(SpinType::Int),
        "BIGINT" | "INT8" => Some(SpinType::BigInt),
        "BOOLEAN" | "BOOL" => Some(SpinType::Bool),
        _ => None,
    }
}

fn type_key(column: &ColumnMetadata) -> String {
    match &column.canonical_type {
        Some(SpinType::Decimal { precision, scale }) => format!("DECIMAL({precision},{scale})"),
        Some(ty) => ty.duckdb_name().to_string(),
        None => format!("OPAQUE:{}", column.source_type.trim().to_ascii_uppercase()),
    }
}

fn fingerprint(tables: &[TableMetadata], diagnostics: &[String], rules: &RuleRegistry) -> String {
    let mut parts = vec![format!("duckdb={}", rules.target_duckdb)];
    for (name, mapped) in &rules.types {
        parts.push(format!("type:{name}={mapped}"));
    }
    let mut sorted: Vec<_> = tables.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for table in sorted {
        parts.push(format!("table:{}", table.name.join(".")));
        parts.push(format!(
            "complete:{},{}",
            table.indexes_complete, table.has_opaque_ddl
        ));
        for column in &table.columns {
            parts.push(format!(
                "col:{}|{}|{}|{}",
                column.name,
                type_key(column),
                column.nullable,
                super::defaults::key(column.default_expr_raw.as_deref().unwrap_or_default())
            ));
        }
        let mut constraints: Vec<_> = table
            .constraints
            .iter()
            .map(|c| format!("{:?}|{}", c.kind, c.declaration_sql))
            .collect();
        constraints.sort();
        parts.extend(constraints);
        let mut indexes: Vec<_> = table
            .indexes
            .iter()
            .map(|i| format!("{}|{}", i.unique, i.definition_sql))
            .collect();
        indexes.sort();
        parts.extend(indexes);
    }
    let mut diagnostics = diagnostics.to_vec();
    diagnostics.sort();
    parts.extend(diagnostics.into_iter().map(|d| format!("diag:{d}")));
    parts.join("\n")
}
