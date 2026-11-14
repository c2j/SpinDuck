//! `version` / 无参用法 / 用法错误面 / 输出通道。

use super::{cli, EXIT_OK, EXIT_USAGE};

#[test]
fn should_print_package_version_on_version() {
    let (code, out, _) = cli(&["version"]);
    assert_eq!(code, EXIT_OK);
    // 发版守卫的 CLI 侧同一条：二进制自报版本 == crate 版本
    assert_eq!(out, format!("spinduck {}\n", spinduck_core::version()));
}

#[test]
fn should_print_usage_and_exit_0_without_arguments() {
    let (code, out, _) = cli(&[]);
    assert_eq!(code, EXIT_OK);
    assert!(out.starts_with("spinduck <command>\n"), "{out}");
    assert!(out.contains("call [--db <f>]"), "{out}");
}

#[test]
fn should_exit_2_with_eng_0010_and_usage_on_unknown_subcommand() {
    let (code, out, _) = cli(&["nosuchcommand"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(
        out.starts_with("[ENG-0010] usage: unknown subcommand nosuchcommand\n"),
        "{out}"
    );
    assert!(out.contains("spinduck <command>\n"), "{out}");
}

#[test]
fn should_exit_2_with_eng_0010_on_missing_compile_path() {
    let (code, out, _) = cli(&["compile"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(
        out.starts_with("[ENG-0010] usage: compile <path>...\n"),
        "{out}"
    );
}

#[test]
fn should_exit_2_with_eng_0010_on_unknown_call_flag() {
    let (code, out, _) = cli(&["call", "--help", "p_x"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(
        out.starts_with("[ENG-0010] usage: unknown flag --help\n"),
        "{out}"
    );
}

#[test]
fn should_exit_2_when_call_gets_neither_a_routine_nor_a_plan() {
    // `--plan` 与位置 `<routine>` 二选一：都不给就没有目标。诊断必须同时点名两条路，
    // 否则只用过 `--file` 的人看不出还有计划面可走。
    let (code, out, err) = cli(&["call"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(
        out.starts_with("[ENG-0010] usage: call [--plan <plan.toml> | <routine>] [args...]"),
        "{out}"
    );
    assert!(out.contains("call --plan <计划.toml>"), "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_mention_project_compile_without_plan_verify_lines() {
    // issue #38：plan/verify 子命令已删除，usage 不得再列；`call --plan` 是计划文件
    // 旗标而非子命令，必须保留（按行首断言，避免子串误伤）。
    let (code, out, err) = cli(&["help"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(err, "");
    assert!(out.contains("compile [--dir <项目目录>]"), "{out}");
    assert!(out.contains("--codeweb-bin"), "{out}");
    assert!(out.contains("--sync-manifest"), "{out}");
    for line in out.lines() {
        let head = line.trim_start();
        assert!(
            !head.starts_with("plan ") && !head.starts_with("verify "),
            "usage 不得再列已删除的子命令: {line}"
        );
    }
    assert!(
        out.lines()
            .any(|l| l.trim_start().starts_with("call --plan")),
        "call --plan 行必须保留: {out}"
    );
    assert!(out.contains("内置种子 < 项目发现 < --rules"), "{out}");
}

#[test]
fn should_reject_plan_and_verify_as_unknown_subcommands() {
    // issue #38：旧命令名不留墓碑提示，直接落未知子命令路径（退出码 2 + ENG-0010）。
    for name in ["plan", "verify"] {
        let (code, out, err) = cli(&[name]);
        assert_eq!(code, EXIT_USAGE, "{name}: {out}");
        assert!(
            out.starts_with(&format!("[ENG-0010] usage: unknown subcommand {name}\n")),
            "{name}: {out}"
        );
        assert_eq!(err, "", "{name}: 全部输出走 stdout");
    }
}

#[test]
fn should_write_contracts_to_stdout_and_leave_stderr_empty() {
    let (code, out, err) = cli(&["nosuchcommand"]);
    assert_eq!(code, EXIT_USAGE);
    assert!(!out.is_empty(), "用法错误必须给出诊断");
    assert_eq!(err, "", "CLI 约定全部输出走 stdout，stderr 恒空");
}
