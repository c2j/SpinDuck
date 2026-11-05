//! `verify` 子命令壳：定位项目 → 链路核验 → 渲染与退出码（§2.1：CLI 只做壳）。
//!
//! 核验逻辑全在 `crate::verify`；本文件只负责项目定位、逐行渲染与退出码。

use crate::cli::run::{EXIT_FAIL, EXIT_OK};
use crate::error::EngineError;
use crate::project::ProjectConfig;
use crate::verify::{EntryReport, Status};
use std::io::Write;
use std::path::Path;

/// `spinduck verify [项目目录]`：目录缺省为当前工作目录。
pub fn verify_cmd(path: Option<&Path>, out: &mut dyn Write) -> Result<i32, EngineError> {
    let start = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| EngineError::env(12, format!("cwd: {e}")))?,
    };
    let project = locate(&start)?;
    // 先核验后落快照：核验没跑通就写运行态，等于声称了一个未被证明的可调用面
    let reports = verify_project(&project)?;
    sync_manifest(&project, out)?;
    render(&reports, out)
}

/// 运行态快照同步（§11.1 `conf/`）。
///
/// 只在研发态目录（含手写 `spinduck.toml`）写入：从运行态目录执行时不回写，
/// 快照的来源标注因此始终指向真源，"漂移由下一次 verify 判读"的口径才不会断。
fn sync_manifest(project: &ProjectConfig, out: &mut dyn Write) -> Result<(), EngineError> {
    let line = if project.root.join("spinduck.toml").is_file() {
        let s = crate::verify::sync_runtime_manifest(project)?;
        format!(
            "manifest: {} ({})",
            s.path.display(),
            if s.changed { "updated" } else { "unchanged" }
        )
    } else {
        format!(
            "manifest: {} (未改写：本目录即运行态快照)",
            project.root.join("conf/spinduck.toml").display()
        )
    };
    writeln!(out, "{line}").map_err(io_err)
}

/// 项目定位：目录本身有声明文件（研发态或运行态快照）直接用，否则向上查找。
fn locate(start: &Path) -> Result<ProjectConfig, EngineError> {
    if start.join("spinduck.toml").is_file() || start.join("conf/spinduck.toml").is_file() {
        return ProjectConfig::load(start);
    }
    ProjectConfig::discover(start).ok_or_else(|| {
        EngineError::env(
            30,
            format!(
                "未在 {} 及其上级目录找到项目声明（spinduck.toml 或 conf/spinduck.toml；verify 以项目声明为起点）",
                start.display()
            ),
        )
    })
}

#[cfg(feature = "native")]
fn verify_project(project: &ProjectConfig) -> Result<Vec<EntryReport>, EngineError> {
    crate::verify::verify_project(project)
}

#[cfg(not(feature = "native"))]
fn verify_project(_project: &ProjectConfig) -> Result<Vec<EntryReport>, EngineError> {
    Err(EngineError::env(
        12,
        "verify 需要 feature `native`（链路核验要把语料部署进内核）".to_string(),
    ))
}

fn render(reports: &[EntryReport], out: &mut dyn Write) -> Result<i32, EngineError> {
    let mut checks = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for r in reports {
        writeln!(out, "== entry {}", r.entry).map_err(io_err)?;
        for c in &r.checks {
            checks += 1;
            let mark = match c.status {
                Status::Pass => "PASS",
                Status::Fail => {
                    failed += 1;
                    "FAIL"
                }
                Status::Skip => {
                    skipped += 1;
                    "SKIP"
                }
            };
            writeln!(out, "  {mark} {:?} {} — {}", c.kind, c.object, c.detail).map_err(io_err)?;
        }
    }
    writeln!(
        out,
        "verify: entries={} checks={} failed={} skipped={}",
        reports.len(),
        checks,
        failed,
        skipped
    )
    .map_err(io_err)?;
    Ok(if failed == 0 { EXIT_OK } else { EXIT_FAIL })
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}
