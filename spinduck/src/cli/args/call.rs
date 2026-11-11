//! `call` 的解析面：目标来源（裸名 / `--plan`）、实参、执行开关与审计旗标。
//!
//! 两条纪律在此定型：`--plan` 之下位置参数**全是实参**（目标在计划里，不再猜例程名，
//! F6）；`--trace*` 三旗标只描述汇，脱离 `--trace` 单独给出即用法错。

use super::{dup_flag_err, usage_err, CallTarget, Command};
use crate::error::EngineError;
use crate::trace::{TraceFormat, TraceLevel};
use std::path::PathBuf;

pub(super) fn parse_call(rest: &[String]) -> Result<Command, EngineError> {
    // `--plan` 先摘出来：计划自带目标例程，此后所有位置参数都是**实参**，
    // 不能再按"第一个位置参数 = 例程名"解读，否则 `call --plan p.toml 20250715` 会把日期
    // 当成例程名。
    let mut plan = None;
    let mut rest2: Vec<String> = Vec::with_capacity(rest.len());
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        if tok == "--plan" {
            if plan.is_some() {
                return Err(dup_flag_err("--plan"));
            }
            plan = Some(PathBuf::from(
                it.next().ok_or_else(|| usage_err("--plan <plan.toml>"))?,
            ));
        } else {
            rest2.push(tok.clone());
        }
    }

    let mut db: Option<PathBuf> = None;
    let mut batch_commit = false;
    let mut step_budget: Option<u64> = None;
    let mut rules = None;
    let mut trace = None;
    let mut trace_format: Option<TraceFormat> = None;
    let mut trace_level: Option<TraceLevel> = None;
    let mut files = Vec::new();
    let mut routine = None;
    let mut args = Vec::new();
    let mut it = rest2.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => {
                if db.is_some() {
                    return Err(dup_flag_err("--db"));
                }
                db = Some(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--db <path>"))?,
                ));
            }
            "--rules" => {
                if rules.is_some() {
                    return Err(dup_flag_err("--rules"));
                }
                rules = Some(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--rules <path>"))?,
                ));
            }
            "--file" => {
                files.push(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--file <path>"))?,
                ));
            }
            "--batch-commit" => {
                batch_commit = true;
            }
            "--step-budget" => {
                if step_budget.is_some() {
                    return Err(dup_flag_err("--step-budget"));
                }
                let v = it
                    .next()
                    .ok_or_else(|| usage_err("--step-budget <n>"))?
                    .parse::<u64>()
                    .map_err(|_| usage_err("--step-budget <n>（正整数）"))?;
                if v == 0 {
                    return Err(usage_err("--step-budget <n>（正整数）"));
                }
                step_budget = Some(v);
            }
            "--trace" => {
                if trace.is_some() {
                    return Err(dup_flag_err("--trace"));
                }
                trace = Some(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--trace <path>"))?,
                ));
            }
            "--trace-format" => {
                if trace_format.is_some() {
                    return Err(dup_flag_err("--trace-format"));
                }
                let v = it
                    .next()
                    .ok_or_else(|| usage_err("--trace-format <text|jsonl>"))?;
                trace_format = Some(
                    TraceFormat::from_flag(v)
                        .ok_or_else(|| usage_err("--trace-format <text|jsonl>"))?,
                );
            }
            "--trace-level" => {
                if trace_level.is_some() {
                    return Err(dup_flag_err("--trace-level"));
                }
                let v = it
                    .next()
                    .ok_or_else(|| usage_err("--trace-level <stmt|sql|bind|all>"))?;
                trace_level = Some(
                    TraceLevel::from_flag(v)
                        .ok_or_else(|| usage_err("--trace-level <stmt|sql|bind|all>"))?,
                );
            }
            t if t.starts_with("--") => {
                return Err(usage_err(format!("unknown flag {t}")));
            }
            _ => {
                if plan.is_none() && routine.is_none() {
                    routine = Some(tok.clone());
                } else {
                    args.push(tok.clone());
                }
            }
        }
    }
    let target = match plan {
        Some(p) => CallTarget::Plan(p),
        // 无计划时第一个位置参数就是例程名（缺失即用法错，绝不猜）
        None => CallTarget::Routine(
            routine.ok_or_else(|| usage_err("call [--plan <plan.toml> | <routine>] [args...]"))?,
        ),
    };
    // F6：--plan 下位置参数全是实参。首位置参数恰为计划 target 时，几乎必是"忘了删例程名"，
    // 静默当实参会把根因盖成 BIZ-0112 之类的值错误——此处直接用法错。计划读不到则跳过本检查
    // （parse 不产生 I/O 错误面，错误仍由执行层按 BIZ/ENV 域给出）。
    if let CallTarget::Plan(p) = &target {
        if let Some(first) = args.first() {
            if let Some(t) = plan_target_if_readable(p) {
                if first == &t {
                    return Err(usage_err(format!(
                        "call --plan 下位置参数全是实参；目标例程来自计划（{t}），\
                         请去掉位置参数或改用无计划形态"
                    )));
                }
            }
        }
    }
    // 形态与级别是给汇用的：没有落盘路径就没有承接者，静默收下等于给出"有日志"的假象。
    if trace.is_none() && (trace_format.is_some() || trace_level.is_some()) {
        return Err(usage_err(
            "--trace-format/--trace-level 需要 --trace <path>；单独给出无落盘处",
        ));
    }
    Ok(Command::Call {
        db,
        rules,
        files,
        target,
        args,
        batch_commit,
        step_budget,
        trace,
        trace_format: trace_format.unwrap_or_default(),
        trace_level: trace_level.unwrap_or_default(),
    })
}

/// F6 辅助：读取计划文件的 `target`（不可读/不可解析即 None，不在 parse 阶段报 I/O 错）。
fn plan_target_if_readable(p: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(p).ok()?;
    crate::plan::RunPlan::from_toml_str(&text)
        .ok()
        .map(|x| x.target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{parse, v};
    use crate::value::SpinValue;

    #[test]
    fn should_accept_repeated_file_flag_for_call() {
        // F5 边界：--file 有意可多次，保持合法
        let c = parse(&v(&["call", "--file", "a.sql", "--file", "b.sql", "r"])).unwrap();
        match c {
            Command::Call { files, .. } => {
                assert_eq!(files.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn should_reject_call_plan_with_target_as_first_positional() {
        // F6：--plan 下位置参数全是实参，例程名误入无防护会把 BIZ-0112 当根因——
        // 首位置参数 == 计划 target 时直接用法错（exit 2）
        let dir = std::env::temp_dir().join(format!("spinduck-f6-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("p.toml");
        std::fs::write(
            &plan,
            "[plan]\nschema_version = 1\ntarget = \"fnc_fee\"\n\
             generator = \"test\"\nproject_dir = \".\"\n[sources]\n",
        )
        .unwrap();
        let e = parse(&v(&["call", "--plan", &plan.to_string_lossy(), "fnc_fee"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("fnc_fee"), "{}", e.message);
        // 非目标名的位置参数仍是实参（守护既有行为）
        let c = parse(&v(&["call", "--plan", &plan.to_string_lossy(), "20250715"])).unwrap();
        match c {
            Command::Call { args, .. } => assert_eq!(args, v(&["20250715"])),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_skip_target_check_when_plan_unreadable() {
        // F6 折衷：parse 不因计划读不到而报错（I/O 错误留给执行层，保持解析零 I/O 错误面）
        let c = parse(&v(&[
            "call",
            "--plan",
            "/nonexistent/p.toml",
            "whatever",
            "1",
        ]));
        assert!(c.is_ok(), "计划不可读时 parse 不得抢判: {c:?}");
    }

    #[test]
    fn should_parse_call_with_flags_and_args() {
        let c = parse(&v(&[
            "call", "--db", "x.duckdb", "--file", "pkg.sql", "prc_x", "1", "2",
        ]))
        .unwrap();
        assert_eq!(
            c,
            Command::Call {
                db: Some(PathBuf::from("x.duckdb")),
                rules: None,
                files: vec![PathBuf::from("pkg.sql")],
                target: CallTarget::Routine("prc_x".to_string()),
                args: v(&["1", "2"]),
                batch_commit: false,
                step_budget: None,
                trace: None,
                trace_format: TraceFormat::Text,
                trace_level: TraceLevel::Bind,
            }
        );
    }

    #[test]
    fn should_take_every_positional_as_an_argument_when_call_uses_a_plan() {
        // `--plan` 的形态：目标例程来自计划，所以位置参数**全是实参**——把第一个当例程名
        // 就等于把计划的 target 再解释一遍，两处不一致时没有答案。
        let c = parse(&v(&[
            "call",
            "--plan",
            "p.toml",
            "--db",
            "w.duckdb",
            "'20250715'",
            "'0130000001'",
        ]))
        .unwrap();
        assert_eq!(
            c,
            Command::Call {
                db: Some(PathBuf::from("w.duckdb")),
                rules: None,
                files: vec![],
                target: CallTarget::Plan(PathBuf::from("p.toml")),
                args: v(&["'20250715'", "'0130000001'"]),
                batch_commit: false,
                step_budget: None,
                trace: None,
                trace_format: TraceFormat::Text,
                trace_level: TraceLevel::Bind,
            }
        );
    }

    #[test]
    fn should_parse_call_with_step_budget() {
        // 复核应修 C：100k 级游标循环默认预算（1M 步）跑不动，CLI 需放大入口
        let c = parse(&v(&["call", "--step-budget", "20000000", "prc_x"])).unwrap();
        assert_eq!(
            c,
            Command::Call {
                db: None,
                rules: None,
                files: vec![],
                target: CallTarget::Routine("prc_x".to_string()),
                args: vec![],
                batch_commit: false,
                step_budget: Some(20_000_000),
                trace: None,
                trace_format: TraceFormat::Text,
                trace_level: TraceLevel::Bind,
            }
        );
        let bad = parse(&v(&["call", "--step-budget", "0", "prc_x"])).unwrap_err();
        assert!(bad.message.contains("--step-budget"));
    }

    #[test]
    fn should_parse_trace_flags_into_the_call_command() {
        // 只给 --trace 时是"默认形态"：文本 + bind 级（已确认的原型口径）
        match parse(&v(&["call", "--trace", "run.log", "r", "1"])).unwrap() {
            Command::Call {
                trace,
                trace_format,
                trace_level,
                args,
                ..
            } => {
                assert_eq!(trace, Some(PathBuf::from("run.log")));
                assert_eq!(trace_format, TraceFormat::Text);
                assert_eq!(trace_level, TraceLevel::Bind);
                assert_eq!(args, v(&["1"]));
            }
            other => panic!("{other:?}"),
        }
        match parse(&v(&[
            "call",
            "--trace",
            "run.jsonl",
            "--trace-format",
            "jsonl",
            "--trace-level",
            "all",
            "r",
        ]))
        .unwrap()
        {
            Command::Call {
                trace,
                trace_format,
                trace_level,
                ..
            } => {
                assert_eq!(trace, Some(PathBuf::from("run.jsonl")));
                assert_eq!(trace_format, TraceFormat::Jsonl);
                assert_eq!(trace_level, TraceLevel::All);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn should_reject_invalid_trace_format_and_level() {
        let e = parse(&v(&["call", "--trace", "x", "--trace-format", "yaml", "r"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(
            e.message.contains("--trace-format <text|jsonl>"),
            "{}",
            e.message
        );
        let e = parse(&v(&["call", "--trace", "x", "--trace-level", "loud", "r"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(
            e.message.contains("--trace-level <stmt|sql|bind|all>"),
            "{}",
            e.message
        );
    }

    #[test]
    fn should_reject_trace_shape_flags_without_a_trace_path() {
        // 形态与级别是给汇用的：没有 --trace 就无处可写，静默收下等于给用户"有日志"的假象
        for argv in [
            v(&["call", "--trace-format", "jsonl", "r"]),
            v(&["call", "--trace-level", "all", "r"]),
        ] {
            let e = parse(&argv).unwrap_err();
            assert_eq!(e.code.to_string(), "ENG-0010", "{argv:?}");
            assert!(e.message.contains("--trace"), "{}", e.message);
        }
    }

    #[test]
    fn should_reject_duplicated_trace_path_flag() {
        let e = parse(&v(&["call", "--trace", "a", "--trace", "b", "r"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("重复旗标"), "{}", e.message);
    }
}
