//! `compile` 的两种 argv：无旗标的路径体检，以及项目态旗标。
//!
//! 未知 `--` 仍是「不接受旗标」。零参数仍是 `compile <path>...`。
//! 裸路径和任一项目旗标同时出现则用法错，不混着做两件事。

use super::{dup_flag_err, next_arg, usage_err, Command};
use crate::error::EngineError;
use std::path::PathBuf;

struct CompileScan {
    dir: Option<PathBuf>,
    schema: Option<String>,
    entries: Vec<String>,
    codeweb_bin: Option<PathBuf>,
    codeweb_project: Option<PathBuf>,
    sync_manifest: bool,
    paths: Vec<PathBuf>,
    project_mode: bool,
}

pub(super) fn parse_compile(rest: &[String]) -> Result<Command, EngineError> {
    let scan = scan_compile(rest)?;
    if scan.project_mode && !scan.paths.is_empty() {
        return Err(usage_err("compile 不能同时给出路径和项目旗标"));
    }
    if scan.project_mode {
        return Ok(Command::CompileProject {
            dir: scan.dir,
            schema: scan.schema,
            entries: scan.entries,
            codeweb_bin: scan.codeweb_bin,
            codeweb_project: scan.codeweb_project,
            sync_manifest: scan.sync_manifest,
        });
    }
    if scan.paths.is_empty() {
        return Err(usage_err("compile <path>..."));
    }
    Ok(Command::Compile { paths: scan.paths })
}

fn scan_compile(rest: &[String]) -> Result<CompileScan, EngineError> {
    let mut scan = CompileScan {
        dir: None,
        schema: None,
        entries: Vec::new(),
        codeweb_bin: None,
        codeweb_project: None,
        sync_manifest: false,
        paths: Vec::new(),
        project_mode: false,
    };
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--dir" => {
                scan.project_mode = true;
                set_path(
                    &mut scan.dir,
                    "--dir",
                    next_arg(&mut it, "--dir <项目目录>")?,
                )?;
            }
            "--schema" => {
                scan.project_mode = true;
                set_string(
                    &mut scan.schema,
                    "--schema",
                    next_arg(&mut it, "--schema <名>")?,
                )?;
            }
            "--entry" => {
                scan.project_mode = true;
                scan.entries.push(next_arg(&mut it, "--entry <routine>")?);
            }
            "--codeweb-bin" => {
                scan.project_mode = true;
                set_path(
                    &mut scan.codeweb_bin,
                    "--codeweb-bin",
                    next_arg(&mut it, "--codeweb-bin <path>")?,
                )?;
            }
            "--codeweb-project" => {
                scan.project_mode = true;
                set_path(
                    &mut scan.codeweb_project,
                    "--codeweb-project",
                    next_arg(&mut it, "--codeweb-project <dir>")?,
                )?;
            }
            "--sync-manifest" => {
                scan.project_mode = true;
                scan.sync_manifest = true;
            }
            t if t.starts_with("--") => {
                return Err(usage_err(format!("compile 不接受旗标，未知参数: {t}")));
            }
            other => scan.paths.push(PathBuf::from(other)),
        }
    }
    Ok(scan)
}

fn set_path(slot: &mut Option<PathBuf>, flag: &str, value: String) -> Result<(), EngineError> {
    set_string(slot, flag, PathBuf::from(value))
}

fn set_string<T>(slot: &mut Option<T>, flag: &str, value: T) -> Result<(), EngineError> {
    if slot.is_some() {
        return Err(dup_flag_err(flag));
    }
    *slot = Some(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{parse, usage, v};

    #[test]
    fn should_parse_project_mode_compile_flags() {
        let got = parse(&v(&[
            "compile",
            "--schema",
            "S",
            "--entry",
            "pkg.a",
            "--entry",
            "pkg.b",
            "--codeweb-bin",
            "cw",
            "--codeweb-project",
            "root",
            "--dir",
            "proj",
        ]))
        .unwrap();
        assert_eq!(
            got,
            Command::CompileProject {
                dir: Some(PathBuf::from("proj")),
                schema: Some("S".into()),
                entries: vec!["pkg.a".into(), "pkg.b".into()],
                codeweb_bin: Some(PathBuf::from("cw")),
                codeweb_project: Some(PathBuf::from("root")),
                sync_manifest: false,
            }
        );
    }

    #[test]
    fn should_parse_sync_manifest_flag_as_project_mode() {
        let got = parse(&v(&["compile", "--dir", "proj", "--sync-manifest"])).unwrap();
        assert_eq!(
            got,
            Command::CompileProject {
                dir: Some(PathBuf::from("proj")),
                schema: None,
                entries: vec![],
                codeweb_bin: None,
                codeweb_project: None,
                sync_manifest: true,
            }
        );
    }

    #[test]
    fn should_reject_paths_mixed_with_any_project_flag() {
        for argv in [
            v(&["compile", "a.sql", "--dir", "p"]),
            v(&["compile", "--entry", "pkg.a", "a.sql"]),
            v(&["compile", "--codeweb-bin", "cw", "a.sql"]),
        ] {
            let err = parse(&argv).unwrap_err();
            assert_eq!(err.code.to_string(), "ENG-0010", "{argv:?}: {err}");
            assert!(err.message.contains("项目旗标"), "{argv:?}: {err}");
        }
    }

    #[test]
    fn should_reject_duplicate_single_value_compile_flags() {
        let err = parse(&v(&["compile", "--dir", "a", "--dir", "b"])).unwrap_err();
        assert!(err.message.contains("重复旗标"), "{err}");
        let err = parse(&v(&["compile", "--schema", "a", "--schema", "b"])).unwrap_err();
        assert!(err.message.contains("重复旗标"), "{err}");
    }

    #[test]
    fn usage_should_mention_project_compile_without_plan_verify_lines() {
        // issue #38：plan/verify 子命令已删除；`call --plan` 是旗标，按行首保留。
        let text = usage();
        assert!(text.contains("compile [--dir <项目目录>]"), "{text}");
        assert!(text.contains("--codeweb-bin"), "{text}");
        assert!(text.contains("--codeweb-project"), "{text}");
        assert!(text.contains("--sync-manifest"), "{text}");
        for line in text.lines() {
            let head = line.trim_start();
            assert!(
                !head.starts_with("plan ") && !head.starts_with("verify "),
                "usage 不得再列已删除的子命令: {line}"
            );
        }
        assert!(
            text.lines()
                .any(|l| l.trim_start().starts_with("call --plan")),
            "{text}"
        );
        assert!(text.contains("内置种子 < 项目发现 < --rules"), "{text}");
    }
}
