//! `materialize` / `sync` 的入口门禁：发版 feature 与连接参数各锁一条契约。
//!
//! 两条路都必须**在打开本地库文件与连接实例之前**失败，所以每个用例顺带断言
//! 本地 DuckDB 文件没被创建——这是 design §1「release 二进制能不能连真实 GaussDB」
//! 那条结论在 CLI 层的可复现形式（R1 闭合时该期望值随行为一起评审）。

use std::path::Path;

use super::{cli, EXIT_FAIL, EXIT_USAGE};

/// `--db` 指向临时目录里的一个**不该被创建**的文件。
fn contract_db(dir: &Path) -> String {
    dir.join("contract.duck").to_string_lossy().into_owned()
}

fn materialize_argv(db: &str) -> Vec<String> {
    vec![
        "materialize".to_string(),
        "--db".to_string(),
        db.to_string(),
        "--table".to_string(),
        "t1".to_string(),
    ]
}

fn sync_argv(db: &str) -> Vec<String> {
    vec![
        "sync".to_string(),
        "--db".to_string(),
        db.to_string(),
        "--table".to_string(),
        "t1".to_string(),
        "--cols".to_string(),
        "a,b".to_string(),
    ]
}

fn run(items: &[String]) -> (i32, String, String) {
    let args: Vec<&str> = items.iter().map(String::as_str).collect();
    cli(&args)
}

#[cfg(not(feature = "gauss"))]
#[test]
fn should_exit_1_with_env_0012_when_materialize_built_without_gauss() {
    let tmp = tempfile::tempdir().unwrap();
    let db = contract_db(tmp.path());
    let (code, out, _) = run(&materialize_argv(&db));
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0012] materialize t1 -> "), "{out}");
    assert!(out.contains("requires feature `gauss`"), "{out}");
    assert!(
        !tmp.path().join("contract.duck").exists(),
        "被 feature 门禁挡住时不该打开本地库"
    );
}

#[cfg(not(feature = "gauss"))]
#[test]
fn should_exit_1_with_env_0012_when_multi_table_built_without_gauss() {
    let tmp = tempfile::tempdir().unwrap();
    let db = contract_db(tmp.path());
    let argv: Vec<String> = ["materialize", "--db", &db, "--table", "t1", "--table", "t2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, out, _) = run(&argv);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0012] materialize "), "{out}");
    assert!(out.contains("requires feature `gauss`"), "{out}");
    assert!(
        !tmp.path().join("contract.duck").exists(),
        "被 feature 门禁挡住时不该打开本地库"
    );
}

#[test]
fn should_reject_duplicate_table_names_as_usage_error_before_touching_db() {
    // #18 契约守护（评审：落 CLI 契约层，独立文件不会被同类合并覆盖）：
    // 重复表名 = 手误，parse 层判用法错（exit 2），不建库。
    // 大小写等价与引擎 lower(table_name) 同口径（T1/t1 是同一张表）。
    let cases: [&[&str]; 3] = [
        &["--table", "t1", "--table", "t1"],
        &["--table", "t1,t1"],
        &["--table", "T1", "--table", "t1"],
    ];
    for flags in cases {
        let tmp = tempfile::tempdir().unwrap();
        let db = contract_db(tmp.path());
        let mut argv: Vec<String> = vec!["materialize".to_string(), "--db".to_string(), db];
        argv.extend(flags.iter().map(|s| s.to_string()));
        let (code, out, _) = run(&argv);
        assert_eq!(code, EXIT_USAGE, "{flags:?}: {out}");
        assert!(out.contains("重复表名"), "{flags:?}: {out}");
        assert!(
            !tmp.path().join("contract.duck").exists(),
            "{flags:?}: 用法错不该打开本地库"
        );
    }
}

#[test]
fn should_reject_where_with_multi_table_before_touching_db() {
    // 语义歧义在 parse 层判用法错（feature 无关，exit 2）：不连源、不建库
    let tmp = tempfile::tempdir().unwrap();
    let db = contract_db(tmp.path());
    let argv: Vec<String> = [
        "materialize",
        "--db",
        &db,
        "--table",
        "t1",
        "--table",
        "t2",
        "--where",
        "bcrq='20240105'",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (code, out, _) = run(&argv);
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("--where 仅限单表形态"), "{out}");
    assert!(
        !tmp.path().join("contract.duck").exists(),
        "用法错不该打开本地库"
    );
}

#[cfg(not(feature = "gauss"))]
#[test]
fn should_exit_1_with_env_0012_when_sync_built_without_gauss() {
    let tmp = tempfile::tempdir().unwrap();
    let db = contract_db(tmp.path());
    let (code, out, _) = run(&sync_argv(&db));
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0012] sync t1 (a,b) from "), "{out}");
    assert!(out.contains("requires feature `gauss`"), "{out}");
    assert!(
        !tmp.path().join("contract.duck").exists(),
        "被 feature 门禁挡住时不该打开本地库"
    );
}

#[cfg(feature = "gauss")]
#[test]
fn should_exit_1_with_env_0014_when_materialize_has_no_instance_env() {
    // 打开 gauss 后契约换成"缺连接参数即红"。本用例剥掉了所有 `SPINDUCK_*` 变量，
    // 因此它同时证明：零外部环境下不会去连任何实例。
    let tmp = tempfile::tempdir().unwrap();
    let db = contract_db(tmp.path());
    let (code, out, _) = run(&materialize_argv(&db));
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(out, "[ENV-0014] SPINDUCK_OG_HOST is required\n", "{out}");
    assert!(
        !tmp.path().join("contract.duck").exists(),
        "缺连接参数即红时不该打开本地库"
    );
}

#[cfg(feature = "gauss")]
#[test]
fn should_exit_1_with_env_0014_when_sync_has_no_instance_env() {
    let tmp = tempfile::tempdir().unwrap();
    let db = contract_db(tmp.path());
    let (code, out, _) = run(&sync_argv(&db));
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(out, "[ENV-0014] SPINDUCK_OG_HOST is required\n", "{out}");
    assert!(
        !tmp.path().join("contract.duck").exists(),
        "缺连接参数即红时不该打开本地库"
    );
}
