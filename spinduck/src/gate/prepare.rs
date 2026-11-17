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

use crate::error::EngineError;
use crate::gate::schema::{SchemaSnapshot, ViewDef};
use crate::ir::BindSrc;
use crate::rules::RuleRegistry;
use crate::types::SpinType;

mod ddl;
#[cfg(test)]
mod tests;

/// 伪表/引擎内建对象：不参与 stub 建模，引用它们的"不存在"类报错抑制。
const SKIP_OBJECTS: &[&str] = &["sys_dummy", "dual"];

fn quote_ident(name: &str) -> String {
    name.split('.')
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(".")
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
            .map(|(c, dt)| format!("\"{c}\" {}", crate::types::duckdb_type_name(dt, rules)))
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
        before.is_none_or(|c| !ident(c) && c != '.') && after.is_none_or(|c| !ident(c) && c != '.')
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
    /// 例程体内局部 DDL：`Some` 时在 stub 库**执行**而非 prepare——
    /// `CreateTableAs` 追加 `LIMIT 0`，建出的表供后续语句真实列检查。
    pub ddl_exec: Option<DdlExec>,
}

/// 局部 DDL 的执行属性（来自 `IrProg.local_ddl`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalDdl {
    /// 引号形态限定名（`"s"."t"`）。
    pub name: String,
    /// `CREATE TEMP TABLE`：stub 中建 temp 影子表。
    pub temporary: bool,
    /// `IF NOT EXISTS`：命中已有对象时 CREATE 为空操作。
    pub if_not_exists: bool,
}

/// 例程体内局部 DDL 的 stub 执行形态（对应 `IrProg.local_ddl`）。
/// `DropLocal` 仅对本连接中**成功建表**的名字执行，避免 CREATE 失败
/// （如与全局表重名）后误删 stub 全局表。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DdlExec {
    /// `CREATE [TEMP] TABLE name (cols)`：直接执行。
    CreateTable(LocalDdl),
    /// `CREATE [TEMP] TABLE name AS <select>`：包成子查询追加 `LIMIT 0`。
    CreateTableAs(LocalDdl),
    /// 例程收尾 `DROP TABLE IF EXISTS name`：清理局部表，防止泄漏到
    /// 同 stub 库的后续例程（`sql` 字段携带 DROP 文本）。
    DropLocal(String),
}

impl DdlExec {
    /// CREATE 形态的局部 DDL 属性；DropLocal 返回 `None`。
    fn local_ddl(&self) -> Option<&LocalDdl> {
        match self {
            DdlExec::CreateTable(d) | DdlExec::CreateTableAs(d) => Some(d),
            DdlExec::DropLocal(_) => None,
        }
    }
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
    // catalog epoch：每条局部 DDL 成功执行后递增。去重键必须含 epoch——
    // 同文本 SQL 在不同局部表状态下（例程 A 的 tmp(a) vs 例程 B 的
    // tmp(b)）必须分别裁决，否则后者被先到者的去重吞掉。
    let mut epoch = 0u64;
    // 本连接中成功建表的局部表名：DropLocal 只对它们生效。
    let mut created: BTreeSet<String> = BTreeSet::new();
    for stmt in sqls {
        let sql = typed_sql(stmt);
        // 去重键 = 类型化后 SQL + 本语句 skip + 执行形态 + catalog 版本：
        // 同文本不同类型/抑制名/DDL 形态/表结构的语句必须分别裁决。
        if !seen.insert((sql.clone(), stmt.skip.clone(), stmt.ddl_exec.clone(), epoch)) {
            continue;
        }
        if let Some(kind) = &stmt.ddl_exec {
            // 局部 DDL：stub 库执行建表，后续语句获得真实列检查；
            // 失败走与 prepare 相同的延迟对象抑制，否则 KERNEL-0011。
            // DropLocal 仅在对应 CREATE 成功过时执行——CREATE 失败
            // （如与全局表重名）时绝不能误删 stub 全局表。
            if let DdlExec::DropLocal(name) = kind {
                if !created.contains(name) {
                    continue;
                }
            }
            // 非临时 `IF NOT EXISTS` 且同名对象已可解析 → CREATE 为空
            // 操作：不执行、不记 created——收尾 DROP 绝不能删掉既有
            // （全局 stub）表，后续例程对它的引用必须仍解析得到。
            if let Some(ddl) = kind.local_ddl() {
                if ddl.if_not_exists
                    && !ddl.temporary
                    && prepare_check(
                        ex.connection(),
                        &format!("SELECT * FROM {} LIMIT 0", ddl.name),
                    )
                    .is_ok()
                {
                    continue;
                }
            }
            let exec_sql = match kind {
                DdlExec::CreateTableAs(_) => match ddl::ctas_limit0_sql(&sql) {
                    Some(s) => s,
                    None => {
                        out.push((
                            stmt.file.clone(),
                            EngineError::kernel(11, "gate2b local ctas: cannot locate AS clause"),
                        ));
                        continue;
                    }
                },
                DdlExec::CreateTable(_) | DdlExec::DropLocal(_) => sql.clone(),
            };
            // 残留 `$v_*`（%TYPE/记录字段/映射表外类型）绑 NULL：
            // execute 要求全部参数有值，不能像 prepare_check 那样
            // 容忍未绑定参数；已知类型已在 typed_sql 替换为 CAST。
            let mut binds = crate::exec::BindMap::new();
            for ph in ddl::residual_placeholders(&exec_sql) {
                binds.insert(ph, crate::value::SpinValue::Null);
            }
            if let Err(e) = ex.execute(&exec_sql, &binds) {
                let mut names = deferred.clone();
                names.extend(stmt.skip.iter().cloned());
                if mentions(&e.to_string(), &names) {
                    continue;
                }
                out.push((
                    stmt.file.clone(),
                    EngineError::kernel(11, format!("gate2b local ddl: {e}")),
                ));
            } else {
                epoch += 1;
                match kind {
                    DdlExec::CreateTable(d) | DdlExec::CreateTableAs(d) => {
                        created.insert(d.name.clone());
                    }
                    DdlExec::DropLocal(name) => {
                        created.remove(name);
                    }
                }
            }
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
