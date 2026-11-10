//! `query` 子命令壳（design §6.2 / §7 A5 本地半边）：跑一条 SQL，把结果集落成快照文件。
//!
//! 语句切分与快照 JSON 约定全在 `crate::snapshot`；本文件只做壳（§1）。两处顺序都是
//! 契约：读 SQL 文件排在开库之前（DuckDB 的 `open` 即建文件，读不到输入还开库就留下
//! 一个空库），写 `--out` 排在查询成功之后（半份快照会被下一步 `compare` 判成
//! "两边都空 ⇒ 一致"，比不写更危险）。

use crate::cli::run::EXIT_OK;
use crate::error::EngineError;
use crate::snapshot::{self, Snapshot};
use std::io::Write;
use std::path::Path;

/// `spinduck query <本地 DuckDB 文件> --sql-file <F.sql> --out <F.json>`。
pub fn query_cmd(
    db: &Path,
    sql_file: &Path,
    out_path: &Path,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let text = crate::cli::read_source_strict(sql_file)?;
    let snap = execute(db, &text)?;
    std::fs::write(out_path, snap.to_json_string())
        .map_err(|e| EngineError::env(30, format!("写 {}: {e}", out_path.display())))?;
    writeln!(
        out,
        "query: rows={} cols={} out={}",
        snap.rows.len(),
        snap.schema.len(),
        out_path.display()
    )
    .map_err(io_err)?;
    Ok(EXIT_OK)
}

#[cfg(feature = "native")]
fn execute(db: &Path, text: &str) -> Result<Snapshot, EngineError> {
    let ex = crate::sql::DuckExecutor::open(&db.to_string_lossy())?;
    snapshot::capture(&ex, text)
}

#[cfg(not(feature = "native"))]
fn execute(db: &Path, _text: &str) -> Result<Snapshot, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "query 对 {} 执行需要 feature `native`（快照来自内核给回的结果集）",
            db.display()
        ),
    ))
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}
