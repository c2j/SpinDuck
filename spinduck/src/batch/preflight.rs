//! Phase 1 预检：`.sdsh` 指令序列 → `cli::args::Command` 序列。
//!
//! 契约（设计 §3.1）：本阶段**不打开任何数据库、不建 trace 文件、不写产物**——
//! 只做词法结果的语义校验与 `--db` 继承注入。任何失败都 Fail-Fast（首个错即返）。
//!
//! `--db` 注入规则：维护 `current_db`（`use`/`--db` 指令顺序更新，行内显式
//! `--db` 优先）；缺 `--db` 且有默认库的 `deploy/materialize/sync/query/call`
//! 在 `cli::args::parse` **之前**注入 `--db <current>`——原解析器对
//! deploy/materialize/sync/query 的 `--db` 是必填旗标，裸 token 过 parse 必
//! `ENG-0010`；`call` 的 `db: Option` 注入后获得默认库，未设默认库时不注入
//! （匿名 `:memory:` 语义保持）。

use super::parser::{BatchLine, ParsedScript};
use crate::cli::args::{self, Command};
use crate::error::EngineError;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// 一个预检通过的执行步。
#[derive(Debug)]
pub struct Step {
    /// 脚本行号（1-based，诊断定位用）。
    pub line_no: usize,
    /// 解析后的子命令（`--db` 已按继承规则注入）。
    pub cmd: Command,
}

/// 需要 `--db` 继承注入的子命令（`call` 的 db 可选但同样吃默认库）。
fn needs_db(head: &str) -> bool {
    matches!(head, "deploy" | "materialize" | "sync" | "query" | "call")
}

/// 执行类/数据类子命令的 feature 门（设计 §3 约束）：
/// `gauss` 指令需 `gauss` feature；`deploy/query/call` 需 `native`。
fn feature_gate(cmd: &Command) -> Option<(&'static str, u32)> {
    match cmd {
        Command::Materialize { .. } | Command::Sync { .. } => {
            #[cfg(not(feature = "gauss"))]
            return Some(("gauss", 12));
            #[cfg(feature = "gauss")]
            return None;
        }
        Command::Deploy { .. } | Command::Query { .. } | Command::Call { .. } => {
            #[cfg(not(feature = "native"))]
            return Some(("native", 12));
            #[cfg(feature = "native")]
            return None;
        }
        _ => None,
    }
}

/// Phase 1 主入口：`ParsedScript` → `Vec<Step>`。
/// 失败 = 首个错误（错误码透传，消息补行号）。
pub fn preflight(parsed: &ParsedScript) -> Result<Vec<Step>, EngineError> {
    let mut steps = Vec::new();
    let mut current_db: Option<String> = None;
    let mut trace_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for (idx, line) in parsed.lines.iter().enumerate() {
        // 行号：parser 不保留原始行号，指令序号 +1 近似——为精确定位，
        // BatchLine::Command 的 tokens 不含行号；此处用指令序号作 step 序号，
        // 诊断消息里的 "step N" 与执行序一致（行号见 parser 错误）。
        let step_no = idx + 1;
        match line {
            BatchLine::Use(db) | BatchLine::GlobalDb(db) => {
                current_db = Some(db.clone());
            }
            BatchLine::Set { .. } => {}
            BatchLine::Command(tokens) => {
                let head = tokens[0].as_str();
                let mut argv: Vec<String> = tokens.clone();
                let has_db = argv.iter().any(|t| t == "--db");
                if needs_db(head) && !has_db {
                    if let Some(db) = &current_db {
                        argv.splice(1..1, ["--db".to_string(), db.clone()]);
                    }
                    // 无默认库：call 保持 db=None（匿名 :memory:）；
                    // 其余指令交给 parse 自然报 ENG-0010（缺 --db）。
                }
                let cmd = args::parse(&argv).map_err(|e| {
                    EngineError::new(
                        e.domain,
                        e.code,
                        format!("step {step_no} (`{head}`): {}", e.message),
                    )
                })?;
                if let Some((feat, _)) = feature_gate(&cmd) {
                    return Err(EngineError::env(
                        12,
                        format!("step {step_no} (`{head}`): 需要 feature `{feat}`"),
                    ));
                }
                // `--trace` 冲突只对 `call` 生效（`explain --trace` 是输入不是输出）。
                // Phase 1 同时拒绝"目标已存在"（PR#51 审核：单步契约是
                // TraceRun::open create_new → ENV-0016；若拖到 Phase 2 才发现，
                // 库文件已经落下，--dry-run 也会被误放行）。
                if let Command::Call { trace: Some(p), .. } = &cmd {
                    if !trace_paths.insert(p.clone()) {
                        return Err(EngineError::env(
                            16,
                            format!("step {step_no}: --trace 目标已存在/重复: {}", p.display()),
                        ));
                    }
                    if p.exists() {
                        return Err(EngineError::env(
                            16,
                            format!("step {step_no}: --trace 目标已存在: {}", p.display()),
                        ));
                    }
                }
                steps.push(Step {
                    line_no: step_no,
                    cmd,
                });
            }
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::super::parser::parse_script;
    use super::*;

    fn steps_of(script: &str) -> Result<Vec<Step>, EngineError> {
        preflight(&parse_script(script).unwrap())
    }

    #[test]
    fn should_inject_default_db_before_args_parse_and_honor_use() {
        // deploy/materialize/sync/query 的 --db 必填：注入后 parse 才能过；
        // use 切换后续步骤的默认库。
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("w.duckdb");
        let dbs = db.to_string_lossy().to_string();
        let script =
            format!("use {dbs}\ndeploy plan.toml\nuse :memory:\ncall --file f.sql prc 1\n");
        let steps = steps_of(&script).unwrap();
        match &steps[0].cmd {
            Command::Deploy { db: d, .. } => assert_eq!(d, &db),
            other => panic!("{other:?}"),
        }
        match &steps[1].cmd {
            Command::Call { db: d, .. } => {
                assert_eq!(d.as_deref(), Some(std::path::Path::new(":memory:")))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn should_fail_eng_0010_for_db_commands_without_default() {
        let e = steps_of("deploy plan.toml\n").unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010", "{e}");
        // call 无默认库不注入 → db=None（匿名 :memory:），预检通过
        assert!(steps_of("call --file f.sql prc 1\n").is_ok());
    }

    #[test]
    fn should_reject_duplicate_call_trace_paths_upfront() {
        let e = steps_of("call --file f prc --trace t.log 1\ncall --file f prc --trace t.log 2\n")
            .unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0016", "{e}");
        // explain --trace 是输入路径，不参与冲突检查
        assert!(steps_of(
            "explain --file f.sql --trace t.log prc\ncall --file f prc --trace t.log 1\n"
        )
        .is_ok());
    }

    #[test]
    fn should_reject_existing_trace_file_in_phase1() {
        // PR#51 审核：trace 目标已存在必须在 Phase 1 拒（ENV-0016），
        // 否则 Phase 2 先建库后 TraceRun::open 才失败，留下空 .duckdb。
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("audit.log");
        std::fs::write(&log, "old").unwrap();
        let script = format!("call --file f prc --trace {} 1\n", log.display());
        let e = steps_of(&script).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0016", "{e}");
        // 不存在的路径放行（Phase 2 才创建）
        let ok = tmp.path().join("new.log");
        let script2 = format!("call --file f prc --trace {} 1\n", ok.display());
        assert!(steps_of(&script2).is_ok());
    }

    #[test]
    fn should_reject_gauss_commands_upfront_without_gauss_feature() {
        // cli feature 不含 gauss：materialize/sync 在预检即拒（不等到执行期）
        #[cfg(not(feature = "gauss"))]
        {
            let e = steps_of("materialize --db w --table t\n").unwrap_err();
            assert_eq!(e.code.to_string(), "ENV-0012", "{e}");
        }
        #[cfg(feature = "gauss")]
        {
            assert!(steps_of("sync --db w --table t --cols a\n").is_ok());
        }
    }
}
