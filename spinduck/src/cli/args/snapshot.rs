//! 快照裁决面的解析：`query`（一条 SQL → 快照）与 `compare`（两份快照逐值比对）。
//!
//! F2-A：`query` 的库文件改 `--db` 旗标、位置参数一律拒，与 deploy/materialize/sync/call
//! 同形；`compare` 的两个操作数按位置序固定为 a/b，多给即拒。

use super::{dup_flag_err, next_arg, usage_err, Command};
use crate::error::EngineError;
use std::path::PathBuf;

/// `query --db <f> --sql-file F --out F.json`：三件输入全旗标、必填（design §6.2 A5）。
///
/// F2-A：库文件改为 `--db` 旗标，与 deploy/materialize/sync/call 统一——此前位置参数形态
/// 与注释里"与 deploy 同形"的说法不符（deploy 的位置参数是计划、db 用旗标）。
pub(super) fn parse_query(rest: &[String]) -> Result<Command, EngineError> {
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
pub(super) fn parse_compare(rest: &[String]) -> Result<Command, EngineError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{parse, v};

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
