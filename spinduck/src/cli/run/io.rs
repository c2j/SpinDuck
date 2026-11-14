//! GaussDB 数据面：`materialize`（源→DuckDB 快照）与 `sync`（回写）。
//!
//! 连接口令一律来自环境变量，不接受命令行传参；未开 `gauss` feature 时两个子命令
//! 都只做拒绝，不静默成功。

// `io_err`/退出码只在 gauss 落地路径使用；未开 feature 时只剩拒绝桩，故逐条 gate。
#[cfg(feature = "gauss")]
use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::args::Command;
use crate::error::EngineError;
#[cfg(feature = "gauss")]
use crate::io::MaterializeOutcome;
use std::io::Write;
#[cfg(feature = "gauss")]
use std::path::Path;
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
    where_clause: Option<String>,
    mode: Option<String>,
    slice: Option<String>,
    key: Option<String>,
}

pub(super) fn cmd_sync(cmd: Command) -> SyncParts {
    match cmd {
        Command::Sync {
            db,
            table,
            cols,
            where_clause,
            mode,
            slice,
            key,
        } => SyncParts {
            db,
            table,
            cols,
            where_clause,
            mode,
            slice,
            key,
        },
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

/// 临时 CSV 落盘目录（#19）：与目标库同目录（失败诊断现场自包含，设计 §7）。
/// 裸文件名 `--db local.duckdb` 时 `Path::parent()` 返回 `Some("")` 而非
/// `None`，空目录统一落 CWD（`create_dir_all("")` 会隐式成功并掩盖该分叉）。
#[cfg(feature = "gauss")]
pub(super) fn materialize_workdir(db: &Path) -> PathBuf {
    match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    }
}

/// 降级可观测（#17）：回落 INSERT 时往 stderr 写 WARN——stdout 契约不变
/// （设计 §3.4），而「回落 = GB 级整表内存驻留」这条风险的唯一预警通道
/// 不能是静默的。TraceSink 分时事件落地后 stderr 只留摘要。
#[cfg(feature = "gauss")]
fn warn_degraded(table: &str, degraded: Option<&str>) {
    if let Some(reason) = degraded {
        eprintln!("WARN materialize {table}: {reason}，已回落 INSERT 批量路径");
    }
}

/// 多表形态逐表进度行：`[k/total]` 前缀 + 与单表契约同形的行主体；
/// 返回该行是否失败。完成即输出（V27 §3.6：不再等全部结束才有输出）。
#[cfg(feature = "gauss")]
fn report_one(
    out: &mut dyn Write,
    k: usize,
    total: usize,
    item: &(String, Result<MaterializeOutcome, EngineError>),
) -> std::io::Result<bool> {
    match &item.1 {
        Ok(o) => writeln!(out, "[{k}/{total}] MATERIALIZE {} rows={}", item.0, o.rows)?,
        Err(e) => writeln!(out, "[{k}/{total}] FAIL {} {e}", item.0)?,
    }
    Ok(item.1.is_err())
}

#[cfg(feature = "gauss")]
pub(super) fn materialize_cmd(
    parts: MaterializeParts,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let workdir = materialize_workdir(&parts.db);
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    if let [table] = parts.tables.as_slice() {
        // 单表形态：与 V26 语义逐字一致（允许 --where）
        let mut og = open_gauss()?;
        let outcome = og.materialize(&duck, table, parts.where_clause.as_deref(), &workdir)?;
        warn_degraded(table, outcome.degraded.as_deref());
        writeln!(out, "MATERIALIZE {} rows={}", table, outcome.rows).map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    materialize_multi(&parts, &duck, &workdir, out)
}

/// 多表形态（V27 §3.6）：--jobs 个工作线程，每线程独立 GaussDB 连接 + 独立
/// DuckDB 连接句柄（同进程同库多连接，MVCC 处理并发写不同表）。
///
/// 调度只有 io 层一份；本函数只组装批次并提供完成序渲染回调（§1「CLI 仅壳」）。
#[cfg(feature = "gauss")]
fn materialize_multi(
    parts: &MaterializeParts,
    duck: &crate::sql::DuckExecutor,
    workdir: &Path,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let executors = (0..parts.tables.len())
        .map(|_| {
            let cloned = duck
                .connection()
                .try_clone()
                .map_err(|e| EngineError::env(20, format!("duckdb 连接克隆失败：{e}")))?;
            Ok::<_, EngineError>(crate::sql::DuckExecutor::from_connection(cloned))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let total = parts.tables.len();
    let mut render_err: Option<EngineError> = None;
    let outcomes = crate::io::materialize_parallel_with_progress(
        crate::io::MaterializeBatch {
            jobs: parts.jobs.unwrap_or(4),
            tables: parts.tables.clone(),
            workdir,
        },
        open_gauss,
        executors,
        |k, total, item| {
            if render_err.is_some() {
                return;
            }
            // 降级预警与进度行同序：stderr 只留摘要，stdout 契约不变（#17）
            if let Ok(o) = &item.1 {
                warn_degraded(&item.0, o.degraded.as_deref());
            }
            if let Err(e) = report_one(out, k, total, item) {
                render_err = Some(io_err(e));
            }
        },
    );
    if let Some(e) = render_err {
        return Err(e);
    }
    // 结果计数兜底：少一条结果就不得被判成成功（单表失败仍由 failed 计数退出 1）
    if outcomes.len() != total {
        return Err(EngineError::env(
            20,
            format!("多表物化结果缺失：收到 {} / 期望 {}", outcomes.len(), total),
        ));
    }
    let failed = outcomes
        .iter()
        .filter(|(_, result)| result.is_err())
        .count();
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
    let mode = crate::io::sync::resolve_mode(
        parts.mode.as_deref(),
        parts.slice.as_deref(),
        parts.key.as_deref(),
        parts.where_clause.as_deref(),
    )?;
    // 临时 CSV 与目标库同目录（失败诊断现场自包含；成功即删，设计 §4.1）
    let workdir = materialize_workdir(&parts.db);
    let mut og = open_gauss()?;
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    let oc = og.sync_write(&duck, &parts.table, &parts.cols, &mode, &workdir)?;
    // 缺省形态（无 mode/where）保持 V26 契约逐字不变
    if parts.mode.is_none() && parts.where_clause.is_none() {
        writeln!(out, "SYNC {} rows={}", parts.table, oc.rows).map_err(io_err)?;
    } else {
        writeln!(
            out,
            "SYNC {} mode={} rows={}",
            parts.table, oc.mode_label, oc.rows
        )
        .map_err(io_err)?;
    }
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

#[cfg(all(test, feature = "gauss"))]
mod tests {
    use super::*;
    use crate::io::{MaterializeMode, MaterializeOutcome};

    /// V27 §3.6 进度输出：多表形态每表完成即输出一行 `[k/total]` 前缀行
    /// （完成序，不再等全部结束），行主体与单表契约同形。
    #[test]
    fn should_report_per_table_progress_prefix_and_fail_lines() {
        let mut buf: Vec<u8> = Vec::new();
        let results = [
            (
                "t_a".to_string(),
                Ok(MaterializeOutcome {
                    rows: 630468,
                    mode: MaterializeMode::CopyStream,
                    degraded: None,
                }),
            ),
            (
                "t_bad".to_string(),
                Err(EngineError::env(20, String::from("源端缺表"))),
            ),
            (
                "t_c".to_string(),
                Ok(MaterializeOutcome {
                    rows: 312,
                    mode: MaterializeMode::CopyStream,
                    degraded: None,
                }),
            ),
        ];
        let total = results.len();
        let mut failed = 0;
        for (k, item) in results.iter().enumerate() {
            if report_one(&mut buf, k + 1, total, item).unwrap() {
                failed += 1;
            }
        }
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(failed, 1);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[1/3] MATERIALIZE t_a rows=630468", "{text}");
        assert!(
            lines[1].starts_with("[2/3] FAIL t_bad [ENV-0020]"),
            "{text}"
        );
        assert_eq!(lines[2], "[3/3] MATERIALIZE t_c rows=312", "{text}");
    }

    #[test]
    fn should_put_workdir_beside_db_when_db_has_parent_dir() {
        // Path::parent() 对带目录的路径返回目录段本身
        let db = std::path::Path::new("run/local.duckdb");
        assert_eq!(materialize_workdir(db), std::path::PathBuf::from("run"));
    }

    #[test]
    fn should_fall_back_to_cwd_when_db_is_bare_filename() {
        // #19 回归守护：裸文件名的 parent() 是 Some("") 而非 None，
        // 旧行为 create_dir_all("") 隐式成功、CSV 落 CWD 而非库旁
        let db = std::path::Path::new("local.duckdb");
        assert_eq!(materialize_workdir(db), std::path::PathBuf::from("."));
    }

    #[test]
    fn should_fall_back_to_cwd_when_db_has_no_parent() {
        let db = std::path::Path::new("/");
        assert_eq!(materialize_workdir(db), std::path::PathBuf::from("."));
    }
}
