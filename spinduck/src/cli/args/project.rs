//! 项目链路面的解析：`verify` 取项目目录、`plan` 求闭包落计划、`deploy` 按计划装库。
//!
//! 这三条共用同一条纪律：位置参数只有一处语义（项目目录 / 目标例程 / 计划文件），
//! 多余位置一律用法错，不做"猜用户想给哪个"的容错。

use super::{dup_flag_err, next_arg, usage_err, Command};
use crate::error::EngineError;
use std::path::PathBuf;

pub(super) fn parse_verify(rest: &[String]) -> Result<Command, EngineError> {
    match rest {
        [] => Ok(Command::Verify { path: None }),
        [p] => Ok(Command::Verify {
            path: Some(PathBuf::from(p)),
        }),
        _ => Err(usage_err("verify [项目目录]")),
    }
}

pub(super) fn parse_plan(rest: &[String]) -> Result<Command, EngineError> {
    let mut dir = None;
    let mut schema = None;
    let mut routine = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--dir" => {
                // F3 取舍：plan 的位置参数已被目标例程占用，项目目录只能走旗标；
                // verify/compile 的目录是主操作数才用位置参数——同概念不同形态是刻意的
                if dir.is_some() {
                    return Err(dup_flag_err("--dir"));
                }
                dir = Some(PathBuf::from(next_arg(&mut it, "--dir <项目目录>")?));
            }
            "--schema" => {
                if schema.is_some() {
                    return Err(dup_flag_err("--schema"));
                }
                schema = Some(next_arg(&mut it, "--schema <名>")?);
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            _ if routine.is_none() => routine = Some(tok.clone()),
            t => return Err(usage_err(format!("plan 只接受一个目标例程，多余参数: {t}"))),
        }
    }
    let routine =
        routine.ok_or_else(|| usage_err("plan [--dir <项目目录>] [--schema <名>] <routine>"))?;
    Ok(Command::Plan {
        dir,
        routine,
        schema,
    })
}

/// `deploy <计划文件> --db <f>`：两个参数都必填（缺库文件即无落盘目标，等于没部署）。
pub(super) fn parse_deploy(rest: &[String]) -> Result<Command, EngineError> {
    let usage = "deploy <计划文件> --db <本地 DuckDB 文件>";
    let mut db = None;
    let mut plan = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => {
                if db.is_some() {
                    return Err(dup_flag_err("--db"));
                }
                db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?));
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            _ if plan.is_none() => plan = Some(PathBuf::from(tok)),
            t => {
                return Err(usage_err(format!(
                    "deploy 只接受一个计划文件，多余参数: {t}"
                )))
            }
        }
    }
    Ok(Command::Deploy {
        plan: plan.ok_or_else(|| usage_err(usage))?,
        db: db.ok_or_else(|| usage_err(format!("{usage}（缺 --db）")))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{parse, v};

    #[test]
    fn should_parse_verify_with_optional_path_and_reject_extra_args() {
        assert_eq!(
            parse(&v(&["verify"])).unwrap(),
            Command::Verify { path: None }
        );
        assert_eq!(
            parse(&v(&["verify", "golden/case_split_agg"])).unwrap(),
            Command::Verify {
                path: Some(PathBuf::from("golden/case_split_agg"))
            }
        );
        let e = parse(&v(&["verify", "a", "b"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("verify"), "{}", e.message);
    }

    #[test]
    fn should_parse_plan_with_dir_and_schema() {
        let c = parse(&v(&[
            "plan",
            "--dir",
            "golden/case_split_agg",
            "--schema",
            "BIGFUND",
            "pkg_a.prc_x",
        ]))
        .unwrap();
        assert_eq!(
            c,
            Command::Plan {
                dir: Some(PathBuf::from("golden/case_split_agg")),
                routine: "pkg_a.prc_x".to_string(),
                schema: Some("BIGFUND".to_string()),
            }
        );
        assert_eq!(
            parse(&v(&["plan", "prc_x"])).unwrap(),
            Command::Plan {
                dir: None,
                routine: "prc_x".to_string(),
                schema: None,
            }
        );
    }

    #[test]
    fn should_reject_plan_without_routine_or_with_extra_positionals() {
        let e = parse(&v(&["plan", "--dir", "x"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("plan [--dir"), "{}", e.message);
        let e = parse(&v(&["plan", "--schema"])).unwrap_err();
        assert!(e.message.contains("--schema <名>"), "{}", e.message);
        let e = parse(&v(&["plan", "a", "b"])).unwrap_err();
        assert!(e.message.contains("只接受一个目标例程"), "{}", e.message);
    }

    #[test]
    fn should_parse_deploy_with_plan_and_db_in_either_order() {
        let want = Command::Deploy {
            plan: PathBuf::from(".spinduck/plan/prc_main.toml"),
            db: PathBuf::from("w.duckdb"),
        };
        assert_eq!(
            parse(&v(&[
                "deploy",
                ".spinduck/plan/prc_main.toml",
                "--db",
                "w.duckdb"
            ]))
            .unwrap(),
            want
        );
        assert_eq!(
            parse(&v(&[
                "deploy",
                "--db",
                "w.duckdb",
                ".spinduck/plan/prc_main.toml"
            ]))
            .unwrap(),
            want
        );
    }

    #[test]
    fn should_reject_deploy_without_db_or_without_plan() {
        let e = parse(&v(&["deploy", "p.toml"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(
            e.message.contains("--db"),
            "缺件面要指认 --db：{}",
            e.message
        );
        let e = parse(&v(&["deploy", "--db", "w"])).unwrap_err();
        assert!(
            e.message.contains("deploy <计划文件>"),
            "缺计划文件要指认位置参数：{}",
            e.message
        );
        let e = parse(&v(&["deploy", "a.toml", "b.toml", "--db", "w"])).unwrap_err();
        assert!(e.message.contains("只接受一个计划文件"), "{}", e.message);
    }
}
