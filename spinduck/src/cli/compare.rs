//! `compare` 子命令壳（design §6.2 / §7 A5 本地半边）：两份快照逐值裁决 → 退出码 + diff 报告。
//!
//! 裁决全在 `crate::snapshot::compare`（判据、定位、差异类型都在 core），本文件只做壳：
//! 读两份快照、把结构化差异渲染成 `DIFF` 行、把"有无差异"翻成退出码（§1 / §5.2 D1）。
//! 列名不参与判定，所以报告里给的是**左快照**的列名——基线常用 `cnt`、本地给回 `n`
//! 是常态，指认到列靠的是位置。

use crate::cli::run::{EXIT_FAIL, EXIT_OK};
use crate::error::EngineError;
use crate::snapshot::compare::{Diff, DiffKind};
use crate::snapshot::Snapshot;
use std::io::Write;
use std::path::Path;

/// `spinduck compare <a.json> <b.json> [--key k1,k2]`。
pub fn compare_cmd(
    a: &Path,
    b: &Path,
    key: &[String],
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let left = load(a)?;
    let right = load(b)?;
    let diffs = if key.is_empty() {
        crate::snapshot::compare::compare(&left, &right)
    } else {
        crate::snapshot::compare::compare_by_key(&left, &right, key)?
    };
    for d in &diffs {
        writeln!(out, "{}", diff_line(d)).map_err(io_err)?;
    }
    let scope = if key.is_empty() {
        String::new()
    } else {
        format!(" key={}", key.join(","))
    };
    let verdict = if diffs.is_empty() {
        ""
    } else {
        " 逐值不一致"
    };
    writeln!(
        out,
        "compare:{scope} rows={}/{} diffs={}{verdict}",
        left.rows.len(),
        right.rows.len(),
        diffs.len()
    )
    .map_err(io_err)?;
    Ok(if diffs.is_empty() { EXIT_OK } else { EXIT_FAIL })
}

/// 读一份快照：JSON 约定不合时点名叫它（`ENV-0031`），不说"没有差异"。
fn load(path: &Path) -> Result<Snapshot, EngineError> {
    let text = crate::cli::read_source_strict(path)?;
    Snapshot::from_json_str(&text)
        .map_err(|e| EngineError::env(31, format!("{}: {}", path.display(), e.message)))
}

/// 差异行：定位用 1 起行号 + 左快照列名；`arity`/整行未匹配没有可指认的列。
fn diff_line(d: &Diff) -> String {
    match d.kind {
        DiffKind::Value => format!(
            "DIFF value row={} col={} a={} b={}",
            d.row, d.column, d.a, d.b
        ),
        DiffKind::Arity => format!("DIFF arity row={} a={} b={}", d.row, d.a, d.b),
        DiffKind::OnlyInA => format!("DIFF only-in-a row={} a={}", d.row, d.a),
        DiffKind::OnlyInB => format!("DIFF only-in-b row={} b={}", d.row, d.b),
    }
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}
