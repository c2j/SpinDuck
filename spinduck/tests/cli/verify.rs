//! `verify`：项目定位失败、编码判定（D9② 后 GBK 可核验）与干净项目的核验面 + 运行态快照。

use super::{
    cli, write, write_bytes, write_gbk_package, EXIT_FAIL, EXIT_OK, PACKAGE_X, PROJECT_GBK,
    PROJECT_X,
};

#[test]
fn should_exit_1_with_env_0030_when_project_declaration_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, out, _) = cli(&["verify", &tmp.path().to_string_lossy()]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0030] 未在 "), "{out}");
    assert!(
        out.contains("spinduck.toml"),
        "诊断须指向缺失的项目声明: {out}"
    );
}

#[test]
fn should_verify_a_gbk_package_without_complaining_about_encoding() {
    // D9② 把核验面也接上同一把解码刀：GBK 包体不再 ENV-0030，而是与 UTF-8 同形通过。
    // 这条与 compile 的 `(non-utf8)` 面（design §3.5 刻意保留）一起构成"四处入口、两种行为"
    // 在 D9② 之后的新分布：执行/计划/部署/核验四面对 GBK 一律解码，体检面仍是有损 + 标记。
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_GBK);
    write_gbk_package(tmp.path(), "sql/routines/pkg_a.sql");
    let (code, out, _) = cli(&["verify", &tmp.path().to_string_lossy()]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        !out.contains("ENV-0030"),
        "GBK 已在读入时解码，核验面不得再报编码: {out}"
    );
    assert!(
        out.contains("PASS Entry pkg_a.prc_gbk"),
        "GBK 包体的入口命中须与 UTF-8 同形: {out}"
    );
}

#[test]
fn should_exit_1_with_env_0030_when_corpus_is_valid_in_neither_encoding() {
    // 核验面的判红另一半仍在（V26.4 的"verify 直接红"形态，输入换成两把判定都不成立的字节）。
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_GBK);
    write_bytes(
        tmp.path(),
        "sql/routines/g.sql",
        b"CREATE OR REPLACE PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS BEGIN p_o := '\xff\xfe'; END;\n",
    );
    let (code, out, _) = cli(&["verify", &tmp.path().to_string_lossy()]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0030] 读取 "), "{out}");
    assert!(out.contains("valid UTF-8"), "{out}");
    assert!(out.contains("valid GBK"), "{out}");
}

#[test]
fn should_exit_0_and_write_runtime_manifest_for_clean_project() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", PROJECT_X);
    write(tmp.path(), "sql/routines/pkg_a.sql", PACKAGE_X);
    let (code, out, _) = cli(&["verify", &tmp.path().to_string_lossy()]);
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.starts_with("manifest: "), "{out}");
    assert!(out.contains("(updated)"), "首次核验须落运行态快照: {out}");
    assert!(
        out.contains("PASS Entry pkg_a.prc_x"),
        "入口命中须为 PASS: {out}"
    );
    assert!(
        out.contains("verify: entries=1 checks=2 failed=0 skipped=1"),
        "{out}"
    );
}
