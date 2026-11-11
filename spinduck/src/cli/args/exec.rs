//! 内核与 IO 面的解析：`explain`（含 `--trace` 对齐）、`materialize`、`sync`。
//!
//! `materialize` 的多表形态（V27 PR-A2）把"重复 `--table`"与"值内逗号分隔"视为同义，
//! 但 `--where` 与多表同用属语义歧义 ⇒ 用法错；`explain --trace` 只多一个日志路径，
//! 不带它时 explain 的输出与退出码逐字不变。

use super::{dup_flag_err, next_arg, usage_err, Command};
use crate::error::EngineError;
use std::path::PathBuf;

pub(super) fn parse_explain(rest: &[String]) -> Result<Command, EngineError> {
    let mut file = None;
    let mut routine = None;
    let mut trace = None;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--file" => {
                if file.is_some() {
                    return Err(dup_flag_err("--file"));
                }
                file = Some(PathBuf::from(next_arg(&mut it, "--file <path>")?));
            }
            "--trace" => {
                // 对齐视图的输入：一次真实运行的审计日志（text 或 jsonl 都可读）
                if trace.is_some() {
                    return Err(dup_flag_err("--trace"));
                }
                trace = Some(PathBuf::from(next_arg(&mut it, "--trace <path>")?));
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
        trace,
    })
}

pub(super) fn parse_materialize(rest: &[String]) -> Result<Command, EngineError> {
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

pub(super) fn parse_sync(rest: &[String]) -> Result<Command, EngineError> {
    let mut db = None;
    let mut table = None;
    let mut cols = Vec::new();
    let mut where_clause = None;
    let mut mode = None;
    let mut slice = None;
    let mut key = None;
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
            "--where" => {
                if where_clause.is_some() {
                    return Err(dup_flag_err("--where"));
                }
                where_clause = Some(next_arg(&mut it, "--where <expr>")?);
            }
            "--mode" => {
                if mode.is_some() {
                    return Err(dup_flag_err("--mode"));
                }
                mode = Some(next_arg(&mut it, "--mode append|slice|upsert")?);
            }
            "--slice" => {
                if slice.is_some() {
                    return Err(dup_flag_err("--slice"));
                }
                slice = Some(next_arg(&mut it, "--slice k=v[,k=v]")?);
            }
            "--key" => {
                if key.is_some() {
                    return Err(dup_flag_err("--key"));
                }
                key = Some(next_arg(&mut it, "--key a,b")?);
            }
            t if t.starts_with("--") => return Err(usage_err(format!("unknown flag {t}"))),
            t => return Err(usage_err(format!("unexpected argument {t}"))),
        }
    }
    validate_sync_flags(
        mode.as_deref(),
        slice.as_deref(),
        key.is_some(),
        where_clause.as_ref(),
    )?;
    let table = table.ok_or_else(|| usage_err("sync --db <path> --table <name> --cols a,b"))?;
    if cols.is_empty() {
        return Err(usage_err("sync requires --cols a,b,..."));
    }
    Ok(Command::Sync {
        db: db.ok_or_else(|| usage_err("sync --db <path> --table <name> --cols a,b"))?,
        table,
        cols,
        where_clause,
        mode,
        slice,
        key,
    })
}

/// sync 旗标组合校验（用法错，ENG-0010）：模式与配套旗标必须成对、
/// `--where` 仅限 append（缺省）语义、`--mode` 值枚举封闭、slice 谓词 `k=v` 形态。
fn validate_sync_flags(
    mode: Option<&str>,
    slice_raw: Option<&str>,
    has_key: bool,
    where_clause: Option<&String>,
) -> Result<(), EngineError> {
    let has_slice = slice_raw.is_some();
    match mode {
        None | Some("append") => {
            if has_slice {
                return Err(usage_err("--slice 需要 --mode slice"));
            }
            if has_key {
                return Err(usage_err("--key 需要 --mode upsert"));
            }
        }
        Some("slice") => {
            if has_key {
                return Err(usage_err("--key 需要 --mode upsert"));
            }
            if !has_slice {
                return Err(usage_err("--mode slice 需要 --slice k=v[,k=v]"));
            }
            if where_clause.is_some() {
                return Err(usage_err(
                    "--where 仅限 append 语义（缺省模式）；slice 模式下过滤条件就是 slice 谓词本身",
                ));
            }
        }
        Some("upsert") => {
            if has_slice {
                return Err(usage_err("--slice 需要 --mode slice"));
            }
            if !has_key {
                return Err(usage_err("--mode upsert 需要 --key a,b"));
            }
            if where_clause.is_some() {
                return Err(usage_err(
                    "--where 仅限 append 语义（缺省模式）；upsert 模式不接受读取过滤",
                ));
            }
        }
        Some(other) => {
            return Err(usage_err(format!(
                "--mode 取值须为 append|slice|upsert，得到 {other}"
            )));
        }
    }
    if let Some(raw) = slice_raw {
        if raw
            .split(',')
            .any(|seg| !seg.trim().is_empty() && !seg.contains('='))
        {
            return Err(usage_err(
                "--slice 谓词必须是 k=v 形态（逗号分隔；值支持 $ENV / ${ENV} 引用）",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{parse, v};

    #[test]
    fn should_parse_explain_with_file_and_routine() {
        let c = parse(&v(&["explain", "--file", "p.sql", "prc_x"])).unwrap();
        assert_eq!(
            c,
            Command::Explain {
                file: PathBuf::from("p.sql"),
                routine: "prc_x".to_string(),
                trace: None,
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
    fn should_parse_explain_with_a_trace_log_for_alignment() {
        // 关联形态 A：`--trace` 让 explain 读一次真实运行的日志，把每条静态 SQL 的命中对上
        let c = parse(&v(&[
            "explain", "--file", "p.sql", "--trace", "run.log", "prc_x",
        ]))
        .unwrap();
        assert_eq!(
            c,
            Command::Explain {
                file: PathBuf::from("p.sql"),
                routine: "prc_x".to_string(),
                trace: Some(PathBuf::from("run.log")),
            }
        );
        // 旗标要给路径；空手给出等于"要对齐但没有日志"
        let bad = parse(&v(&["explain", "--file", "p.sql", "--trace", "prc_x"])).unwrap_err();
        assert!(
            bad.message.contains("explain --file <sql> <routine>"),
            "{}",
            bad.message
        );
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
                where_clause: None,
                mode: None,
                slice: None,
                key: None,
            }
        );
    }
}
