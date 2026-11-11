//! 命令行参数解析（零依赖手写，AGENTS.md §1：CLI 只做壳）。
//!
//! 契约：解析失败返回 `ENG-0010`（用法错误），退出码 2；未知子命令同上。
//!
//! 按子命令职责分文件（§3 单文件 ≤500 行，先例见 `tests/cli.rs`）：`call` 面在
//! `call.rs`，项目/计划/部署面在 `project.rs`，快照面在 `snapshot.rs`，内核与 IO
//! 面在 `exec.rs`。本文件只留 `Command`/`CallTarget` 契约、`parse` 派发、共享旗标
//! 工具与 usage 文案。

mod call;
mod exec;
mod project;
mod snapshot;

use call::parse_call;
use exec::{parse_explain, parse_materialize, parse_sync};
use project::{parse_deploy, parse_plan, parse_verify};
use snapshot::{parse_compare, parse_query};

use crate::error::EngineError;
use crate::trace::{TraceFormat, TraceLevel};
use std::path::PathBuf;

/// 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 打印版本。
    Version,
    /// 打印用法。
    Help,
    /// 语料编译体检（文件或目录，可多个）。
    Compile { paths: Vec<PathBuf> },
    /// 规则文件探针（版本锚定检查）。
    RulesProbe { path: PathBuf },
    /// 打印例程 IR（调试/回归，不执行）。
    /// `trace` = 审计日志路径（关联形态 A）：给出时每条静态 SQL 行尾追加命中注记。
    Explain {
        file: PathBuf,
        routine: String,
        trace: Option<PathBuf>,
    },
    /// 物化：GaussDB 表 → 本地 DuckDB 表（feature `gauss`）。
    Materialize {
        db: PathBuf,
        /// 一张或多张表：`--table` 可重复，值内逗号分隔同义（V27 §3.6 多表并行）。
        tables: Vec<String>,
        /// 可选 WHERE 过滤（仅单表形态；多表形态拒绝，语义歧义）。
        where_clause: Option<String>,
        /// 并发度（默认 4；0 视为用法错）。
        jobs: Option<usize>,
    },
    /// 回写：本地 DuckDB 表 → GaussDB（feature `gauss`）。
    /// 默认（无 mode/where）保持 V26 append 全表契约逐字不变。
    Sync {
        db: PathBuf,
        table: String,
        cols: Vec<String>,
        /// 读取过滤（仅 append 语义；slice/upsert 模式拒绝）。
        where_clause: Option<String>,
        /// 回写模式：append（缺省）/ slice / upsert（原始串，engine 侧解析）。
        mode: Option<String>,
        /// mode=slice 的结构化谓词 `k=v,k=v`（值支持 `$ENV` / `${ENV}` 引用）。
        slice: Option<String>,
        /// mode=upsert 的键列 `a,b`。
        key: Option<String>,
    },
    /// 链路校验（§12.1 入口声明）：从 `[[entry]]` 出发逐对象核验。
    Verify { path: Option<PathBuf> },
    /// 运行计划生成（design §6.1）：目标例程 → 闭包清单落盘。
    Plan {
        dir: Option<PathBuf>,
        routine: String,
        schema: Option<String>,
    },
    /// 按运行计划把资产装进**落盘**的本地 DuckDB（design §6.2，A2-3）。
    Deploy { plan: PathBuf, db: PathBuf },
    /// 跑一条 SQL 落盘结果快照（design §6.2，A5 本地半边）。
    Query {
        db: PathBuf,
        sql_file: PathBuf,
        out: PathBuf,
    },
    /// 两份结果快照逐值比对（design §6.2，A5 本地半边）。
    Compare {
        a: PathBuf,
        b: PathBuf,
        key: Vec<String>,
    },
    /// 调用例程（可先从 SQL 文件注册例程；可选持久化库文件）。
    Call {
        db: Option<PathBuf>,
        rules: Option<PathBuf>,
        files: Vec<PathBuf>,
        target: CallTarget,
        args: Vec<String>,
        /// 合并 COMMIT 开关（V26，默认关；见 writebuf.rs 语义边界）。
        batch_commit: bool,
        /// 步数预算覆盖（复核应修 C：默认预算跑不动 100k 级游标循环，
        /// 大业务经此入口放大；None = 用引擎默认值）。
        step_budget: Option<u64>,
        /// 审计日志落盘路径：None = 不记（默认，stdout 契约因此逐字不变）。
        trace: Option<PathBuf>,
        /// 日志形态（`--trace-format`，默认 text）；只在 `--trace` 给出时有意义。
        trace_format: TraceFormat,
        /// 详细级别（`--trace-level`，默认 bind）；同上。
        trace_level: TraceLevel,
    },
}

/// `call` 的目标来源（design D5/R3，A2-4）。
///
/// 用类型而不是 `Option` 去掉非法状态：给了计划就不该再有裸例程名——计划里已经写了
/// `target` 与例程源清单，两处不一致时谁说了算本就没有答案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// 裸例程名：源码必须逐个 `--file` 传进来（V31.3 那条"例程注册在进程内"的口径）。
    Routine(String),
    /// 运行计划文件：目标例程与例程源都从计划取。
    Plan(PathBuf),
}

pub(super) fn usage_err(msg: impl std::fmt::Display) -> EngineError {
    EngineError::invariant(10, format!("usage: {msg}"))
}

/// 测试夹具：`&[&str]` → argv 形态的 `Vec<String>`，各解析子模块共用。
#[cfg(test)]
pub(crate) fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// 解析 argv（不含程序名）。
pub fn parse(argv: &[String]) -> Result<Command, EngineError> {
    let Some(head) = argv.first() else {
        return Ok(Command::Help);
    };
    match head.as_str() {
        "version" | "--version" | "-V" => Ok(Command::Version),
        "help" | "--help" | "-h" => Ok(Command::Help),
        "compile" => {
            // F4：与其余子命令同一旗标纪律——`--` 前缀一律拒，绝不当路径静默读
            let paths: Vec<PathBuf> = argv[1..]
                .iter()
                .map(|t| {
                    if t.starts_with("--") {
                        Err(usage_err(format!("compile 不接受旗标，未知参数: {t}")))
                    } else {
                        Ok(PathBuf::from(t))
                    }
                })
                .collect::<Result<_, _>>()?;
            if paths.is_empty() {
                return Err(usage_err("compile <path>..."));
            }
            Ok(Command::Compile { paths })
        }
        "rules" => {
            if argv.get(1).map(|s| s.as_str()) != Some("probe") {
                return Err(usage_err("rules probe <rules.toml>"));
            }
            let path = argv
                .get(2)
                .ok_or_else(|| usage_err("rules probe <rules.toml>"))?;
            Ok(Command::RulesProbe {
                path: PathBuf::from(path),
            })
        }
        "explain" => parse_explain(&argv[1..]),
        "materialize" => parse_materialize(&argv[1..]),
        "sync" => parse_sync(&argv[1..]),
        "verify" => parse_verify(&argv[1..]),
        "plan" => parse_plan(&argv[1..]),
        "deploy" => parse_deploy(&argv[1..]),
        "query" => parse_query(&argv[1..]),
        "compare" => parse_compare(&argv[1..]),
        "call" => parse_call(&argv[1..]),
        other => Err(usage_err(format!("unknown subcommand {other}"))),
    }
}

pub(super) fn next_arg(
    it: &mut std::slice::Iter<'_, String>,
    usage: &str,
) -> Result<String, EngineError> {
    it.next().cloned().ok_or_else(|| usage_err(usage))
}

/// F5：语义单值的旗标重复给 = 用法错（与位置参数重复即报错同一纪律）。
pub(super) fn dup_flag_err(flag: &str) -> EngineError {
    usage_err(format!("重复旗标 {flag}"))
}

/// 字面量 → SpinValue 推断（CLI 入参）：整数 / 小数 / 带引号串 / 其余按串。
pub fn parse_literal(text: &str) -> crate::value::SpinValue {
    use crate::value::SpinValue;
    let t = text.trim();
    if t.eq_ignore_ascii_case("null") {
        return SpinValue::Null;
    }
    if t.eq_ignore_ascii_case("true") {
        return SpinValue::Bool(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return SpinValue::Bool(false);
    }
    if let Some(inner) = t.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return SpinValue::Str(inner.to_string());
    }
    if let Ok(i) = t.parse::<i64>() {
        return SpinValue::Int(i);
    }
    if let Some((ip, fp)) = t.split_once('.') {
        if !ip.is_empty() && !fp.is_empty() && fp.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(unscaled) = format!("{ip}{fp}").parse::<i128>() {
                let scale = fp.len().min(18) as u8;
                return SpinValue::Decimal(unscaled, 38, scale);
            }
        }
    }
    SpinValue::Str(t.to_string())
}

/// 用法文本（`help` 输出）。
pub fn usage() -> &'static str {
    "spinduck <command>\n\
     \n\
     commands:\n\
     \x20 version                         打印版本\n\
     \x20 compile <path>...               语料编译体检（递归 *.sql）\n\
     \x20 rules probe <rules.toml>        规则文件版本锚定检查\n\
     \x20 explain --file <sql> [--trace <审计日志>] <routine>\n\
     \x20                                打印例程 IR（不执行）；给 --trace 则逐行标注该次真实运行的命中\n\
     \x20 verify [项目目录]               从 [[entry]] 出发核验链路对象与场景\n\
     \x20 plan [--dir <项目目录>] [--schema <名>] <routine>\n\
     \x20                                求目标例程闭包并落盘运行计划（缺件即判红）\n\
     \x20 deploy <计划.toml> --db <f>    按运行计划把资产装进落盘的本地 DuckDB（缺件即拒）\n\
     \x20 query --db <f> --sql-file <F.sql> --out <F.json>\n\
     \x20                                跑一条 SQL，把结果集落成快照文件（内核文本，未归一）\n\
     \x20 compare <a.json> <b.json> [--key k1,k2]\n\
     \x20                                两份结果快照逐值比对（归一后判值；红 = 退出码 1 + DIFF）\n\
     \x20 materialize --db <f> --table <t>[,<t>...] [--table <t>]... [--jobs <n>] [--where <expr>]\n\
     \x20                                从 GaussDB 物化到本地 DuckDB（需 feature gauss）；\n\
     \x20                                  --table 可重复/逗号分隔，重复表名（含大小写等价）= 用法错；\n\
     \x20                                  --jobs 默认 4，多表进程内并行，单表失败不中断其余表\n\
     \x20 sync --db <f> --table <t> --cols a,b [--where <expr>]\n\
     \x20      [--mode append|slice|upsert] [--slice k=$ENV,...] [--key a,b]\n\
     \x20                                本地 DuckDB 回写 GaussDB（需 feature gauss；\n\
     \x20                                  缺省 append；slice=同事务先删后灌幂等，\n\
     \x20                                  upsert=键控合并；--slice 值支持 $ENV 引用）\n\
     \x20 call [--db <f>] [--rules <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>]\n\
     \x20      [--trace <f> [--trace-format <text|jsonl>] [--trace-level <stmt|sql|bind|all>]] <routine> [args...]\n\
     \x20 call --plan <计划.toml> [--db <f>] [--rules <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>]\n\
     \x20      [--trace <f> [--trace-format <text|jsonl>] [--trace-level <stmt|sql|bind|all>]] [args...]\n\
     \x20                                注册例程并调用；计划面自带目标与源清单，缺件即红\n\
     \x20                                （args: 整数/小数/'串'/null；--plan 下位置参数全是实参；\n\
     \x20                                  规则口径：内置种子 < 项目发现 < --rules；\n\
     \x20                                  --trace 把审计日志逐行落盘（默认 text），sidecar <日志>.sql\n\
     \x20                                  按行内 sqlid 存 SQL 全文，可 grep 反查；日志目标已存在即\n\
     \x20                                  ENV-0016 拒绝执行、不覆盖上一轮审计，重跑请换路径）\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::SpinValue;

    #[test]
    fn should_parse_compile_with_multiple_paths() {
        let c = parse(&v(&["compile", "a", "b"])).unwrap();
        assert_eq!(
            c,
            Command::Compile {
                paths: vec![PathBuf::from("a"), PathBuf::from("b")]
            }
        );
    }

    #[test]
    fn should_reject_duplicate_single_value_flag() {
        // F5：语义单值的旗标重复给 = 手误，不得静默 last-wins（与位置参数重复即报错同纪律）
        for argv in [
            v(&["plan", "--dir", "a", "--dir", "b", "r"]),
            v(&["plan", "--schema", "a", "--schema", "b", "r"]),
            v(&["deploy", "--db", "a", "--db", "b", "p.toml"]),
            v(&[
                "query",
                "--db",
                "w",
                "--sql-file",
                "s1",
                "--sql-file",
                "s2",
                "--out",
                "o",
            ]),
            v(&[
                "query",
                "--db",
                "w",
                "--sql-file",
                "q",
                "--out",
                "o1",
                "--out",
                "o2",
            ]),
            v(&["compare", "a.json", "b.json", "--key", "k1", "--key", "k2"]),
            v(&["explain", "--file", "a.sql", "--file", "b.sql", "foo"]),
            v(&["materialize", "--db", "a", "--db", "b", "--table", "t"]),
            v(&[
                "materialize",
                "--db",
                "d",
                "--table",
                "t",
                "--where",
                "a",
                "--where",
                "b",
            ]),
            v(&[
                "sync", "--db", "a", "--db", "b", "--table", "t", "--cols", "c",
            ]),
            v(&[
                "sync", "--db", "d", "--table", "t", "--table", "u", "--cols", "c",
            ]),
            v(&[
                "sync", "--db", "d", "--table", "t", "--cols", "a", "--cols", "b",
            ]),
            v(&["call", "--db", "a", "--db", "b", "r"]),
            v(&["call", "--rules", "a", "--rules", "b", "r"]),
            v(&["call", "--plan", "a", "--plan", "b"]),
            v(&["call", "--step-budget", "1", "--step-budget", "2", "r"]),
        ] {
            let e = parse(&argv).unwrap_err();
            assert!(e.message.contains("重复旗标"), "{argv:?}: {e}");
        }
    }

    #[test]
    fn should_reject_compile_with_flag_argument() {
        // F4：compile 是唯一不拒 `--` 前缀的分支会把旗标当路径读——统一为用法错
        let e = parse(&v(&["compile", "--help"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("compile 不接受旗标"), "{}", e.message);
        let e = parse(&v(&["compile", "--rules", "x", "dir"])).unwrap_err();
        assert!(e.message.contains("--rules"), "{}", e.message);
    }

    #[test]
    fn should_reject_compile_without_path() {
        let e = parse(&v(&["compile"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
    }

    #[test]
    fn should_reject_unknown_subcommand() {
        let e = parse(&v(&["frobnicate"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("unknown subcommand"));
    }

    #[test]
    fn usage_snapshot_should_mention_rules_in_plan_form() {
        // F8：usage 与解析器能力对齐（--plan 形态本就接受 --rules），并写明规则三通道口径
        let u = usage();
        let plan_line = u
            .lines()
            .find(|l| l.contains("call --plan"))
            .expect("usage 必含 call --plan 行");
        assert!(
            plan_line.contains("--rules"),
            "call --plan 行须列 --rules: {plan_line}"
        );
        assert!(
            u.contains("内置种子 < 项目发现 < --rules"),
            "规则口径说明缺失: {u}"
        );
    }

    #[test]
    fn usage_snapshot_should_mention_the_trace_overwrite_refusal() {
        // 重跑会清掉上一轮审计这件事必须能在 usage 里查到，而不是只写在源码注释里
        let u = usage();
        assert!(u.contains("ENV-0016"), "usage 须写明拒绝覆盖的错误码: {u}");
        assert!(
            u.contains("不覆盖上一轮审计"),
            "usage 须说明为什么拒绝: {u}"
        );
    }

    #[test]
    fn should_reject_unknown_flag() {
        let e = parse(&v(&["call", "--nope", "p"])).unwrap_err();
        assert!(e.message.contains("unknown flag"));
    }

    #[test]
    fn should_infer_literal_types() {
        assert_eq!(parse_literal("42"), SpinValue::Int(42));
        assert_eq!(parse_literal("'42'"), SpinValue::Str("42".into()));
        assert_eq!(parse_literal("1.25"), SpinValue::Decimal(125, 38, 2));
        assert_eq!(parse_literal("null"), SpinValue::Null);
        assert_eq!(parse_literal("TRUE"), SpinValue::Bool(true));
        assert_eq!(parse_literal("abc"), SpinValue::Str("abc".into()));
    }

    #[test]
    fn should_treat_empty_argv_as_help() {
        assert_eq!(parse(&[]).unwrap(), Command::Help);
    }
}
