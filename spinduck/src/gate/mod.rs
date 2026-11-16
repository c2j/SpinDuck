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
use ogsql_parser::ast::Statement;

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
        Statement::Merge(_) => "MERGE INTO",
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
