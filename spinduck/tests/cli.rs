//! CLI 契约层（AGENTS §4.1）：子命令退出码 + stdout 契约。
//!
//! 第四轮复审存档缺口的落地：`call --batch-commit` / `--step-budget` 的实际
//! 接线（`cli/run.rs` 对 `eng.batch_commit` / `eng.step_budget` 的赋值）此前
//! 只有解析层测试面。本文件驱动**打包后的二进制**走完整 run 路径。
//! `cli` feature 蕴含 `native`；CI Test job 以 `--features cli` 运行（口径见
//! workflow 注释与 AGENTS §9）。

#![cfg(feature = "cli")]

use std::path::PathBuf;
use std::process::Command;

/// 驱动打包后的 spinduck 二进制，返回 (退出码, 合并的 stdout/stderr 文本)。
fn run_cli(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_spinduck"))
        .args(args)
        .output()
        .expect("spawn spinduck");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// 语料同款 LOOP/COMMIT/EXIT 例程：COMMIT 走 ON 开关的吞并路径。
const ROUTINE_SQL: &str = "CREATE OR REPLACE PROCEDURE prc_fill(p_n IN INT) IS\n  v_i INT := 0;\nBEGIN\n  LOOP\n    v_i := v_i + 1;\n    COMMIT;\n    EXIT WHEN v_i >= p_n;\n  END LOOP;\nEND;\n/\n";

fn write_routine_file(dir: &tempfile::TempDir) -> PathBuf {
    let p = dir.path().join("routines.sql");
    std::fs::write(&p, ROUTINE_SQL).expect("write routines.sql");
    p
}

#[test]
fn should_call_routine_with_batch_commit_and_step_budget_flags() {
    // 开关接线证据：--batch-commit / --step-budget 经 cli::run 真实生效
    // （parse → run → Engine::call 全链路，注册与调用均成功）
    let dir = tempfile::tempdir().expect("tempdir");
    let f = write_routine_file(&dir);
    let file = f.to_string_lossy().to_string();
    let (code, out) = run_cli(&[
        "call",
        "--file",
        &file,
        "--batch-commit",
        "--step-budget",
        "1000000",
        "prc_fill",
        "5",
    ]);
    assert_eq!(code, 0, "call 应成功，实际输出: {out}");
    assert!(
        out.contains("CALL prc_fill OK"),
        "stdout 契约：CALL <routine> OK，实际: {out}"
    );
}

#[test]
fn should_reject_step_budget_zero_with_usage_exit() {
    // 0 拒绝在解析层：usage 退出码 2 + 消息含 flag 名
    let (code, out) = run_cli(&["call", "--step-budget", "0", "prc_x"]);
    assert_eq!(code, 2, "usage 错误退出码应为 2，实际输出: {out}");
    assert!(
        out.contains("--step-budget"),
        "消息应含 flag 名，实际: {out}"
    );
}

#[test]
fn should_call_routine_on_default_off_path() {
    // 不带开关的默认路径：与 ON 路径同例程，锁「开关缺省不改变契约」
    let dir = tempfile::tempdir().expect("tempdir");
    let f = write_routine_file(&dir);
    let file = f.to_string_lossy().to_string();
    let (code, out) = run_cli(&["call", "--file", &file, "prc_fill", "5"]);
    assert_eq!(code, 0, "call 应成功，实际输出: {out}");
    assert!(out.contains("CALL prc_fill OK"), "实际: {out}");
}
