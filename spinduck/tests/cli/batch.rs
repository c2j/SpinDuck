//! `batch` 子命令契约（设计 docs/spinduck-batch-sdsh-design.md §5.2）：
//! 真子进程跑 `spinduck batch <script.sdsh>`，锁退出码 + stdout + 副作用面。
//!
//! 覆盖：基本编排、`use`/`--db` 注入、`:memory:` 显式共享 vs 匿名隔离、
//! `--dry-run` 零副作用、`--continue-on-error` 汇总、`str:` 前导零保真、
//! `ENV-0017` 未定义变量、嵌套 batch/help 拒绝、shell 算符拒绝。

use super::{cli, EXIT_FAIL, EXIT_OK};
use std::path::Path;

const P_DOUBLE: &str = "CREATE OR REPLACE PROCEDURE prc_double(p_i IN NUMBER, p_o OUT NUMBER) IS BEGIN p_o := p_i * 2; END;";

fn write(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn should_run_ordered_call_steps_in_one_process() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p.sql", P_DOUBLE);
    let script = write(
        tmp.path(),
        "run.sdsh",
        &format!("call --file {f} prc_double 21 null\ncall --file {f} prc_double 1 null\n"),
    );
    let (code, out, err) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_OK, "{out}{err}");
    assert_eq!(out.matches("OUT p_o = 42").count(), 1, "{out}");
    assert!(out.contains("OUT p_o = 2"), "{out}");
    assert!(err.is_empty(), "{err}");
}

#[test]
fn should_share_explicit_memory_db_across_steps() {
    // use :memory: → 显式共享内存库：query 建表后 call 可见（同一 instance）。
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p.sql", P_DOUBLE);
    // query 每步只跑一条语句：第一步建表，第二步读它——跨步可见即证明共享库。
    let ddl = write(tmp.path(), "ddl.sql", "CREATE TABLE t AS SELECT 5 AS a;");
    let sel = write(tmp.path(), "sel.sql", "SELECT a FROM t");
    let snap1 = tmp.path().join("s1.json");
    let snap2 = tmp.path().join("s2.json");
    let script = format!(
        "use :memory:\nquery --sql-file {ddl} --out {s1}\nquery --sql-file {sel} --out {s2}\ncall --file {f} prc_double 3 null\n",
        ddl = ddl,
        sel = sel,
        s1 = snap1.to_string_lossy(),
        s2 = snap2.to_string_lossy(),
        f = f
    );
    let script = write(tmp.path(), "run.sdsh", &script);
    let (code, out, err) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_OK, "{out}{err}");
    assert!(snap1.is_file(), "query --out 未产出");
    assert!(snap2.is_file(), "第二步读不到第一步建的表: {out}");
    assert!(out.contains("OUT p_o = 6"), "{out}");
}

#[test]
fn should_isolate_anonymous_memory_calls_without_default_db() {
    // call 无 --db 且无默认库 → 每步独立 :memory:（A4 语义保持）。
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p.sql", P_DOUBLE);
    let script = write(
        tmp.path(),
        "run.sdsh",
        &format!("call --file {f} prc_double 2 null\ncall --file {f} prc_double 3 null\n"),
    );
    let (code, out, _) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.contains("OUT p_o = 4"), "{out}");
    assert!(out.contains("OUT p_o = 6"), "{out}");
}

#[test]
fn should_inject_use_db_into_db_commands() {
    // use 设默认库后，deploy/query 等必填 --db 指令省略旗标仍能跑通。
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("w.duckdb");
    let dbs = db.to_string_lossy().to_string();
    let q = write(tmp.path(), "q.sql", "SELECT 1 AS one");
    let snap = tmp.path().join("s.json");
    let script = write(
        tmp.path(),
        "run.sdsh",
        &format!(
            "use {dbs}\nquery --sql-file {q} --out {sn}\n",
            q = q,
            sn = snap.to_string_lossy()
        ),
    );
    let (code, out, err) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_OK, "{out}{err}");
    assert!(db.is_file(), "默认库未被创建");
    assert!(snap.is_file());
}

#[test]
fn should_dry_run_with_zero_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("w.duckdb");
    let script = write(
        tmp.path(),
        "run.sdsh",
        &format!("use {}\ncall --file x.sql prc 1\n", db.to_string_lossy()),
    );
    let (code, out, _) = cli(&["batch", &script, "--dry-run"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.contains("dry-run"), "{out}");
    assert!(!db.exists(), "dry-run 不得创建库文件");
}

#[test]
fn should_continue_on_error_and_report_aggregate_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let good = write(tmp.path(), "g.sql", P_DOUBLE);
    let script = write(
        tmp.path(),
        "run.sdsh",
        &format!(
            "call --file {g} prc_double 1 null\ncall --file /nonexistent.sql prc 1\ncall --file {g} prc_double 2 null\n",
            g = good
        ),
    );
    // 无 --continue-on-error：第二步失败即停（第三步不执行）
    let (code, out, _) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        !out.contains("OUT p_o = 4"),
        "fail-fast 不应执行第三步: {out}"
    );
    // 有 --continue-on-error：第三步照跑，批级仍 1
    let (code, out, _) = cli(&["batch", &script, "--continue-on-error"]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("OUT p_o = 4"), "{out}");
}

#[test]
fn should_preserve_leading_zero_via_str_sugar() {
    let tmp = tempfile::tempdir().unwrap();
    // p_o 回显入参原文：str: 保真前导零（parse_literal 数字路径会丢）
    let f = write(
        tmp.path(),
        "p.sql",
        "CREATE OR REPLACE PROCEDURE prc_id(p_i IN VARCHAR2, p_o OUT VARCHAR2) IS BEGIN p_o := p_i; END;",
    );
    let script = write(
        tmp.path(),
        "run.sdsh",
        &format!("call --file {f} prc_id str:0130000001 x\n"),
    );
    let (code, out, _) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.contains("OUT p_o = 0130000001"), "{out}");
}

#[test]
fn should_reject_undefined_variable_env_0017() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write(
        tmp.path(),
        "run.sdsh",
        "query --db \"$SD_SHOULD_NOT_EXIST\" --sql-file q --out o\n",
    );
    let (code, out, _) = cli(&["batch", &script]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("ENV-0017"), "{out}");
}

#[test]
fn should_reject_nested_batch_help_and_shell_ops() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in [
        "batch x.sdsh\n",
        "help\n",
        "call --file f prc 1 ; ls\n",
        "call --file f prc 1 | cat\n",
    ] {
        let s = write(tmp.path(), "bad.sdsh", bad);
        let (code, out, _) = cli(&["batch", &s]);
        assert_eq!(code, EXIT_FAIL, "{bad}: {out}");
    }
}
