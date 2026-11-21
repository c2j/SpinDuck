//! 编译期门禁（设计文档 V4 §4.4 Gate 2A）：GaussDB/Oracle 方言或
//! DuckDB 1.5.5 不支持的顶层 DDL，以 `Statement` AST 变体判定，
//! 编译期显式拒绝（`ENG-0071`）。判定只看 AST，不做文本/关键字匹配，
//! 字符串字面量与注释中的同名文本不经过此处。

pub mod check;
pub mod columns;
pub mod pipeline;
pub mod prepare;
pub mod schema;

use crate::error::EngineError;
use ogsql_parser::ast::{ObjectName, Statement};

/// 返回被拒绝语句的类别标签；`None` = 放行（常规 DDL 由 load/deploy 通道执行）。
fn denied_kind(st: &Statement) -> Option<&'static str> {
    let kind = match st {
        Statement::CreateTrigger(_) => "CREATE TRIGGER",
        Statement::CreateMaterializedView(_) => "CREATE MATERIALIZED VIEW",
        Statement::CreateDatabaseLink(_) => "CREATE DATABASE LINK",
        Statement::CreateForeignServer(_) | Statement::CreateForeignTable(_) => {
            "CREATE FOREIGN SERVER/TABLE"
        }
        Statement::CreateFdw(_) => "CREATE FDW",
        Statement::CreateTablespace(_) | Statement::AlterTablespace(_) => "TABLESPACE DDL",
        Statement::CreateGlobalIndex(_) => "CREATE GLOBAL INDEX",
        Statement::Grant(_) | Statement::Revoke(_) => "GRANT/REVOKE",
        Statement::CreateRole(_) | Statement::CreateUser(_) | Statement::CreateGroup(_) => {
            "CREATE ROLE/USER/GROUP"
        }
        Statement::CreateSynonym(_) => "CREATE SYNONYM",
        Statement::Listen(_) | Statement::Notify(_) | Statement::Unlisten(_) => "LISTEN/NOTIFY",
        Statement::CreatePublication(_) | Statement::CreateSubscription(_) => {
            "PUBLICATION/SUBSCRIPTION"
        }
        Statement::Rule(_) => "CREATE RULE",
        Statement::CreateCast(_) | Statement::CreateConversion(_) => "CREATE CAST/CONVERSION",
        Statement::CreateDomain(_) | Statement::AlterDomain(_) => "DOMAIN DDL",
        Statement::CreateExtension(_) => "CREATE EXTENSION",
        Statement::CreateDatabase(_) => "CREATE DATABASE",
        Statement::Vacuum(_) | Statement::Cluster(_) | Statement::Reindex(_) => {
            "VACUUM/CLUSTER/REINDEX"
        }
        Statement::Discard(_) | Statement::Checkpoint => "DISCARD/CHECKPOINT",
        Statement::Lock(_) => "LOCK TABLE",
        Statement::Do(_) => "DO block",
        Statement::InsertAll(_) | Statement::InsertFirst(_) => "INSERT ALL/FIRST",
        Statement::CreateAm(_)
        | Statement::CreateDirectory(_)
        | Statement::CreateNode(_)
        | Statement::CreateNodeGroup(_)
        | Statement::CreateResourcePool(_)
        | Statement::CreateWorkloadGroup(_)
        | Statement::CreateAuditPolicy(_)
        | Statement::CreateMaskingPolicy(_)
        | Statement::CreateModel(_) => "GaussDB MPP/admin DDL",
        _ => return None,
    };
    Some(kind)
}

/// Gate 2A 顶层 DDL 拦截：命中拒绝清单时报 `ENG-0071`。
pub(crate) fn reject_unsupported_ddl(st: &Statement) -> Result<(), EngineError> {
    match denied_kind(st) {
        Some(kind) => Err(EngineError::invariant(
            71,
            format!("{kind} is not supported (DuckDB 1.5.5 / GaussDB dialect gate)"),
        )),
        None => Ok(()),
    }
}

/// 例程名归一（ObjectName → 点分小写全名）：`pkg.prc` / 裸 `prc` 两种
/// 书写形态统一成目录键口径（目录键是小写全名）。
pub(crate) fn routine_key(name: &ObjectName) -> String {
    name.iter()
        .map(|i| i.value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(".")
}

/// 体检范围命中：例程名（全名或尾段）在可达集内。尾段近似与
/// `plan::closure::kernel_has` 同口径——闭包写 `pkg.prc`、登记是
/// `pkg.prc`，两侧都截尾比对；范围过滤是"少检"方向，宁可多命中。
pub(crate) fn scope_hit(scope: &std::collections::BTreeSet<String>, name: &ObjectName) -> bool {
    let full = routine_key(name);
    if scope.contains(&full) {
        return true;
    }
    let tail = full.rsplit('.').next().unwrap_or(&full);
    scope.iter().any(|e| e.rsplit('.').next() == Some(tail))
}

/// 语句声明的表对象名（`CreateTable`/`CreateTableAs`，含例程体内
/// `SqlStatement` 下钻形态）：Gate 1B 将其视为本语句局部声明，
/// 对象已知、列集合延迟到 Gate 2B。顶层永久表与全局快照同名时
/// 语义一致，重复登记无害。
pub(crate) fn declared_table_key(st: &Statement) -> Option<String> {
    match st {
        Statement::CreateTable(t) => Some(schema::object_key(&t.name)),
        Statement::CreateTableAs(t) => Some(schema::object_key(&t.name)),
        _ => None,
    }
}
