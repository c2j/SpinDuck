//! 命令行参数解析（零依赖手写，AGENTS.md §1：CLI 只做壳）。
//!
//! 契约：解析失败返回 `ENG-0010`（用法错误），退出码 2；未知子命令同上。

use crate::error::EngineError;
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
    Explain { file: PathBuf, routine: String },
    /// 物化：GaussDB 表 → 本地 DuckDB 表（feature `gauss`）。
    Materialize {
        db: PathBuf,
        table: String,
        where_clause: Option<String>,
    },
    /// 回写：本地 DuckDB 表 → GaussDB 追加（feature `gauss`）。
    Sync {
        db: PathBuf,
        table: String,
        cols: Vec<String>,
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
    },
}

/// `call` 的目标来源（design D5/R3，A2-4）。
///
/// 用类型而不是 `Option` 去掉非法状态：给了计划就不该再有裸例程名——计划里已经写了
/// `target` 与例程源清单，两处不一致时谁说了算本就没有答案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// 裸例程名：源码必须逐个 `--file` 传进来（V30.3 那条"例程注册在进程内"的口径）。
    Routine(String),
    /// 运行计划文件：目标例程与例程源都从计划取。
    Plan(PathBuf),
}

fn usage_err(msg: impl std::fmt::Display) -> EngineError {
    EngineError::invariant(10, format!("usage: {msg}"))
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
            let paths: Vec<PathBuf> = argv[1..].iter().map(PathBuf::from).collect();
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

fn parse_verify(rest: &[String]) -> Result<Command, EngineError> {
    match rest {
        [] => Ok(Command::Verify { path: None }),
        [p] => Ok(Command::Verify {
            path: Some(PathBuf::from(p)),
        }),
        _ => Err(usage_err("verify [项目目录]")),
    }
}

fn parse_plan(rest: &[String]) -> Result<Command, EngineError> {
    let mut dir = None;
    let mut schema = None;
    let mut routine = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--dir" => dir = Some(PathBuf::from(next_arg(&mut it, "--dir <项目目录>")?)),
            "--schema" => schema = Some(next_arg(&mut it, "--schema <名>")?),
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
fn parse_deploy(rest: &[String]) -> Result<Command, EngineError> {
    let usage = "deploy <计划文件> --db <本地 DuckDB 文件>";
    let mut db = None;
    let mut plan = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?)),
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

/// `query <库文件> --sql-file F --out F.json`：三个输入都必填（design §6.2 A5）。
///
/// 库文件是位置参数而不是 `--db`，与 `deploy` 同形：`query` 只有一个落盘目标，
/// 没有"可选的库"这种形态（缺库就没有执行面）。
fn parse_query(rest: &[String]) -> Result<Command, EngineError> {
    let usage = "query <本地 DuckDB 文件> --sql-file <F.sql> --out <F.json>";
    let mut db = None;
    let mut sql_file = None;
    let mut out = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--sql-file" => {
                sql_file = Some(PathBuf::from(next_arg(&mut it, "--sql-file <F.sql>")?))
            }
            "--out" => out = Some(PathBuf::from(next_arg(&mut it, "--out <F.json>")?)),
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            _ if db.is_none() => db = Some(PathBuf::from(tok)),
            t => return Err(usage_err(format!("query 只接受一个库文件，多余参数: {t}"))),
        }
    }
    Ok(Command::Query {
        db: db.ok_or_else(|| usage_err(usage))?,
        sql_file: sql_file.ok_or_else(|| usage_err(format!("{usage}（缺 --sql-file）")))?,
        out: out.ok_or_else(|| usage_err(format!("{usage}（缺 --out）")))?,
    })
}

/// `compare a.json b.json [--key k1,k2]`：恰好两个快照文件，键可选。
///
/// 两个操作数没有先后语义上的区别，但报告里 `a=`/`b=` 要能指回命令行，所以位置序
/// 固定为"第一个 = a、第二个 = b"，多给一律用法错——绝不猜第三个是干什么的。
fn parse_compare(rest: &[String]) -> Result<Command, EngineError> {
    let usage = "compare <a.json> <b.json> [--key k1,k2]";
    let mut key: Vec<String> = Vec::new();
    let mut key_given = false;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--key" => {
                key_given = true;
                let raw = next_arg(&mut it, "--key k1,k2")?;
                key = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => files.push(PathBuf::from(t)),
        }
    }
    // 给了 `--key` 却一个列名都没剩下，不能退化成位置序：调用方以为在按键配对，
    // 实际比的是到达顺序（`--key ""`、`--key ,` 都会走到这里）
    if key_given && key.is_empty() {
        return Err(usage_err("compare --key 至少要给一个列名"));
    }
    if files.len() != 2 {
        return Err(usage_err(format!(
            "{usage}（实际给了 {} 个快照文件）",
            files.len()
        )));
    }
    Ok(Command::Compare {
        a: files[0].clone(),
        b: files[1].clone(),
        key,
    })
}

fn parse_call(rest: &[String]) -> Result<Command, EngineError> {
    // `--plan` 先摘出来：计划自带目标例程，此后所有位置参数都是**实参**，
    // 不能再按"第一个位置参数 = 例程名"解读，否则 `call --plan p.toml 20250715` 会把日期
    // 当成例程名。
    let mut plan = None;
    let mut rest2: Vec<String> = Vec::with_capacity(rest.len());
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        if tok == "--plan" {
            plan = Some(PathBuf::from(
                it.next().ok_or_else(|| usage_err("--plan <plan.toml>"))?,
            ));
        } else {
            rest2.push(tok.clone());
        }
    }

    let mut db = None;
    let mut rules = None;
    let mut files = Vec::new();
    let mut routine = None;
    let mut args = Vec::new();
    let mut it = rest2.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => {
                db = Some(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--db <path>"))?,
                ));
            }
            "--rules" => {
                rules = Some(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--rules <path>"))?,
                ));
            }
            "--file" => {
                files.push(PathBuf::from(
                    it.next().ok_or_else(|| usage_err("--file <path>"))?,
                ));
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
    Ok(Command::Call {
        db,
        rules,
        files,
        target,
        args,
    })
}

fn parse_explain(rest: &[String]) -> Result<Command, EngineError> {
    let mut file = None;
    let mut routine = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--file" => file = Some(PathBuf::from(next_arg(&mut it, "--file <path>")?)),
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            _ => {
                if routine.is_none() {
                    routine = Some(tok.clone());
                } else {
                    return Err(usage_err("explain --file <sql> <routine>"));
                }
            }
        }
    }
    Ok(Command::Explain {
        file: file.ok_or_else(|| usage_err("explain --file <sql> <routine>"))?,
        routine: routine.ok_or_else(|| usage_err("explain --file <sql> <routine>"))?,
    })
}

fn parse_materialize(rest: &[String]) -> Result<Command, EngineError> {
    let mut db = None;
    let mut table = None;
    let mut where_clause = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?)),
            "--table" => table = Some(next_arg(&mut it, "--table <name>")?),
            "--where" => where_clause = Some(next_arg(&mut it, "--where <expr>")?),
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => return Err(usage_err(format!("unexpected argument {t}"))),
        }
    }
    Ok(Command::Materialize {
        db: db.ok_or_else(|| usage_err("materialize --db <path> --table <name>"))?,
        table: table.ok_or_else(|| usage_err("materialize --db <path> --table <name>"))?,
        where_clause,
    })
}

fn parse_sync(rest: &[String]) -> Result<Command, EngineError> {
    let mut db = None;
    let mut table = None;
    let mut cols = Vec::new();
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?)),
            "--table" => table = Some(next_arg(&mut it, "--table <name>")?),
            "--cols" => {
                let raw = next_arg(&mut it, "--cols a,b")?;
                cols = raw
                    .split(',')
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect();
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => return Err(usage_err(format!("unexpected argument {t}"))),
        }
    }
    let table = table.ok_or_else(|| usage_err("sync --db <path> --table <name> --cols a,b"))?;
    if cols.is_empty() {
        return Err(usage_err("sync requires --cols a,b,..."));
    }
    Ok(Command::Sync {
        db: db.ok_or_else(|| usage_err("sync --db <path> --table <name> --cols a,b"))?,
        table,
        cols,
    })
}

fn next_arg(it: &mut std::slice::Iter<'_, String>, usage: &str) -> Result<String, EngineError> {
    it.next().cloned().ok_or_else(|| usage_err(usage))
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
     \x20 explain --file <sql> <routine>   打印例程 IR（不执行）\n\
     \x20 verify [项目目录]               从 [[entry]] 出发核验链路对象与场景\n\
     \x20 plan [--dir <项目目录>] [--schema <名>] <routine>\n\
     \x20                                求目标例程闭包并落盘运行计划（缺件即判红）\n\
     \x20 deploy <计划.toml> --db <f>    按运行计划把资产装进落盘的本地 DuckDB（缺件即拒）\n\
     \x20 query <本地 DuckDB 文件> --sql-file <F.sql> --out <F.json>\n\
     \x20                                跑一条 SQL，把结果集落成快照文件（内核文本，未归一）\n\
     \x20 compare <a.json> <b.json> [--key k1,k2]\n\
     \x20                                两份结果快照逐值比对（归一后判值；红 = 退出码 1 + DIFF）\n\
     \x20 materialize --db <f> --table <t> [--where <expr>]\n\
     \x20                                从 GaussDB 物化到本地 DuckDB（需 feature gauss）\n\
     \x20 sync --db <f> --table <t> --cols a,b\n\
     \x20                                本地 DuckDB 回写追加到 GaussDB（需 feature gauss）\n\
     \x20 call [--db <f>] [--rules <f>] [--file <sql>]... <routine> [args...]\n\
     \x20 call --plan <计划.toml> [--db <f>] [--file <sql>]... [args...]\n\
     \x20                                注册例程并调用；计划面自带目标与源清单，缺件即红\n\
     \x20                                （args: 整数/小数/'串'/null；--plan 下位置参数全是实参）\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::SpinValue;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

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
            }
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
    fn should_parse_explain_with_file_and_routine() {
        let c = parse(&v(&["explain", "--file", "p.sql", "prc_x"])).unwrap();
        assert_eq!(
            c,
            Command::Explain {
                file: PathBuf::from("p.sql"),
                routine: "prc_x".to_string(),
            }
        );
        assert!(
            parse(&v(&["explain", "prc_x"])).is_err(),
            "缺 --file 应报用法错"
        );
    }

    #[test]
    fn should_parse_materialize_with_optional_where() {
        let c = parse(&v(&["materialize", "--db", "w.duckdb", "--table", "t1"])).unwrap();
        assert_eq!(
            c,
            Command::Materialize {
                db: PathBuf::from("w.duckdb"),
                table: "t1".to_string(),
                where_clause: None,
            }
        );
        let c = parse(&v(&[
            "materialize",
            "--db",
            "w",
            "--table",
            "t1",
            "--where",
            "bcrq = '20240105'",
        ]))
        .unwrap();
        match c {
            Command::Materialize { where_clause, .. } => {
                assert_eq!(where_clause.as_deref(), Some("bcrq = '20240105'"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn should_require_table_and_cols_for_sync() {
        let e = parse(&v(&["sync", "--db", "w", "--table", "t"])).unwrap_err();
        assert!(e.message.contains("--cols"), "{}", e.message);
        let c = parse(&v(&[
            "sync", "--db", "w", "--table", "t", "--cols", "a, b ,c",
        ]))
        .unwrap();
        assert_eq!(
            c,
            Command::Sync {
                db: PathBuf::from("w"),
                table: "t".to_string(),
                cols: vec!["a".into(), "b".into(), "c".into()],
            }
        );
    }

    #[test]
    fn should_treat_empty_argv_as_help() {
        assert_eq!(parse(&[]).unwrap(), Command::Help);
    }

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

    #[test]
    fn should_parse_query_with_db_and_both_flags() {
        assert_eq!(
            parse(&v(&[
                "query",
                "w.duckdb",
                "--sql-file",
                "q.sql",
                "--out",
                "snap.json"
            ]))
            .unwrap(),
            Command::Query {
                db: PathBuf::from("w.duckdb"),
                sql_file: PathBuf::from("q.sql"),
                out: PathBuf::from("snap.json"),
            }
        );
        // 库文件是位置参数，旗标在前后都成立（与 deploy 同形）
        assert!(parse(&v(&[
            "query",
            "--sql-file",
            "q.sql",
            "w.duckdb",
            "--out",
            "snap.json"
        ]))
        .is_ok());
    }

    #[test]
    fn should_reject_query_with_a_missing_flag_or_a_second_db() {
        let e = parse(&v(&["query", "w.duckdb"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(
            e.message.contains("--sql-file"),
            "缺件面要指认缺哪个旗标：{}",
            e.message
        );
        let e = parse(&v(&["query", "--sql-file", "q.sql", "--out", "s.json"])).unwrap_err();
        assert!(e.message.contains("query <本地 DuckDB 文件>"), "{e}");
        let e = parse(&v(&[
            "query",
            "w.duckdb",
            "v.duckdb",
            "--sql-file",
            "q.sql",
            "--out",
            "s.json",
        ]))
        .unwrap_err();
        assert!(e.message.contains("只接受一个库文件"), "{e}");
        let e = parse(&v(&["query", "w", "--sql-file"])).unwrap_err();
        assert!(e.message.contains("--sql-file <F.sql>"), "{e}");
    }

    #[test]
    fn should_parse_compare_with_two_snapshots_and_split_the_key() {
        assert_eq!(
            parse(&v(&["compare", "a.json", "b.json"])).unwrap(),
            Command::Compare {
                a: PathBuf::from("a.json"),
                b: PathBuf::from("b.json"),
                key: vec![],
            }
        );
        assert_eq!(
            parse(&v(&["compare", "a.json", "b.json", "--key", "k1, k2 ,"])).unwrap(),
            Command::Compare {
                a: PathBuf::from("a.json"),
                b: PathBuf::from("b.json"),
                key: vec!["k1".into(), "k2".into()],
            }
        );
    }

    #[test]
    fn should_reject_compare_without_two_snapshots_or_without_a_key_column() {
        for argv in [
            v(&["compare"]),
            v(&["compare", "a.json"]),
            v(&["compare", "a.json", "b.json", "c.json"]),
        ] {
            let e = parse(&argv).unwrap_err();
            assert_eq!(e.code.to_string(), "ENG-0010", "{argv:?}");
            assert!(e.message.contains("个快照文件"), "{e}");
        }
        // 给了 --key 却一个列名都没有：退化成位置序等于把"没比成"说成"比过了"
        let e = parse(&v(&["compare", "a.json", "b.json", "--key", ""])).unwrap_err();
        assert!(e.message.contains("至少要给一个列名"), "{e}");
        let e = parse(&v(&["compare", "a.json", "b.json", "--key"])).unwrap_err();
        assert!(e.message.contains("--key k1,k2"), "{e}");
    }
}
