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
fn should_mention_project_compile_without_dropping_plan_verify_wording() {
    let (code, out, err) = cli(&["help"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(err, "");
    assert!(out.contains("compile [--dir <项目目录>]"), "{out}");
    assert!(out.contains("--codeweb-bin"), "{out}");
    assert!(
        out.contains("本发布周期的兼容入口，由项目态 compile 替换"),
        "{out}"
    );
    assert!(out.contains("call --plan"), "{out}");
    assert!(out.contains("内置种子 < 项目发现 < --rules"), "{out}");
}

#[test]
fn should_write_contracts_to_stdout_and_leave_stderr_empty() {
    let (code, out, err) = cli(&["nosuchcommand"]);
    assert_eq!(code, EXIT_USAGE);
    assert!(!out.is_empty(), "用法错误必须给出诊断");
    assert_eq!(err, "", "CLI 约定全部输出走 stdout，stderr 恒空");
}
