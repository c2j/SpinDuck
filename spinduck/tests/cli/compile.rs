//! `compile`：语料编译体检的逐文件行、summary 计数与失败直方图。

use super::{cli, write, write_gbk_routine, EXIT_FAIL, EXIT_OK, P_CLEAN_A, P_CLEAN_B, P_FORALL};

#[test]
fn should_compile_clean_corpus_and_report_zero_failures() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "clean.sql", P_CLEAN_A);
    write(tmp.path(), "clean2.sql", P_CLEAN_B);
    let dir = tmp.path().to_string_lossy().into_owned();
    let (code, out, _) = cli(&["compile", &dir]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains(&format!("{dir}/clean.sql  routines=1")),
        "{out}"
    );
    assert!(
        out.contains("summary: files=2 ok=2 failed=0 routines=2"),
        "{out}"
    );
    assert!(!out.contains("reason "), "无失败时不该输出直方图: {out}");
}

#[test]
fn should_exit_1_with_fail_line_and_reason_histogram() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "clean.sql", P_CLEAN_A);
    write(tmp.path(), "forall.sql", P_FORALL);
    let dir = tmp.path().to_string_lossy().into_owned();
    let (code, out, _) = cli(&["compile", &dir]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        out.contains(&format!("FAIL {dir}/forall.sql  [ENG-0022]")),
        "{out}"
    );
    assert!(
        out.contains("summary: files=2 ok=1 failed=1 routines=1"),
        "{out}"
    );
    assert!(out.contains("reason ENG-0022 x1"), "{out}");
}

#[test]
fn should_exit_1_with_env_0010_when_corpus_path_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.sql").to_string_lossy().into_owned();
    let (code, out, _) = cli(&["compile", &missing]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        out.starts_with(&format!("[ENV-0010] read {missing}: ")),
        "{out}"
    );
}

#[test]
fn should_mark_non_utf8_corpus_in_compile_report() {
    let tmp = tempfile::tempdir().unwrap();
    write_gbk_routine(tmp.path(), "gbk.sql");
    let dir = tmp.path().to_string_lossy().into_owned();
    let (code, out, _) = cli(&["compile", &dir]);
    // 编码契约的第一种行为（V27.4）：有损解码 + 显式 (non-utf8) 标记，仍算 OK
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains(&format!("{dir}/gbk.sql  routines=1  (non-utf8)")),
        "{out}"
    );
}
