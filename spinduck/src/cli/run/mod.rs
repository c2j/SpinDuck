//! 子命令执行与渲染（纯函数式：写 `out` 并返回退出码，便于契约测试）。
//!
//! 按职责分文件（§3 单文件 ≤500 行，先例见 `tests/cli.rs`）：编译体检与 explain 在
//! `compile.rs`，`call` 执行面在 `call.rs`，GaussDB 物化/回写在 `io.rs`。本文件只留
//! `run`/`exec` 派发、退出码与 `io_err`。

mod call;
mod compile;
mod io;

use call::{call_cmd, cmd_call};
use compile::{compile_cmd, explain_cmd};
use io::{cmd_materialize, cmd_sync, materialize_cmd, sync_cmd};

use crate::cli::args::{parse, usage, Command};
use crate::error::EngineError;
use crate::rules::RuleRegistry;
use std::io::Write;

/// 退出码：0 成功 / 1 业务或编译失败 / 2 用法错误。
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

/// 执行一次 CLI 调用。
pub fn run(argv: &[String], out: &mut dyn Write) -> i32 {
    let cmd = match parse(argv) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            let _ = write!(out, "{}", usage());
            return EXIT_USAGE;
        }
    };
    match exec(cmd, out) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            EXIT_FAIL
        }
    }
}

fn exec(cmd: Command, out: &mut dyn Write) -> Result<i32, EngineError> {
    match cmd {
        Command::Help => {
            write!(out, "{}", usage()).map_err(io_err)?;
            Ok(EXIT_OK)
        }
        Command::Version => {
            writeln!(out, "spinduck {}", crate::version()).map_err(io_err)?;
            Ok(EXIT_OK)
        }
        Command::RulesProbe { path } => {
            let mut rules = RuleRegistry::default();
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| EngineError::env(12, format!("read rules: {e}")))?;
            rules.overlay(&raw)?;
            let rules = rules;
            writeln!(
                out,
                "rules: {} target_duckdb={} probe_duckdb={} match={}",
                path.display(),
                rules.target_duckdb,
                rules.probe_duckdb,
                rules.probe_versions_match()
            )
            .map_err(io_err)?;
            Ok(if rules.probe_versions_match() {
                EXIT_OK
            } else {
                EXIT_FAIL
            })
        }
        Command::Compile { paths } => compile_cmd(&paths, out),
        Command::Explain {
            file,
            routine,
            trace,
        } => explain_cmd(&file, &routine, trace.as_deref(), out),
        Command::Materialize { .. } => materialize_cmd(cmd_materialize(cmd), out),
        Command::Sync { .. } => sync_cmd(cmd_sync(cmd), out),
        Command::Call { .. } => call_cmd(cmd_call(cmd), out),
        Command::Verify { path } => crate::cli::verify::verify_cmd(path.as_deref(), out),
        Command::Plan {
            dir,
            routine,
            schema,
        } => crate::cli::plan::plan_cmd(dir.as_deref(), &routine, schema, out),
        Command::Deploy { plan, db } => crate::cli::deploy::deploy_cmd(&plan, &db, out),
        Command::Query {
            db,
            sql_file,
            out: out_path,
        } => crate::cli::query::query_cmd(&db, &sql_file, &out_path, out),
        Command::Compare { a, b, key } => crate::cli::compare::compare_cmd(&a, &b, &key, out),
    }
}

pub(super) fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}

#[cfg(test)]
pub(crate) fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
pub(crate) fn run_str(items: &[&str]) -> (i32, String) {
    let mut buf: Vec<u8> = Vec::new();
    let code = run(&argv(items), &mut buf);
    (code, String::from_utf8(buf).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_print_usage_and_exit_2_on_usage_error() {
        let (code, out) = run_str(&["compile"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(out.contains("ENG-0010"));
        assert!(out.contains("compile <path>"));
    }

    #[test]
    fn should_pass_rules_probe_when_versions_match() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rules.toml");
        std::fs::write(&p, "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.5.5\"\n").unwrap();
        let (code, out) = run_str(&["rules", "probe", &p.to_string_lossy()]);
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("match=true"), "{out}");
    }

    #[test]
    fn should_fail_rules_probe_on_version_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rules.toml");
        std::fs::write(&p, "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.4.4\"\n").unwrap();
        let (code, out) = run_str(&["rules", "probe", &p.to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(out.contains("match=false"), "{out}");
    }

    #[test]
    fn should_fail_verify_when_project_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (code, out) = run_str(&["verify", &tmp.path().to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(
            out.contains("spinduck.toml"),
            "诊断需指向缺失的项目声明文件：{out}"
        );
    }
}
