//! `rules probe`（版本锚定）与 `explain`（IR 打印）。

use super::{
    cli, write, write_bytes, write_gbk_routine, EXIT_FAIL, EXIT_OK, P_LITERAL_UTF8, P_X,
    RULES_DRIFT, RULES_MATCH,
};

#[test]
fn should_pass_rules_probe_and_exit_0_when_versions_match() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "rules.toml", RULES_MATCH);
    let (code, out, _) = cli(&["rules", "probe", &f.to_string_lossy()]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains(&format!(
            "rules: {} target_duckdb=1.5.5 probe_duckdb=1.5.5 match=true",
            f.display()
        )),
        "{out}"
    );
}

#[test]
fn should_exit_1_on_rules_probe_when_versions_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "rules.toml", RULES_DRIFT);
    let (code, out, _) = cli(&["rules", "probe", &f.to_string_lossy()]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("probe_duckdb=1.4.4 match=false"), "{out}");
}

#[test]
fn should_exit_1_with_env_0012_when_rules_file_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.toml").to_string_lossy().into_owned();
    let (code, out, _) = cli(&["rules", "probe", &missing]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0012] read rules: "), "{out}");
}

#[test]
fn should_explain_routine_body_as_exact_ir_line() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p_x.sql", P_X);
    let (code, out, _) = cli(&["explain", "--file", &f.to_string_lossy(), "prc_x"]);
    assert_eq!(code, EXIT_OK, "{out}");
    // 参数由调用方预绑定，IR 只含函数体：唯一一条赋值
    assert_eq!(out, "0\tassign\tp_o := (p_i * 2)\n", "{out}");
}

#[test]
fn should_explain_gbk_literal_verbatim_as_the_same_ir_as_its_utf8_twin() {
    // D9② 的 IR 侧证据：GBK 源经 `explain` 产出的那一行与 UTF-8  twin **逐字相同**，
    // 说明还原发生在读入时、且没有把字面量送进 U+FFFD（V27.4 的旧形态是
    // `0\tassign\tp_o := '<U+FFFD>…'` + rc=0）。
    let tmp = tempfile::tempdir().unwrap();
    let gbk = write_gbk_routine(tmp.path(), "gbk.sql");
    let utf8 = write(tmp.path(), "utf8.sql", P_LITERAL_UTF8);
    let (code_gbk, out_gbk, _) = cli(&["explain", "--file", &gbk.to_string_lossy(), "prc_gbk"]);
    let (code_utf8, out_utf8, _) = cli(&["explain", "--file", &utf8.to_string_lossy(), "prc_gbk"]);
    assert_eq!(code_utf8, EXIT_OK, "{out_utf8}");
    assert_eq!(code_gbk, EXIT_OK, "{out_gbk}");
    assert_eq!(out_utf8, "0\tassign\tp_o := '中文'\n", "{out_utf8}");
    assert_eq!(out_gbk, out_utf8, "两种编码的源码须给出同一行 IR");
}

#[test]
fn should_exit_1_with_env_0030_when_explain_source_is_valid_in_neither_encoding() {
    // D9① 判红面的另一半在 D9② 之后仍然成立：两种判定都不成立才红，不退回有损解码。
    // 输入换成 `0xFF 0xFE`（非合法 UTF-8，也非合法 GBK 双字节）。
    let tmp = tempfile::tempdir().unwrap();
    let broken = write_bytes(
        tmp.path(),
        "broken.sql",
        b"CREATE OR REPLACE PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS BEGIN p_o := '\xff\xfe'; END;\n",
    );
    let (code, out, _) = cli(&["explain", "--file", &broken.to_string_lossy(), "prc_gbk"]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        out.starts_with(&format!("[ENV-0030] 读取 {}", broken.display())),
        "{out}"
    );
    assert!(out.contains("valid UTF-8"), "{out}");
    assert!(out.contains("valid GBK"), "{out}");
    assert!(!out.contains("assign"), "判红前不得产出 IR: {out}");
}

#[test]
fn should_explain_utf8_literal_verbatim() {
    // 判红的另一半：UTF-8 字面量仍逐字节打印，说明红的是**编码**，不是中文字面量本身。
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "utf8.sql", P_LITERAL_UTF8);
    let (code, out, _) = cli(&["explain", "--file", &f.to_string_lossy(), "prc_gbk"]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert_eq!(out, "0\tassign\tp_o := '中文'\n", "{out}");
}

#[test]
fn should_exit_1_with_biz_0150_when_explain_routine_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let f = write(tmp.path(), "p_x.sql", P_X);
    let (code, out, _) = cli(&["explain", "--file", &f.to_string_lossy(), "prc_missing"]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert_eq!(
        out, "[BIZ-0150] routine prc_missing not registered\n",
        "{out}"
    );
}
