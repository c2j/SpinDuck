//! Gate 2B（设计 V4 §4.4）：`SchemaSnapshot` → DuckDB stub DDL，
//! 对编译产物中的改写后 SQL 做 `prepare` 校验（`KERNEL-0011`）。
//!
//! 口径：
//! - 仅对"列集合完全已知"的对象建 stub：普通表、显式列清单视图、序列；
//!   限定名（`s.t`）先补 `CREATE SCHEMA`；
//! - CTAS 延迟表、未知视图、伪表（`sys_dummy`/`dual`）与例程调用下沉
//!   临时表**不建模**——prepare 报"对象不存在"类错误且对象名命中这些
//!   名字时按"延迟判定"抑制（与 Gate 1B 的 DeferredToGate2B/Unknown
//!   口径一致）；列级/函数级错误不受抑制；
//! - `$v_*` 绑定按 `IrProg.var_types` 替换为 `CAST(NULL AS <类型>)`，
//!   使 Binder 能做重载裁决（设计 §4.4.1）；未知类型保留参数形态；
//! - 动态 SQL（`SqlExecDynamic`）无静态文本，不在本门范围；
//! - stub DDL 自身失败 / 内存库打不开 = 引擎侧缺陷，`Err` 上抛，
//!   不得静默当通过。

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use ogsql_parser::ast::DataType;

use crate::error::EngineError;
use crate::gate::schema::{SchemaSnapshot, ViewDef};
use crate::ir::BindSrc;
use crate::rules::RuleRegistry;
use crate::types::SpinType;

/// 伪表/引擎内建对象：不参与 stub 建模，引用它们的"不存在"类报错抑制。
const SKIP_OBJECTS: &[&str] = &["sys_dummy", "dual"];

fn quote_ident(name: &str) -> String {
    name.split('.')
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
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

/// 已知对象 → stub DDL（`BTreeMap` 遍历保证确定性顺序；限定名先补
/// schema）。schema 来源 = 实际会发出的表 / 已知列视图 / 序列的
/// 限定名——仅有 `s.v`/`s.seq` 而无 `s` 下表时同样先建 schema。
pub fn stub_ddl(snap: &SchemaSnapshot, rules: &RuleRegistry) -> Vec<String> {
    let mut out = Vec::new();
    let mut schemas = BTreeSet::new();
    let mut note_schema = |name: &str| {
        if let Some((s, _)) = name.split_once('.') {
            schemas.insert(s.to_string());
        }
    };
    for (name, def) in &snap.tables {
        if !(def.columns_deferred || def.columns.is_empty()) {
            note_schema(name);
        }
    }
    for (name, def) in &snap.views {
        if matches!(def, ViewDef::Known(_)) {
            note_schema(name);
        }
    }
    for name in &snap.sequences {
        note_schema(name);
    }
    for s in schemas {
        out.push(format!("CREATE SCHEMA IF NOT EXISTS \"{s}\""));
    }
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

/// 不建模对象名集合：延迟表 + 未知视图 + 伪表。prepare 报"对象不存在"
/// 类错误且命中任一成员（不区分大小写）即抑制——这些对象的存在性/列
/// 判定已由 Gate 1B 显式延迟，2B 不重复裁决。
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

/// `needle` 是否以"标识符边界"出现在 `hay` 中：两侧都不能紧邻
/// 标识符字符（`[A-Za-z0-9_]`），左界额外拒绝 `.`——`s.tmp` 不得
/// 命中 `x.s.tmp`，`tmp` 不得命中 `s.tmp`/`tmp_hist`/`dual_acct`。
fn bounded_contains(hay: &str, needle: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        before.is_none_or(|c| !ident(c) && c != '.') && after.is_none_or(|c| !ident(c))
    })
}

/// 名字在错误文本中是否以"对象引用"形态出现：先去掉双引号
/// （`"s"."t1"` → `s.t1`），再按标识符边界匹配限定名整体或叶子名。
/// 刻意**不**匹配 `Candidate bindings` 类候选列回显（列级错误，
/// 不属于对象缺失）。
fn name_is_object_ref(lower_err: &str, needle: &str) -> bool {
    let norm = lower_err.replace('"', "");
    bounded_contains(&norm, needle)
}

/// 报错是否为"延迟对象不存在"：必须同时满足 ① 错误主体是对象缺失
/// （`does not exist`/`not found` 的 catalog 语义）② 点名命中延迟对象。
/// 列级/函数级/语法级错误一律不抑制。
fn mentions(err_msg: &str, names: &BTreeSet<String>) -> bool {
    let lower = err_msg.to_lowercase();
    let object_missing = lower.contains("does not exist")
        || (lower.contains("not found") && lower.contains("table"));
    object_missing
        && names
            .iter()
            .any(|n| name_is_object_ref(&lower, &n.to_lowercase()))
}

/// 一条待 prepare 的改写后 SQL 及其绑定/类型/抑制上下文。
#[derive(Debug, Clone)]
pub struct PrepareStmt {
    /// 归属文件（诊断用）。
    pub file: String,
    /// 改写后 SQL 文本（含 `$v_*` 占位符）。
    pub sql: String,
    /// 绑定表（`$v_*` → BindSrc）。
    pub binds: Vec<(String, BindSrc)>,
    /// 例程标量变量类型（`%ROWTYPE` 与映射表外类型不在此）。
    pub var_types: BTreeMap<String, SpinType>,
    /// 本语句专属的"不建模对象"抑制名：例程调用下沉临时表 +
    /// `[[missing]]` 中**由本例程引用**的 relation（按 `MissingObject.from`
    /// 归属收窄，不把全计划缺失套到每条语句）。
    pub skip: BTreeSet<String>,
}

/// `$v_*` → `CAST(NULL AS <类型>)` 类型化替换（设计 §4.4.1）：
/// 已知类型让 Binder 做重载裁决；记录字段与未知类型保留参数形态
/// （DuckDB 按参数处理，不产生重载假阴性以外的误报）。
///
/// 替换按"最长占位符优先 + 右标识符边界 + 跳过字符串/注释"实现：
/// `$v_v_return_rate` 不得吃掉 `$v_v_return_rate_flag` 的前缀，
/// 字符串字面量里的 `$v_…` 原样保留。
fn typed_sql(stmt: &PrepareStmt) -> String {
    let mut reps: Vec<(&str, String)> = stmt
        .binds
        .iter()
        .filter_map(|(ph, src)| {
            let BindSrc::Var(var) = src else {
                return None;
            };
            let ty = stmt.var_types.get(&var.0.to_ascii_lowercase())?;
            ph.starts_with("$v_")
                .then(|| (ph.as_str(), format!("CAST(NULL AS {})", ty.duckdb_name())))
        })
        .collect();
    if reps.is_empty() {
        return stmt.sql.clone();
    }
    reps.sort_by_key(|(ph, _)| std::cmp::Reverse(ph.len()));
    let sql = stmt.sql.as_str();
    let b = sql.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = String::with_capacity(sql.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            // 字符串字面量（'..' / ".." / `..`，'' 双写转义）整段跳过
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // 行注释 / 块注释整段跳过
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b'$' => {
                // 左界：前一字节不得是标识符/`$`（`a$v_b` 不算占位符）
                let left_ok = i == 0 || (!ident(b[i - 1]) && b[i - 1] != b'$');
                let hit = left_ok.then(|| {
                    reps.iter().find(|(ph, _)| {
                        sql[i..].starts_with(ph)
                            && sql[i + ph.len()..].chars().next().is_none_or(|c| {
                                !(c.is_ascii_alphanumeric() || c == '_' || c == '$')
                            })
                    })
                });
                if let Some(Some((ph, rep))) = hit {
                    out.push_str(&sql[last..i]);
                    out.push_str(rep);
                    i += ph.len();
                    last = i;
                    continue;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[last..]);
    out
}

/// Gate 2B 编排（`native`）：建内存 stub 库 → 应用 stub DDL →
/// 逐条类型化替换后 `prepare_check`。抑制集合 = 快照延迟对象 ∪
/// 逐语句 `skip`（下沉临时表与按例程归属的 `[[missing]]` relation）。
/// stub/开库失败 `Err` 上抛。
#[cfg(feature = "native")]
pub fn prepare_all(
    snap: &SchemaSnapshot,
    rules: &RuleRegistry,
    sqls: &[PrepareStmt],
) -> Result<Vec<(String, EngineError)>, EngineError> {
    prepare_all_impl(snap, rules, sqls, &[])
}

/// `prepare_all` 内部实现；`extra_ddl` 为测试注入缝（stub DDL
/// 失败路径 `ENG-0072` 的守护用例需要一条必失败的 DDL）。
#[cfg(feature = "native")]
fn prepare_all_impl(
    snap: &SchemaSnapshot,
    rules: &RuleRegistry,
    sqls: &[PrepareStmt],
    extra_ddl: &[String],
) -> Result<Vec<(String, EngineError)>, EngineError> {
    use crate::exec::SqlExecutor;
    use crate::sql::exec_native::{prepare_check, DuckExecutor};

    let deferred = deferred_names(snap);
    let ex = DuckExecutor::open_in_memory()
        .map_err(|e| EngineError::kernel(11, format!("gate2b stub db open: {e}")))?;
    for ddl in stub_ddl(snap, rules)
        .into_iter()
        .chain(extra_ddl.iter().cloned())
    {
        ex.execute(&ddl, &crate::exec::BindMap::new())
            .map_err(|e| {
                EngineError::invariant(72, format!("gate2b stub ddl failed: {ddl}: {e}"))
            })?;
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for stmt in sqls {
        let sql = typed_sql(stmt);
        // 去重键 = 类型化后 SQL + 本语句 skip：同文本不同类型/抑制名
        // 的语句必须分别裁决，不能让先到的成功语句吞掉后者。
        if !seen.insert((sql.clone(), stmt.skip.clone())) {
            continue;
        }
        if let Err(e) = prepare_check(ex.connection(), &sql) {
            let mut names = deferred.clone();
            names.extend(stmt.skip.iter().cloned());
            if mentions(&e.to_string(), &names) {
                continue;
            }
            out.push((stmt.file.clone(), e));
        }
    }
    Ok(out)
}

/// 非 native 构建：Gate 2B 显式跳过（不产生诊断；CLI 摘要输出
/// `gate2b=skipped(no-native)`，不假装门禁通过）。
#[cfg(not(feature = "native"))]
pub fn prepare_all(
    _snap: &SchemaSnapshot,
    _rules: &RuleRegistry,
    _sqls: &[PrepareStmt],
) -> Result<Vec<(String, EngineError)>, EngineError> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::VarName;
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
        assert!(ddl.iter().any(|d| d == "CREATE SCHEMA IF NOT EXISTS \"s\""));
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
    fn should_emit_schema_for_qualified_view_and_sequence_only() {
        // 仅有 s.v / s.seq、无 s 下表时也必须先 CREATE SCHEMA
        let snap = snap_of("CREATE VIEW s.v1(x) AS SELECT 1; CREATE SEQUENCE s.seq1;");
        let ddl = stub_ddl(&snap, &RuleRegistry::default());
        assert_eq!(ddl[0], "CREATE SCHEMA IF NOT EXISTS \"s\"", "{ddl:?}");
        assert!(
            ddl.iter().any(|d| d.contains("CREATE VIEW \"s\".\"v1\"")),
            "{ddl:?}"
        );
        assert!(
            ddl.iter()
                .any(|d| d.contains("CREATE SEQUENCE \"s\".\"seq1\"")),
            "{ddl:?}"
        );
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

    #[test]
    fn should_only_suppress_object_missing_errors() {
        let names: BTreeSet<String> = ["tmp".to_string(), "missing_t".to_string()].into();
        // 对象不存在且点名延迟对象 → 抑制
        assert!(mentions(
            "Catalog Error: Table with name tmp does not exist!",
            &names
        ));
        // 列级错误即使回显里出现延迟表名 → 不抑制
        assert!(!mentions(
            "Binder Error: Referenced column \"tmp\" not found in FROM clause!",
            &names
        ));
        // 候选列回显含同名 → 不抑制
        assert!(!mentions("Candidate bindings: \"t1\".\"tmp\"", &names));
        // 不点名任何延迟对象 → 不抑制
        assert!(!mentions(
            "Catalog Error: Table with name other_t does not exist!",
            &names
        ));
    }

    #[test]
    fn should_not_suppress_near_miss_object_names() {
        // 词边界：延迟名 dual/tmp 不得命中 dual_acct/tmp_hist/s.tmp_hist
        let names: BTreeSet<String> =
            ["dual".to_string(), "tmp".to_string(), "s.tmp".to_string()].into();
        for err in [
            "Catalog Error: Table with name ods.dual_acct does not exist!",
            "Catalog Error: Table with name s.tmp_hist does not exist!",
            "Catalog Error: Table with name tmp2 does not exist!",
        ] {
            assert!(!mentions(err, &names), "{err}");
        }
        // 精确命中（含限定名与引号形态）→ 抑制
        for err in [
            "Catalog Error: Table with name tmp does not exist!",
            "Catalog Error: Table with name \"s\".\"tmp\" does not exist!",
            "Catalog Error: Table with name s.tmp does not exist!",
        ] {
            assert!(mentions(err, &names), "{err}");
        }
    }

    #[test]
    fn should_type_placeholder_vars() {
        let stmt = PrepareStmt {
            file: "f".into(),
            sql: "SELECT * FROM t WHERE d = $v_d AND n = $v_n".into(),
            binds: vec![
                ("$v_d".into(), BindSrc::Var(VarName("d".into()))),
                ("$v_n".into(), BindSrc::Var(VarName("n".into()))),
            ],
            var_types: BTreeMap::from([
                ("d".to_string(), SpinType::Date),
                ("n".to_string(), SpinType::Int),
            ]),
            skip: BTreeSet::new(),
        };
        let sql = typed_sql(&stmt);
        assert!(sql.contains("CAST(NULL AS DATE)"), "{sql}");
        assert!(sql.contains("CAST(NULL AS INTEGER)"), "{sql}");
        assert!(!sql.contains("$v_d"), "{sql}");
    }

    #[test]
    fn should_not_corrupt_prefix_placeholders_or_literals() {
        // $v_v_return_rate 是 $v_v_return_rate_flag 的前缀：最长优先 +
        // 右标识符边界，两个占位符必须各自完整替换；字符串里的不动。
        let stmt = PrepareStmt {
            file: "f".into(),
            sql: "SELECT $v_v_return_rate, $v_v_return_rate_flag, '$v_v_return_rate' FROM t".into(),
            binds: vec![
                (
                    "$v_v_return_rate".into(),
                    BindSrc::Var(VarName("v_return_rate".into())),
                ),
                (
                    "$v_v_return_rate_flag".into(),
                    BindSrc::Var(VarName("v_return_rate_flag".into())),
                ),
            ],
            var_types: BTreeMap::from([
                (
                    "v_return_rate".to_string(),
                    SpinType::Decimal {
                        precision: 10,
                        scale: 2,
                    },
                ),
                ("v_return_rate_flag".to_string(), SpinType::Bool),
            ]),
            skip: BTreeSet::new(),
        };
        let sql = typed_sql(&stmt);
        assert_eq!(
            sql, "SELECT CAST(NULL AS DECIMAL), CAST(NULL AS BOOLEAN), '$v_v_return_rate' FROM t",
            "{sql}"
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_prepare_clean_sql_and_suppress_deferred_errors() {
        let snap =
            snap_of("CREATE TABLE t1(a INT, b VARCHAR); CREATE TABLE tmp AS SELECT a FROM t1;");
        let rules = RuleRegistry::default();
        let mk = |sql: &str, skip: &[&str]| PrepareStmt {
            file: "f1".to_string(),
            sql: sql.to_string(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: skip.iter().map(|s| s.to_string()).collect(),
        };
        let sqls = vec![
            mk("SELECT a FROM t1", &[]),
            mk("SELECT nope FROM t1", &[]),
            mk("SELECT a FROM tmp", &[]),
            mk("SELECT a FROM __sd_fn_tmp", &["__sd_fn_tmp"]),
        ];
        let out = prepare_all(&snap, &rules, &sqls).unwrap();
        // 仅真实缺列报 KERNEL-0011；延迟表与下沉临时表的"不存在"错误被抑制
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, "f1");
        assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_prepare_schema_qualified_stub_table() {
        let snap = snap_of("CREATE TABLE s.t1(a INT)");
        let rules = RuleRegistry::default();
        let sqls = vec![PrepareStmt {
            file: "f".into(),
            sql: "SELECT a FROM s.t1".into(),
            binds: Vec::new(),
            var_types: BTreeMap::new(),
            skip: BTreeSet::new(),
        }];
        // schema 已建 → prepare 通过；若漏建 schema 会报 KERNEL-0011
        let out = prepare_all(&snap, &rules, &sqls).unwrap();
        assert!(out.is_empty(), "{out:?}");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_catch_typed_placeholder_overload_error() {
        // $v_d 类型化为 DATE 后，round(date,'MM') 这类重载错误能被 Binder 捕获；
        // 不替换时 DuckDB 把 $v_d 当参数，prepare 不报错（漏报）。
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let rules = RuleRegistry::default();
        let sqls = vec![PrepareStmt {
            file: "f".into(),
            sql: "SELECT round($v_d, 'MM') FROM t1".into(),
            binds: vec![("$v_d".into(), BindSrc::Var(VarName("d".into())))],
            var_types: BTreeMap::from([("d".to_string(), SpinType::Date)]),
            skip: BTreeSet::new(),
        }];
        let out = prepare_all(&snap, &rules, &sqls).unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_judge_same_sql_with_different_types_and_skip() {
        // 同文本不同 var_types：DATE 的 round 重载错误必须上报，
        // 不能被先到的"通过"语句去重吞掉；skip 不同同理。
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let rules = RuleRegistry::default();
        let mk = |ty: SpinType, skip: &[&str]| PrepareStmt {
            file: "f".into(),
            sql: "SELECT round($v_d, 'MM') FROM t1".into(),
            binds: vec![("$v_d".into(), BindSrc::Var(VarName("d".into())))],
            var_types: BTreeMap::from([("d".to_string(), ty)]),
            skip: skip.iter().map(|s| s.to_string()).collect(),
        };
        let sqls = vec![mk(SpinType::Int, &[]), mk(SpinType::Date, &[])];
        let out = prepare_all(&snap, &rules, &sqls).unwrap();
        // Int 版 prepare 通过；Date 版 KERNEL-0011——去重不得跨类型合并
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].1.code.to_string(), "KERNEL-0011");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_fail_stub_ddl_as_eng_0072() {
        // stub DDL 执行失败 = 引擎侧缺陷 ENG-0072，Err 上抛而非空通过
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let rules = RuleRegistry::default();
        let err = prepare_all_impl(&snap, &rules, &[], &["THIS IS NOT VALID SQL".to_string()])
            .unwrap_err();
        assert_eq!(err.code.to_string(), "ENG-0072", "{err:?}");
    }
}
