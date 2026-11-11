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
        /// 一张或多张表：`--table` 可重复，值内逗号分隔同义（V27 §3.6 多表并行）。
        tables: Vec<String>,
        /// 可选 WHERE 过滤（仅单表形态；多表形态拒绝，语义歧义）。
        where_clause: Option<String>,
        /// 并发度（默认 4；0 视为用法错）。
        jobs: Option<usize>,
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
        /// 合并 COMMIT 开关（V26，默认关；见 writebuf.rs 语义边界）。
        batch_commit: bool,
        /// 步数预算覆盖（复核应修 C：默认预算跑不动 100k 级游标循环，
        /// 大业务经此入口放大；None = 用引擎默认值）。
        step_budget: Option<u64>,
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
fn parse_deploy(rest: &[String]) -> Result<Command, EngineError> {
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

/// `query --db <f> --sql-file F --out F.json`：三件输入全旗标、必填（design §6.2 A5）。
///
/// F2-A：库文件改为 `--db` 旗标，与 deploy/materialize/sync/call 统一——此前位置参数形态
/// 与注释里"与 deploy 同形"的说法不符（deploy 的位置参数是计划、db 用旗标）。
fn parse_query(rest: &[String]) -> Result<Command, EngineError> {
    let usage = "query --db <本地 DuckDB 文件> --sql-file <F.sql> --out <F.json>";
    let mut db = None;
    let mut sql_file = None;
    let mut out = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => {
                if db.is_some() {
                    return Err(dup_flag_err("--db"));
                }
                db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?));
            }
            "--sql-file" => {
                if sql_file.is_some() {
                    return Err(dup_flag_err("--sql-file"));
                }
                sql_file = Some(PathBuf::from(next_arg(&mut it, "--sql-file <F.sql>")?));
            }
            "--out" => {
                if out.is_some() {
                    return Err(dup_flag_err("--out"));
                }
                out = Some(PathBuf::from(next_arg(&mut it, "--out <F.json>")?));
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => return Err(usage_err(format!("query 不接受位置参数: {t}"))),
        }
    }
    Ok(Command::Query {
        db: db.ok_or_else(|| usage_err(format!("{usage}（缺 --db）")))?,
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
                if key_given {
                    return Err(dup_flag_err("--key"));
                }
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
    Ok(Command::Call {
        db,
        rules,
        files,
        target,
        args,
        batch_commit,
        step_budget,
    })
}

/// F6 辅助：读取计划文件的 `target`（不可读/不可解析即 None，不在 parse 阶段报 I/O 错）。
fn plan_target_if_readable(p: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(p).ok()?;
    crate::plan::RunPlan::from_toml_str(&text)
        .ok()
        .map(|x| x.target)
}

fn parse_explain(rest: &[String]) -> Result<Command, EngineError> {
    let mut file = None;
    let mut routine = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--file" => {
                if file.is_some() {
                    return Err(dup_flag_err("--file"));
                }
                file = Some(PathBuf::from(next_arg(&mut it, "--file <path>")?));
            }
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
    // --table 可重复出现，值内逗号分隔同义（V27 §3.6 多表并行）
    let mut tables: Vec<String> = Vec::new();
    let mut where_clause = None;
    let mut jobs = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => {
                if db.is_some() {
                    return Err(dup_flag_err("--db"));
                }
                db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?));
            }
            "--table" => {
                let v = next_arg(&mut it, "--table <name>[,name…]")?;
                tables.extend(
                    v.split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string),
                );
            }
            "--where" => {
                if where_clause.is_some() {
                    return Err(dup_flag_err("--where"));
                }
                where_clause = Some(next_arg(&mut it, "--where <expr>")?);
            }
            "--jobs" => {
                if jobs.is_some() {
                    return Err(dup_flag_err("--jobs"));
                }
                let v = next_arg(&mut it, "--jobs <n>")?;
                let n: usize = v
                    .parse()
                    .map_err(|_| usage_err(format!("materialize --jobs 须为正整数：{v}")))?;
                if n == 0 {
                    return Err(usage_err("materialize --jobs 须 ≥ 1"));
                }
                jobs = Some(n);
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => return Err(usage_err(format!("unexpected argument {t}"))),
        }
    }
    if tables.is_empty() {
        return Err(usage_err("materialize --db <path> --table <name>[,name…]"));
    }
    // WHERE 与多表同用是语义歧义（过滤条件归属哪张表？）——用法错，绝不猜
    if tables.len() > 1 && where_clause.is_some() {
        return Err(usage_err(
            "materialize --where 仅限单表形态；多表请逐表指定 --where 分次物化",
        ));
    }
    Ok(Command::Materialize {
        db: db.ok_or_else(|| usage_err("materialize --db <path> --table <name>[,name…]"))?,
        tables,
        where_clause,
        jobs,
    })
}

fn parse_sync(rest: &[String]) -> Result<Command, EngineError> {
    let mut db = None;
    let mut table = None;
    let mut cols = Vec::new();
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--db" => {
                if db.is_some() {
                    return Err(dup_flag_err("--db"));
                }
                db = Some(PathBuf::from(next_arg(&mut it, "--db <path>")?));
            }
            "--table" => {
                if table.is_some() {
                    return Err(dup_flag_err("--table"));
                }
                table = Some(next_arg(&mut it, "--table <name>")?);
            }
            "--cols" => {
                if !cols.is_empty() {
                    return Err(dup_flag_err("--cols"));
                }
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

/// F5：语义单值的旗标重复给 = 用法错（与位置参数重复即报错同一纪律）。
fn dup_flag_err(flag: &str) -> EngineError {
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
     \x20 explain --file <sql> <routine>   打印例程 IR（不执行）\n\
     \x20 verify [项目目录]               从 [[entry]] 出发核验链路对象与场景\n\
     \x20 plan [--dir <项目目录>] [--schema <名>] <routine>\n\
     \x20                                求目标例程闭包并落盘运行计划（缺件即判红）\n\
     \x20 deploy <计划.toml> --db <f>    按运行计划把资产装进落盘的本地 DuckDB（缺件即拒）\n\
     \x20 query --db <f> --sql-file <F.sql> --out <F.json>\n\
     \x20                                跑一条 SQL，把结果集落成快照文件（内核文本，未归一）\n\
     \x20 compare <a.json> <b.json> [--key k1,k2]\n\
     \x20                                两份结果快照逐值比对（归一后判值；红 = 退出码 1 + DIFF）\n\
     \x20 materialize --db <f> --table <t> [--where <expr>]\n\
     \x20                                从 GaussDB 物化到本地 DuckDB（需 feature gauss）\n\
     \x20 sync --db <f> --table <t> --cols a,b\n\
     \x20                                本地 DuckDB 回写追加到 GaussDB（需 feature gauss）\n\
     \x20 call [--db <f>] [--rules <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] <routine> [args...]\n\
     \x20 call --plan <计划.toml> [--db <f>] [--rules <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] [args...]\n\
     \x20                                注册例程并调用；计划面自带目标与源清单，缺件即红\n\
     \x20                                （args: 整数/小数/'串'/null；--plan 下位置参数全是实参；\n\
     \x20                                  规则口径：内置种子 < 项目发现 < --rules）\n"
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
            }
        );
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
            }
        );
        let bad = parse(&v(&["call", "--step-budget", "0", "prc_x"])).unwrap_err();
        assert!(bad.message.contains("--step-budget"));
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
    fn should_parse_materialize_multi_table_via_repeated_and_comma_forms() {
        // 重复 --table 与逗号分隔同义（V27 §3.6），两者混用取并集（顺序保留）
        let c = parse(&v(&[
            "materialize",
            "--db",
            "w.duckdb",
            "--table",
            "t1",
            "--table",
            "t2,t3",
            "--jobs",
            "2",
        ]))
        .unwrap();
        match c {
            Command::Materialize {
                db: _,
                tables,
                where_clause,
                jobs,
            } => {
                assert_eq!(tables, vec!["t1", "t2", "t3"]);
                assert_eq!(where_clause, None);
                assert_eq!(jobs, Some(2));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn should_reject_where_with_multi_table_as_usage_error() {
        let err = parse(&v(&[
            "materialize",
            "--db",
            "w.duckdb",
            "--table",
            "t1,t2",
            "--where",
            "bcrq='20240105'",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("--where 仅限单表形态"), "{err}");
    }

    #[test]
    fn should_reject_jobs_zero_and_non_numeric_as_usage_error() {
        for bad in ["0", "x"] {
            let err = parse(&v(&[
                "materialize",
                "--db",
                "w.duckdb",
                "--table",
                "t1",
                "--jobs",
                bad,
            ]))
            .unwrap_err();
            assert!(err.to_string().contains("--jobs"), "{bad}: {err}");
        }
    }

    #[test]
    fn should_parse_materialize_with_optional_where() {
        let c = parse(&v(&["materialize", "--db", "w.duckdb", "--table", "t1"])).unwrap();
        assert_eq!(
            c,
            Command::Materialize {
                db: PathBuf::from("w.duckdb"),
                tables: vec!["t1".to_string()],
                where_clause: None,
                jobs: None,
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
    fn should_parse_query_with_all_three_flags_in_any_order() {
        // F2-A：三件输入全旗标、顺序自由（与 deploy/materialize/sync/call 的 --db 同形）
        assert_eq!(
            parse(&v(&[
                "query",
                "--db",
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
        assert!(parse(&v(&[
            "query",
            "--sql-file",
            "q.sql",
            "--db",
            "w.duckdb",
            "--out",
            "snap.json"
        ]))
        .is_ok());
    }

    #[test]
    fn should_reject_query_with_a_missing_flag_or_a_positional() {
        // 缺件面逐旗标指认
        let e = parse(&v(&["query", "--sql-file", "q.sql", "--out", "s.json"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENG-0010");
        assert!(e.message.contains("--db"), "{e}");
        let e = parse(&v(&["query", "--db", "w.duckdb", "--out", "s.json"])).unwrap_err();
        assert!(e.message.contains("--sql-file"), "{e}");
        let e = parse(&v(&["query", "--db", "w.duckdb", "--sql-file", "q.sql"])).unwrap_err();
        assert!(e.message.contains("--out"), "{e}");
        // 位置参数一律拒（旧形态迁移点）
        let e = parse(&v(&[
            "query",
            "w.duckdb",
            "--sql-file",
            "q.sql",
            "--out",
            "s.json",
        ]))
        .unwrap_err();
        assert!(e.message.contains("不接受位置参数"), "{e}");
        let e = parse(&v(&["query", "--db", "w", "--sql-file"])).unwrap_err();
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
