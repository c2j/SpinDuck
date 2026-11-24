//! `compile` 的两种 argv：无旗标的路径体检，以及项目态旗标。
//!
//! 未知 `--` 仍是「不接受旗标」。零参数仍是 `compile <path>...`。
//! 裸路径和任一项目旗标同时出现则用法错，不混着做两件事。

use super::{dup_flag_err, next_arg, usage_err, Command};
use crate::error::EngineError;
use crate::ir::set_oriented::SetOrientedMode;
use std::path::PathBuf;

/// `compile --admit` 判定档位（设计 v3.1 §3.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitMode {
    /// 只报告，不改退出码。
    Report,
    /// firm `skipped` 计入退出码（conservative/conditional 不判红，F2）。
    Error,
}

struct CompileScan {
    dir: Option<PathBuf>,
    schema: Option<String>,
    entries: Vec<String>,
    analyze: bool,
    codeweb_project: Option<PathBuf>,
    sync_manifest: bool,
    admit: Option<AdmitMode>,
    db: Option<PathBuf>,
    set_oriented: Option<SetOrientedMode>,
    admit_record: Option<PathBuf>,
    paths: Vec<PathBuf>,
    project_mode: bool,
}

pub(super) fn parse_compile(rest: &[String]) -> Result<Command, EngineError> {
    let scan = scan_compile(rest)?;
    if scan.project_mode && !scan.paths.is_empty() {
        return Err(usage_err("compile 不能同时给出路径和项目旗标"));
    }
    if scan.project_mode {
        // §3.1 约束 1：admit 从属旗标不带 --admit 出现 = 用法错（不静默忽略）。
        if scan.admit.is_none()
            && (scan.db.is_some() || scan.set_oriented.is_some() || scan.admit_record.is_some())
        {
            return Err(usage_err(
                "compile 的 --db/--set-oriented/--admit-record 需要 --admit",
            ));
        }
        // D9 定稿：--admit 强制 --db；无数据面只剩静态体检，CLI 层堵死误用面。
        if scan.admit.is_some() && scan.db.is_none() {
            return Err(usage_err("compile --admit 需要 --db <文件>"));
        }
        return Ok(Command::CompileProject {
            dir: scan.dir,
            schema: scan.schema,
            entries: scan.entries,
            analyze: scan.analyze,
            codeweb_project: scan.codeweb_project,
            sync_manifest: scan.sync_manifest,
            admit: scan.admit,
            db: scan.db,
            set_oriented: scan.set_oriented,
            admit_record: scan.admit_record,
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
        analyze: false,
        codeweb_project: None,
        sync_manifest: false,
        admit: None,
        db: None,
        set_oriented: None,
        admit_record: None,
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
            "--analyze" => {
                scan.project_mode = true;
                scan.analyze = true;
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
            "--admit" => {
                scan.project_mode = true;
                if scan.admit.is_some() {
                    return Err(dup_flag_err("--admit"));
                }
                scan.admit = Some(AdmitMode::Report);
            }
            t if t.starts_with("--admit=") => {
                scan.project_mode = true;
                if scan.admit.is_some() {
                    return Err(dup_flag_err("--admit"));
                }
                scan.admit = Some(match &t[8..] {
                    "report" => AdmitMode::Report,
                    "error" => AdmitMode::Error,
                    _ => return Err(usage_err("--admit[=report|error]")),
                });
            }
            "--db" => {
                scan.project_mode = true;
                set_path(&mut scan.db, "--db", next_arg(&mut it, "--db <文件>")?)?;
            }
            "--admit-record" => {
                scan.project_mode = true;
                set_path(
                    &mut scan.admit_record,
                    "--admit-record",
                    next_arg(&mut it, "--admit-record <文件>")?,
                )?;
            }
            flag if flag == "--set-oriented" || flag.starts_with("--set-oriented=") => {
                scan.project_mode = true;
                if scan.set_oriented.is_some() {
                    return Err(dup_flag_err("--set-oriented"));
                }
                let name = flag.strip_prefix("--set-oriented=").unwrap_or("strict");
                scan.set_oriented = Some(
                    SetOrientedMode::from_name(name)
                        .ok_or_else(|| usage_err("--set-oriented[=off|strict|batch-atomic]"))?,
                );
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
            "--analyze",
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
                analyze: true,
                codeweb_project: Some(PathBuf::from("root")),
                sync_manifest: false,
                admit: None,
                db: None,
                set_oriented: None,
                admit_record: None,
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
                analyze: false,
                codeweb_project: None,
                sync_manifest: true,
                admit: None,
                db: None,
                set_oriented: None,
                admit_record: None,
            }
        );
    }

    #[test]
    fn should_parse_admit_flags_with_db() {
        let got = parse(&v(&[
            "compile",
            "--admit=error",
            "--db",
            "a.duckdb",
            "--set-oriented=batch-atomic",
            "--admit-record",
            "rep.toml",
        ]))
        .unwrap();
        assert_eq!(
            got,
            Command::CompileProject {
                dir: None,
                schema: None,
                entries: vec![],
                analyze: false,
                codeweb_project: None,
                sync_manifest: false,
                admit: Some(AdmitMode::Error),
                db: Some(PathBuf::from("a.duckdb")),
                set_oriented: Some(SetOrientedMode::BatchAtomic),
                admit_record: Some(PathBuf::from("rep.toml")),
            }
        );
    }

    #[test]
    fn should_default_bare_admit_to_report_and_set_oriented_to_strict() {
        let got = parse(&v(&["compile", "--admit", "--db", "a", "--set-oriented"])).unwrap();
        match got {
            Command::CompileProject {
                admit,
                set_oriented,
                ..
            } => {
                assert_eq!(admit, Some(AdmitMode::Report));
                assert_eq!(set_oriented, Some(SetOrientedMode::Strict));
            }
            other => panic!("expected CompileProject, got {other:?}"),
        }
    }

    #[test]
    fn should_reject_admit_flags_without_admit() {
        // §3.1 约束 1：admit 从属旗标不带 --admit = 用法错（不静默忽略）
        for argv in [
            v(&["compile", "--db", "a"]),
            v(&["compile", "--admit-record", "r.toml"]),
            v(&["compile", "--set-oriented=strict"]),
            v(&["compile", "--dir", "p", "--db", "a"]),
        ] {
            let err = parse(&argv).unwrap_err();
            assert_eq!(err.code.to_string(), "ENG-0010", "{argv:?}: {err}");
            assert!(err.message.contains("--admit"), "{argv:?}: {err}");
        }
    }

    #[test]
    fn should_reject_admit_without_db_per_d9() {
        for argv in [
            v(&["compile", "--admit"]),
            v(&["compile", "--admit=error", "--dir", "p"]),
        ] {
            let err = parse(&argv).unwrap_err();
            assert_eq!(err.code.to_string(), "ENG-0010", "{argv:?}: {err}");
            assert!(err.message.contains("--db"), "{argv:?}: {err}");
        }
    }

    #[test]
    fn should_reject_bad_admit_value_and_duplicate_flags() {
        let err = parse(&v(&["compile", "--admit=bogus", "--db", "a"])).unwrap_err();
        assert!(err.message.contains("--admit"), "{err}");
        let err = parse(&v(&["compile", "--admit", "--admit=error", "--db", "a"])).unwrap_err();
        assert!(err.message.contains("重复旗标"), "{err}");
        let err = parse(&v(&["compile", "--admit", "--db", "a", "--db", "b"])).unwrap_err();
        assert!(err.message.contains("重复旗标"), "{err}");
    }

    #[test]
    fn should_reject_paths_mixed_with_any_project_flag() {
        for argv in [
            v(&["compile", "a.sql", "--dir", "p"]),
            v(&["compile", "--entry", "pkg.a", "a.sql"]),
            v(&["compile", "--analyze", "a.sql"]),
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
        assert!(text.contains("--analyze"), "{text}");
        assert!(!text.contains("--codeweb-bin"), "{text}");
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
