//! GaussDB 数据面：`materialize`（源→DuckDB 快照）与 `sync`（回写）。
//!
//! 连接口令一律来自环境变量，不接受命令行传参；未开 `gauss` feature 时两个子命令
//! 都只做拒绝，不静默成功。

// `io_err`/退出码只在 gauss 落地路径使用；未开 feature 时只剩拒绝桩，故逐条 gate。
#[cfg(feature = "gauss")]
use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::args::Command;
use crate::error::EngineError;
use std::io::Write;
use std::path::PathBuf;

pub(super) struct MaterializeParts {
    db: PathBuf,
    tables: Vec<String>,
    where_clause: Option<String>,
    jobs: Option<usize>,
}

pub(super) fn cmd_materialize(cmd: Command) -> MaterializeParts {
    match cmd {
        Command::Materialize {
            db,
            tables,
            where_clause,
            jobs,
        } => MaterializeParts {
            db,
            tables,
            where_clause,
            jobs,
        },
        _ => unreachable!("cmd_materialize only for Materialize"),
    }
}

pub(super) struct SyncParts {
    db: PathBuf,
    table: String,
    cols: Vec<String>,
}

pub(super) fn cmd_sync(cmd: Command) -> SyncParts {
    match cmd {
        Command::Sync { db, table, cols } => SyncParts { db, table, cols },
        _ => unreachable!("cmd_sync only for Sync"),
    }
}

/// GaussDB 连接参数来自环境变量（避免命令行泄露口令，§10.4）。
#[cfg(feature = "gauss")]
fn open_gauss() -> Result<crate::io::GaussConn, EngineError> {
    let host = std::env::var("SPINDUCK_OG_HOST")
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_HOST is required".to_string()))?;
    let port: u16 = std::env::var("SPINDUCK_OG_PORT")
        .unwrap_or_else(|_| "5432".to_string())
        .parse()
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_PORT must be a port number".to_string()))?;
    let user = std::env::var("SPINDUCK_OG_USER").unwrap_or_else(|_| "gaussdb".to_string());
    let password = std::env::var("SPINDUCK_OG_PASSWORD")
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_PASSWORD is required".to_string()))?;
    let dbname = std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".to_string());
    crate::io::GaussConn::connect_parts(&host, port, &user, &password, &dbname)
}

#[cfg(feature = "gauss")]
pub(super) fn materialize_cmd(
    parts: MaterializeParts,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    // 临时 CSV 与目标库同目录（失败诊断现场自包含；成功即删，设计文档 §3.1）
    let workdir = parts
        .db
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    if let [table] = parts.tables.as_slice() {
        // 单表形态：与 V26 语义逐字一致（允许 --where）
        let mut og = open_gauss()?;
        let outcome = og.materialize(&duck, table, parts.where_clause.as_deref(), &workdir)?;
        writeln!(out, "MATERIALIZE {} rows={}", table, outcome.rows).map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    // 多表形态（V27 §3.6）：--jobs 个工作线程，每线程独立 GaussDB 连接 +
    // 独立 DuckDB 连接句柄（同进程同库多连接，MVCC 处理并发写不同表）
    let jobs = parts.jobs.unwrap_or(4).min(parts.tables.len());
    let executors = (0..parts.tables.len())
        .map(|_| {
            let cloned = duck
                .connection()
                .try_clone()
                .map_err(|e| EngineError::env(20, format!("duckdb 连接克隆失败：{e}")))?;
            Ok::<_, EngineError>(crate::sql::DuckExecutor::from_connection(cloned))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let outcomes = crate::io::materialize_parallel(
        jobs,
        parts.tables.clone(),
        &workdir,
        open_gauss,
        executors,
    );
    let mut failed = 0;
    for (table, result) in &outcomes {
        match result {
            Ok(o) => {
                writeln!(out, "MATERIALIZE {} rows={}", table, o.rows).map_err(io_err)?;
            }
            Err(e) => {
                failed += 1;
                // 单表失败不中断其余表（快照批次语义），任一失败整体退出码 1
                writeln!(out, "FAIL {} {}", table, e).map_err(io_err)?;
            }
        }
    }
    if failed > 0 {
        return Ok(EXIT_FAIL);
    }
    Ok(EXIT_OK)
}

#[cfg(not(feature = "gauss"))]
pub(super) fn materialize_cmd(
    parts: MaterializeParts,
    _out: &mut dyn Write,
) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "materialize {} -> {} (where: {:?}) requires feature `gauss`",
            parts.tables.join(","),
            parts.db.display(),
            parts.where_clause
        ),
    ))
}

#[cfg(feature = "gauss")]
pub(super) fn sync_cmd(parts: SyncParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    let mut og = open_gauss()?;
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    let n = og.sync_append(&duck, &parts.table, &parts.cols)?;
    writeln!(out, "SYNC {} rows={}", parts.table, n).map_err(io_err)?;
    Ok(EXIT_OK)
}

#[cfg(not(feature = "gauss"))]
pub(super) fn sync_cmd(parts: SyncParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "sync {} ({}) from {} requires feature `gauss`",
            parts.table,
            parts.cols.join(","),
            parts.db.display()
        ),
    ))
}
