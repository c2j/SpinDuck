//! 项目态 compile 的诊断与编码面：项目定位失败、GBK 判定（D9②）与干净项目的快照。
//!
//! issue #38：原 `verify` 命令的 4 条契约改经 `compile --dir` / `--sync-manifest`，
//! 行为断言逐条保留（诊断码、编码判定、manifest 前缀与 (updated)）。

use super::{
    abs_file, cli, compile_project_in_process, cw_graph, write, write_bytes, write_gbk_package,
    EXIT_FAIL, EXIT_OK, PACKAGE_X, PROJECT_GBK, PROJECT_X,
};

#[test]
fn should_exit_1_with_env_0030_when_project_declaration_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, out, _) = cli(&["compile", "--dir", &tmp.path().to_string_lossy()]);
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.starts_with("[ENV-0030] 未在 "), "{out}");
    assert!(
        out.contains("spinduck.toml"),
        "诊断须指向缺失的项目声明: {out}"
    );
}

#[test]
fn should_compile_a_gbk_package_without_complaining_about_encoding() {
    // D9② 把核验面也接上同一把解码刀：GBK 包体不再 ENV-0030，而是与 UTF-8 同形通过。
    // 这条与 compile 的 `(non-utf8)` 面（design §3.5 刻意保留）一起构成"四处入口、两种行为"
    // 在 D9② 之后的新分布：执行/计划/部署/核验四面对 GBK 一律解码，体检面仍是有损 + 标记。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "spinduck.toml", PROJECT_GBK);
    write_gbk_package(root, "sql/routines/pkg_a.sql");
    let file = abs_file(&root.join("sql/routines/pkg_a.sql"));
    let (code, out) = compile_project_in_process(
        root,
        &[],
        None,
        true,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "package", "name": "pkg_a"},
                {"id": 2, "type": "procedure", "name": "prc_gbk", "file": file}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "contains_routine"}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        !out.contains("ENV-0030"),
        "GBK 已在读入时解码，核验面不得再报编码: {out}"
    );
    assert!(
        out.contains("plan: target=pkg_a.prc_gbk"),
        "GBK 包体解码后应进入项目态 compile: {out}"
    );
    assert!(
        out.contains("compile: entries=1 missing=0 failed=0 dynamic=0"),
        "{out}"
    );
    assert!(out.contains("manifest: "), "{out}");
    assert!(
        !out.contains("PASS "),
        "场景 PASS 行已不在项目态 compile 输出: {out}"
    );
    assert!(
        !out.contains("SKIP "),
        "场景 SKIP 行已不在项目态 compile 输出: {out}"
    );
}

#[test]
fn should_exit_1_with_env_0030_when_corpus_is_valid_in_neither_encoding() {
    // 核验面的判红另一半仍在（V27.4 的"verify 直接红"形态，输入换成两把判定都不成立的字节）。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "spinduck.toml", PROJECT_GBK);
    write_bytes(
        root,
        "sql/routines/g.sql",
        b"CREATE OR REPLACE PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS BEGIN p_o := '\xff\xfe'; END;\n",
    );
    let file = abs_file(&root.join("sql/routines/g.sql"));
    let (code, out) = compile_project_in_process(
        root,
        &[],
        None,
        true,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "package", "name": "pkg_a"},
                {"id": 2, "type": "procedure", "name": "prc_gbk", "file": file}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "contains_routine"}
            ]),
        ),
    );
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(
        out.contains("[ENV-0030] 读取 "),
        "编码失败仍须带读取路径前缀: {out}"
    );
    assert!(out.contains("valid UTF-8"), "{out}");
    assert!(out.contains("valid GBK"), "{out}");
}

#[test]
fn should_exit_0_and_write_runtime_manifest_for_clean_project() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "spinduck.toml", PROJECT_X);
    write(root, "sql/routines/pkg_a.sql", PACKAGE_X);
    let file = abs_file(&root.join("sql/routines/pkg_a.sql"));
    let (code, out) = compile_project_in_process(
        root,
        &[],
        None,
        true,
        cw_graph(
            serde_json::json!([
                {"id": 1, "type": "package", "name": "pkg_a"},
                {"id": 2, "type": "procedure", "name": "prc_x", "file": file}
            ]),
            serde_json::json!([
                {"source": 1, "target": 2, "type": "contains_routine"}
            ]),
        ),
    );
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.starts_with("manifest: "), "{out}");
    assert!(out.contains("(updated)"), "首次同步须落运行态快照: {out}");
    assert!(
        out.contains("plan: target=pkg_a.prc_x"),
        "入口命中须正向钉死，不能只靠旧 PASS 行消失: {out}"
    );
    assert!(
        out.contains("compile: entries=1 missing=0 failed=0 dynamic=0"),
        "{out}"
    );
    assert!(
        !out.contains("PASS "),
        "场景 PASS 行已不在项目态 compile 输出: {out}"
    );
    assert!(
        !out.contains("verify: entries="),
        "不再打印 verify 汇总行: {out}"
    );
}
