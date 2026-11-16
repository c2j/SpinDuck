//! Gate 2B（设计 V4 §4.4）：`SchemaSnapshot` → DuckDB stub DDL，
//! 对编译产物中的改写后 SQL 做 `prepare` 校验（`KERNEL-0011`）。
//!
//! V1 口径（保守、不产假阳性）：
//! - 仅对"列集合完全已知"的对象建 stub：普通表、显式列清单视图、序列；
//! - CTAS 延迟表、未知视图、伪表（`sys_dummy`/`dual`）与例程调用下沉
//!   临时表**不建模**——prepare 报错文本命中这些名字的发现按"延迟判定"
//!   抑制（与 Gate 1B 的 DeferredToGate2B/Unknown 口径一致）；
//! - 动态 SQL（`SqlExecDynamic`）无静态文本，不在本门范围。

use std::collections::BTreeSet;

use ogsql_parser::ast::DataType;

use crate::error::EngineError;
use crate::gate::schema::{SchemaSnapshot, ViewDef};
use crate::rules::RuleRegistry;

/// 伪表/引擎内建对象：不参与 stub 建模，引用它们的 prepare 报错一律抑制。
const SKIP_OBJECTS: &[&str] = &["sys_dummy", "dual"];

fn quote_ident(name: &str) -> String {
    name.split('.')
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(".")
}

/// `DataType` → DuckDB 类型名；`Custom` 经 `[types]` 规则解析，
/// 解析不出回退 `VARCHAR`（stub 只要求 binder 可绑定，不要求精确）。
fn duck_type(dt: &DataType, rules: &RuleRegistry) -> String {
    let std = match dt {
        DataType::Boolean => Some("BOOLEAN"),
        DataType::TinyInt(_) | DataType::SmallInt(_) | DataType::Integer(_) => Some("INTEGER"),
        DataType::BigInt(_) | DataType::BigSerial => Some("BIGINT"),
        DataType::Real | DataType::Float(_) | DataType::Double => Some("DOUBLE"),
        DataType::BinaryFloat | DataType::BinaryDouble => Some("DOUBLE"),
        DataType::Numeric(_, _) => Some("DECIMAL"),
        DataType::Char(_) | DataType::Varchar(_) | DataType::Text => Some("VARCHAR"),
        DataType::Bytea => Some("BLOB"),
        DataType::Timestamp(_, _) | DataType::Timestamptz(_) => Some("TIMESTAMP"),
        DataType::Date => Some("DATE"),
        DataType::Time(_, _) => Some("TIME"),
        DataType::Interval(_) => Some("INTERVAL"),
        DataType::Json | DataType::Jsonb => Some("JSON"),
        DataType::Uuid => Some("UUID"),
        DataType::Bit(_) | DataType::Varbit(_) => Some("BIT"),
        DataType::Serial | DataType::SmallSerial => Some("INTEGER"),
        DataType::Array(_) => Some("VARCHAR[]"),
        _ => None,
    };
    if let Some(t) = std {
        return t.to_string();
    }
    if let DataType::Custom(name, _) = dt {
        let raw = name
            .iter()
            .map(|i| i.value.clone())
            .collect::<Vec<_>>()
            .join(".");
        if let Some(st) = rules.spin_type(&raw) {
            return st.duckdb_name().to_string();
        }
    }
    "VARCHAR".to_string()
}

/// 已知对象 → stub DDL（`BTreeMap` 遍历保证确定性顺序）。
pub fn stub_ddl(snap: &SchemaSnapshot, rules: &RuleRegistry) -> Vec<String> {
    let mut out = Vec::new();
    for (name, def) in &snap.tables {
        if def.columns_deferred || def.columns.is_empty() {
            continue;
        }
        let cols = def
            .columns
            .iter()
            .map(|(c, dt)| format!("\"{c}\" {}", duck_type(dt, rules)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!("CREATE TABLE {} ({cols})", quote_ident(name)));
    }
    for (name, def) in &snap.views {
        if let ViewDef::Known(cols) = def {
            let list = cols
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let sel = cols
                .iter()
                .map(|c| format!("NULL AS \"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!(
                "CREATE VIEW {} ({list}) AS SELECT {sel}",
                quote_ident(name)
            ));
        }
    }
    for name in &snap.sequences {
        out.push(format!("CREATE SEQUENCE {}", quote_ident(name)));
    }
    out
}

/// 不建模对象名集合：延迟表 + 未知视图 + 伪表。prepare 报错命中
/// 任一成员（不区分大小写）即抑制——这些对象的列/存在性判定已由
/// Gate 1B 显式延迟，2B 不重复裁决。
pub fn deferred_names(snap: &SchemaSnapshot) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = SKIP_OBJECTS.iter().map(|s| s.to_string()).collect();
    for (name, def) in &snap.tables {
        if def.columns_deferred || def.columns.is_empty() {
            out.insert(name.clone());
        }
    }
    for (name, def) in &snap.views {
        if matches!(def, ViewDef::Unknown) {
            out.insert(name.clone());
        }
    }
    out
}

/// 报错文本是否命中延迟对象（名字以 `"name"` 或词边界出现）。
fn mentions(err_msg: &str, names: &BTreeSet<String>) -> bool {
    let lower = err_msg.to_lowercase();
    names.iter().any(|n| {
        let needle = n.to_lowercase();
        // 最后一段（表名）出现即视为命中：`s.t` 的报错通常只引 `t`
        let leaf = needle.rsplit('.').next().unwrap_or(needle.as_str());
        lower.contains(&format!("\"{leaf}\""))
            || lower.contains(&format!("'{leaf}'"))
            || lower.contains(&format!(" {leaf} "))
            || lower.contains(&format!(" {leaf}."))
            || lower.contains(&format!(".{leaf}"))
            || (needle.contains('.') && lower.contains(&needle))
    })
}

/// Gate 2B 编排（`native`）：建内存 stub 库 → 应用 stub DDL →
/// 逐条 `prepare_check`。`extra_skip` 为例程调用下沉临时表名等
/// 编译产物侧的不建模对象。返回 `(归属文件, KERNEL-0011)` 列表。
#[cfg(feature = "native")]
pub fn prepare_all(
    snap: &SchemaSnapshot,
    rules: &RuleRegistry,
    extra_skip: &BTreeSet<String>,
    sqls: &[(String, String)],
) -> Vec<(String, EngineError)> {
    use crate::exec::SqlExecutor;
    use crate::sql::exec_native::{prepare_check, DuckExecutor};

    let mut deferred = deferred_names(snap);
    deferred.extend(extra_skip.iter().cloned());
    let Ok(ex) = DuckExecutor::open_in_memory() else {
        return Vec::new();
    };
    for ddl in stub_ddl(snap, rules) {
        if ex.execute(&ddl, &crate::exec::BindMap::new()).is_err() {
            // stub DDL 自身失败属引擎缺陷面，不在业务诊断中放大
            continue;
        }
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (file, sql) in sqls {
        if !seen.insert(sql.clone()) {
            continue;
        }
        if let Err(e) = prepare_check(ex.connection(), sql) {
            if mentions(&e.to_string(), &deferred) {
                continue;
            }
            out.push((file.clone(), e));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogsql_parser::ast::Statement;
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;

    fn snap_of(sql: &str) -> SchemaSnapshot {
        let toks = Tokenizer::new(sql).tokenize().unwrap();
        let stmts: Vec<Statement> = Parser::new(toks).parse();
        SchemaSnapshot::from_statements(&stmts).unwrap()
    }

    #[test]
    fn should_emit_stub_ddl_for_known_objects_only() {
        let snap = snap_of(
            "CREATE TABLE s.t1(a INT, b VARCHAR2(10)); \
             CREATE VIEW v1(x, y) AS SELECT a, b FROM s.t1; \
             CREATE VIEW v2 AS SELECT a FROM s.t1; \
             CREATE SEQUENCE seq1; \
             CREATE TABLE tmp AS SELECT a FROM s.t1;",
        );
        let rules = RuleRegistry::default();
        let ddl = stub_ddl(&snap, &rules);
        assert!(ddl.iter().any(|d| d.contains("CREATE TABLE \"s\".\"t1\"")));
        assert!(ddl
            .iter()
            .any(|d| d.contains("CREATE VIEW \"v1\" (\"x\", \"y\")")));
        assert!(ddl.iter().any(|d| d.contains("CREATE SEQUENCE \"seq1\"")));
        // CTAS 延迟表与未知视图不建模
        assert!(!ddl.iter().any(|d| d.contains("\"tmp\"")));
        assert!(!ddl.iter().any(|d| d.contains("\"v2\"")));
    }

    #[test]
    fn should_map_custom_type_via_rules() {
        let snap = snap_of("CREATE TABLE t1(a NUMBER(10,2), b VARCHAR2(20))");
        let mut rules = RuleRegistry::default();
        rules.types.insert("NUMBER".into(), "DECIMAL".into());
        rules.types.insert("VARCHAR2".into(), "VARCHAR".into());
        let ddl = stub_ddl(&snap, &rules);
        assert_eq!(ddl.len(), 1);
        assert!(ddl[0].contains("\"a\" DECIMAL"), "{ddl:?}");
        assert!(ddl[0].contains("\"b\" VARCHAR"), "{ddl:?}");
    }

    #[test]
    fn should_include_deferred_and_unknown_names() {
        let snap = snap_of(
            "CREATE TABLE t1(a INT); CREATE TABLE tmp AS SELECT a FROM t1; \
             CREATE VIEW vu AS SELECT a FROM t1;",
        );
        let names = deferred_names(&snap);
        assert!(names.contains("tmp"));
        assert!(names.contains("vu"));
        assert!(names.contains("sys_dummy"));
        assert!(names.contains("dual"));
        assert!(!names.contains("t1"));
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_prepare_clean_sql_and_suppress_deferred_errors() {
        let snap =
            snap_of("CREATE TABLE t1(a INT, b VARCHAR); CREATE TABLE tmp AS SELECT a FROM t1;");
        let rules = RuleRegistry::default();
        let mut skip = BTreeSet::new();
        skip.insert("__sd_fn_tmp".to_string());
        let sqls = vec![
            ("f1".to_string(), "SELECT a FROM t1".to_string()),
            ("f1".to_string(), "SELECT nope FROM t1".to_string()),
            ("f1".to_string(), "SELECT a FROM tmp".to_string()),
            ("f1".to_string(), "SELECT a FROM __sd_fn_tmp".to_string()),
        ];
        let out = prepare_all(&snap, &rules, &skip, &sqls);
        // 仅真实缺列报 KERNEL-0011；延迟表与下沉临时表的错误被抑制
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, "f1");
        assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
    }
}
