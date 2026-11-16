//! `batch` 子命令解析：`batch <script.sdsh> [--continue-on-error] [--dry-run]`。
//!
//! 脚本内指令的解析在 `crate::batch`（词法 + 逐行 `cli::args::parse`）；
//! 本文件只管 batch 自身的三个参数面。

use super::{dup_flag_err, usage_err, Command};
use crate::error::EngineError;
use std::path::PathBuf;

pub(super) fn parse_batch(rest: &[String]) -> Result<Command, EngineError> {
    let usage = "batch <script.sdsh> [--continue-on-error] [--dry-run]";
    let mut script = None;
    let mut continue_on_error = false;
    let mut coe_seen = false;
    let mut dry_run = false;
    let mut dry_seen = false;
    for tok in rest {
        match tok.as_str() {
            "--continue-on-error" => {
                if coe_seen {
                    return Err(dup_flag_err("--continue-on-error"));
                }
                coe_seen = true;
                continue_on_error = true;
            }
            "--dry-run" => {
                if dry_seen {
                    return Err(dup_flag_err("--dry-run"));
                }
                dry_seen = true;
                dry_run = true;
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => {
                if script.is_some() {
                    return Err(usage_err(format!("{usage}（脚本路径重复）")));
                }
                script = Some(PathBuf::from(t));
            }
        }
    }
    Ok(Command::Batch {
        script: script.ok_or_else(|| usage_err(format!("{usage}（缺脚本路径）")))?,
        continue_on_error,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{parse, v};

    #[test]
    fn should_parse_batch_with_flags() {
        assert_eq!(
            parse(&v(&["batch", "run.sdsh"])).unwrap(),
            Command::Batch {
                script: PathBuf::from("run.sdsh"),
                continue_on_error: false,
                dry_run: false,
            }
        );
        assert_eq!(
            parse(&v(&[
                "batch",
                "run.sdsh",
                "--continue-on-error",
                "--dry-run"
            ]))
            .unwrap(),
            Command::Batch {
                script: PathBuf::from("run.sdsh"),
                continue_on_error: true,
                dry_run: true,
            }
        );
    }

    #[test]
    fn should_reject_batch_without_script_or_with_dup_flags() {
        assert!(parse(&v(&["batch"])).is_err());
        assert!(parse(&v(&["batch", "a.sdsh", "b.sdsh"])).is_err());
        assert!(parse(&v(&["batch", "a.sdsh", "--dry-run", "--dry-run"])).is_err());
        assert!(parse(&v(&["batch", "a.sdsh", "--bogus"])).is_err());
    }
}
