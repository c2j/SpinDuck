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
    /// 调用例程（可先从 SQL 文件注册例程；可选持久化库文件）。
    Call {
        db: Option<PathBuf>,
        rules: Option<PathBuf>,
        files: Vec<PathBuf>,
        routine: String,
        args: Vec<String>,
    },
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
        "call" => parse_call(&argv[1..]),
        other => Err(usage_err(format!("unknown subcommand {other}"))),
    }
}

fn parse_call(rest: &[String]) -> Result<Command, EngineError> {
    let mut db = None;
    let mut rules = None;
    let mut files = Vec::new();
    let mut routine = None;
    let mut args = Vec::new();
    let mut it = rest.iter();
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
                if routine.is_none() {
                    routine = Some(tok.clone());
                } else {
                    args.push(tok.clone());
                }
            }
        }
    }
    let routine = routine.ok_or_else(|| usage_err("call <routine> [args...]"))?;
    Ok(Command::Call {
        db,
        rules,
        files,
        routine,
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
     \x20 materialize --db <f> --table <t> [--where <expr>]
\
     \x20                                从 GaussDB 物化到本地 DuckDB（需 feature gauss）
\
     \x20 sync --db <f> --table <t> --cols a,b
\
     \x20                                本地 DuckDB 回写追加到 GaussDB（需 feature gauss）
\
     \x20 call [--db <f>] [--rules <f>] [--file <sql>]... <routine> [args...]\n\
     \x20                                注册 SQL 文件内例程并调用（args: 整数/小数/'串'/null）\n"
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
                routine: "prc_x".to_string(),
                args: v(&["1", "2"]),
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
}
