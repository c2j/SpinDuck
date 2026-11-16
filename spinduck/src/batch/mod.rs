//! `spinduck batch <script.sdsh>`：同进程按序编排多个 SpinDuck 指令
//! （设计 docs/spinduck-batch-sdsh-design.md）。
//!
//! 两阶段：Phase 1 词法/预检（不碰数据库、不建日志），Phase 2 顺序执行
//! （`DbPool`/`EngineCache`/`CompiledRoutineCache` 共享）。本文件只做出口；
//! 词法在 `parser.rs`，预检在 `preflight.rs`，会话在 `session.rs`，
//! 执行分发在 `runner.rs`。

pub mod parser;
mod preflight;
#[cfg(feature = "native")]
mod runner;
#[cfg(feature = "native")]
mod session;

use std::io::Write;
use std::path::Path;

/// `spinduck batch <script.sdsh> [--continue-on-error] [--dry-run]`。
/// 返回进程退出码（0/1；用法错误也归 1——脚本内逐行契约，不走全局 usage dump）。
pub fn run_batch(
    script: &Path,
    continue_on_error: bool,
    dry_run: bool,
    out: &mut dyn Write,
) -> i32 {
    let text = match crate::cli::read_source_strict(script) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            return 1;
        }
    };
    let parsed = match parser::parse_script(&text) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            return 1;
        }
    };
    let steps = match preflight::preflight(&parsed) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            return 1;
        }
    };
    if dry_run {
        let _ = writeln!(
            out,
            "batch: dry-run steps={} default_db={}",
            steps.len(),
            parsed.default_db.as_deref().unwrap_or("<none>")
        );
        return 0;
    }
    #[cfg(feature = "native")]
    {
        let mut session = session::BatchSession::new();
        runner::run_steps(&steps, &mut session, continue_on_error, out)
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (steps, continue_on_error);
        let _ = writeln!(out, "[ENV-0012] batch 执行需要 feature `native`");
        1
    }
}
