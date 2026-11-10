//! `compare`：两份结果快照逐值比对（design §6.2 / §7 A5 本地半边）。
//!
//! 裁决只有两条承诺，本层各锁一条：判**值**不判文本（口径是 `crate::norm`，与黄金层
//! 同一份实现），以及红 = **退出码非 0 + 逐条 DIFF**。列名不是判据，行序只在 `--key`
//! 下才不是判据。

use super::{cli, write, EXIT_FAIL, EXIT_OK, EXIT_USAGE};

fn snap(dir: &std::path::Path, name: &str, text: &str) -> String {
    write(dir, name, text).to_string_lossy().into_owned()
}

#[test]
fn should_exit_0_when_values_match_after_norm() {
    // 尾零、`+` 前缀、`.5` 补零、空串与 NULL 同值——黄金层一直是这么判的
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(
        tmp.path(),
        "a.json",
        r#"{"schema":["k","v"],"rows":[["1","1.50"],["2","+42"]]}"#,
    );
    let b = snap(
        tmp.path(),
        "b.json",
        r#"{"schema":["cnt","val"],"rows":[["1","1.5"],["2","42"]]}"#,
    );
    let (code, out, err) = cli(&["compare", &a, &b]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "compare: rows=2/2 diffs=0\n", "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_exit_1_and_list_every_value_difference() {
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(
        tmp.path(),
        "a.json",
        r#"{"schema":["k","v"],"rows":[["1","x"],["2","y"],["3","z"]]}"#,
    );
    let b = snap(
        tmp.path(),
        "b.json",
        r#"{"schema":["k","v"],"rows":[["1","zz"],["2","8"],["3","z"]]}"#,
    );
    let (code, out, err) = cli(&["compare", &a, &b]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(
        out,
        "DIFF value row=1 col=v a=x b=zz\n\
         DIFF value row=2 col=v a=y b=8\n\
         compare: rows=3/3 diffs=2 逐值不一致\n"
    );
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_exit_1_when_row_counts_differ() {
    // 千万级结果集上"少一行"是最常见的一类真缺陷：整行未匹配也要进报告
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(
        tmp.path(),
        "a.json",
        r#"{"schema":["k"],"rows":[["1"],["2"]]}"#,
    );
    let b = snap(tmp.path(), "b.json", r#"{"schema":["k"],"rows":[["1"]]}"#);
    let (code, out, _) = cli(&["compare", &a, &b]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(
        out,
        "DIFF only-in-a row=2 a=[2]\ncompare: rows=2/1 diffs=1 逐值不一致\n"
    );
}

#[test]
fn should_match_rows_by_key_so_arrival_order_is_not_a_difference() {
    // 实例侧不保证行序；不带 --key 时同一份数据会判成一片红，带 key 即绿
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(
        tmp.path(),
        "a.json",
        r#"{"schema":["k","v"],"rows":[["1","x"],["2","y"]]}"#,
    );
    let b = snap(
        tmp.path(),
        "b.json",
        r#"{"schema":["k","v"],"rows":[["2","y"],["1","x"]]}"#,
    );
    let (code, out, _) = cli(&["compare", &a, &b]);
    assert_eq!(
        code, EXIT_FAIL,
        "位置序必须判红，否则 --key 就没有存在理由: {out}"
    );

    let (code, out, err) = cli(&["compare", &a, &b, "--key", "k"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "compare: key=k rows=2/2 diffs=0\n", "{out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_reject_a_key_column_that_is_not_in_the_snapshot() {
    // 键名写错绝不能退化成位置序：那会把"没比成"说成"比过了"
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(tmp.path(), "a.json", r#"{"schema":["k"],"rows":[["1"]]}"#);
    let (code, out, err) = cli(&["compare", &a, &a, "--key", "nope"]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0031]"), "{out}");
    assert!(out.contains("nope"), "诊断要点名写错的键列: {out}");
    assert_eq!(err, "", "全部输出走 stdout");
}

#[test]
fn should_report_a_row_with_different_column_count_as_arity() {
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(
        tmp.path(),
        "a.json",
        r#"{"schema":["k","v"],"rows":[["1","x"]]}"#,
    );
    let b = snap(
        tmp.path(),
        "b.json",
        r#"{"schema":["k","v","w"],"rows":[["1","x","p"]]}"#,
    );
    let (code, out, _) = cli(&["compare", &a, &b]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(
        out,
        "DIFF arity row=1 a=2 列 b=3 列\ncompare: rows=1/1 diffs=1 逐值不一致\n"
    );
}

#[test]
fn should_exit_1_with_env_0031_when_a_snapshot_file_is_not_a_snapshot() {
    // 手写基线最容易犯的错：少一个 rows 键。指认到具体文件，不说"没有差异"
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(tmp.path(), "a.json", r#"{"schema":["k"],"rows":[["1"]]}"#);
    let b = snap(tmp.path(), "b.json", r#"{"schema":["k"]}"#);
    let (code, out, _) = cli(&["compare", &a, &b]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0031]"), "{out}");
    assert!(out.contains("b.json"), "诊断要点名不合约定的快照: {out}");
}

#[test]
fn should_compare_two_snapshots_written_by_query_on_the_same_database() {
    // 本地半边的闭环：同一条 SQL 两次快照必须绿；换一条只改常量的 SQL 必须红——
    // 两半都锁住，裁决才不是"永绿"
    let tmp = tempfile::tempdir().unwrap();
    let db = super::query::seeded_db(tmp.path());
    write(
        tmp.path(),
        "sql/count.sql",
        "SELECT COUNT(*) AS n FROM t_a;",
    );
    write(
        tmp.path(),
        "sql/plus.sql",
        "SELECT COUNT(*) + 1 AS n FROM t_a;",
    );

    let dump = |sql: &str, name: &str| {
        let out_file = tmp.path().join(name);
        let (code, out, _) = cli(&[
            "query",
            &db.to_string_lossy(),
            "--sql-file",
            &tmp.path().join("sql").join(sql).to_string_lossy(),
            "--out",
            &out_file.to_string_lossy(),
        ]);
        assert_eq!(code, EXIT_OK, "{out}");
        out_file.to_string_lossy().into_owned()
    };

    let a = dump("count.sql", "a.json");
    let b = dump("count.sql", "b.json");
    let (code, out, err) = cli(&["compare", &a, &b]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "compare: rows=1/1 diffs=0\n", "{out}");
    assert_eq!(err, "", "全部输出走 stdout");

    let c = dump("plus.sql", "c.json");
    let (code, out, _) = cli(&["compare", &a, &c]);
    assert_eq!(code, EXIT_FAIL, "同一条语句改一个常量必须红: {out}");
    assert!(out.contains("DIFF value row=1 col=n a=2 b=3"), "{out}");
}

#[test]
fn should_exit_2_when_compare_gets_other_than_two_snapshot_files() {
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(tmp.path(), "a.json", r#"{"schema":["k"],"rows":[]}"#);
    for argv in [
        vec!["compare".to_string()],
        vec!["compare".to_string(), a.clone()],
        vec!["compare".to_string(), a.clone(), a.clone(), a.clone()],
    ] {
        let (code, out, _) = cli(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(code, EXIT_USAGE, "{out}");
        assert!(out.starts_with("[ENG-0010] usage: compare"), "{out}");
    }
}

#[test]
fn should_exit_2_when_compare_gets_an_unknown_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let a = snap(tmp.path(), "a.json", r#"{"schema":["k"],"rows":[]}"#);
    let (code, out, _) = cli(&["compare", &a, &a, "--nope"]);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("unknown flag"), "{out}");
}
