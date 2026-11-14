//! 项目链路面的解析：`deploy` 按计划装库。
//!
//! 位置参数只有一处语义（计划文件），多余位置一律用法错，
//! 不做"猜用户想给哪个"的容错。项目目录/入口的解析在项目态 `compile`（`compile.rs`）。

use super::{dup_flag_err, next_arg, usage_err, Command};
use crate::error::EngineError;
use std::path::PathBuf;

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
