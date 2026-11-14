//! 项目态 `compile` 的真子进程契约：进程内嵌 codeweb 取图，缺图/过期不退回词法 `build`。
//!
//! 旧版用 `/bin/sh` 桩模拟 `--version`/`diff`/`export` 三次进程调用；嵌入后这些
//! 进程面已退役，改为 tempfile 真实 codeweb 项目——契约面从"argv 序列"变为
//! "图索引状态 → 退出码/消息"（docs/embeded-codeweb.md §4.6）。

use super::{cli, write, EXIT_FAIL, EXIT_OK, EXIT_USAGE};
use std::path::Path;

const PKG: &str = "CREATE OR REPLACE PACKAGE pkg_a AS\n  PROCEDURE prc_x(p_o OUT NUMBER);\nEND;\n/\n\
     CREATE OR REPLACE PACKAGE BODY pkg_a AS\n  PROCEDURE prc_x(p_o OUT NUMBER) IS\n  BEGIN\n    p_o := 1;\n  END;\nEND;\n/\n";

const CODEWEB_TOML: &str = "[project]\n\
     name = \"cw\"\n\
     \n\
     [analysis]\n\
     paths = [\"sql/\"]\n\
     \n\
     [store]\n\
     path = \".codeweb/store.bincode\"\n\
     format = \"bincode\"\n";

/// 最小 SpinDuck 项目 + 真实 codeweb 项目（同根，codeweb.toml 在 `cw/`）。
/// 返回 (dir, codeweb_root)。
fn project(root: &Path, entry: &str) -> (String, String) {
    write(
        root,
        "spinduck.toml",
        &format!(
            "[project]\nname = \"p\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"{entry}\"\n"
        ),
    );
    write(root, "sql/routines/pkg_a.sql", PKG);
    write(root, "cw/codeweb.toml", CODEWEB_TOML);
    write(root, "cw/sql/pkg_a.sql", PKG);
    (
        root.to_string_lossy().into_owned(),
        root.join("cw").to_string_lossy().into_owned(),
    )
}

fn plan_text(dir: &Path, target: &str) -> String {
    std::fs::read_to_string(dir.join(format!(".spinduck/plan/{target}.toml"))).unwrap_or_default()
}

#[test]
fn should_generate_a_plan_with_analyze_against_a_real_codeweb_project() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (dir, cw) = project(root, "pkg_a.prc_x");
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &dir,
        "--schema",
        "BIGFUND",
        "--entry",
        "pkg_a.prc_x",
        "--codeweb-project",
        &cw,
        "--analyze",
    ]);
    assert_eq!(err, "", "codeweb 的日志不得漏到 CLI: {err}");
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains("plan: target=pkg_a.prc_x routines=1 tables=0 missing=0"),
        "{out}"
    );
    assert!(
        out.contains("compile: entries=1 missing=0 failed=0 dynamic=0"),
        "{out}"
    );
    let plan = plan_text(root, "pkg_a.prc_x");
    assert!(
        plan.contains(&format!(
            "generator = \"codeweb {}\"",
            spinduck_core::plan::codeweb::embedded_version()
        )),
        "generator 必须来自链接的 codeweb 版本: {plan}"
    );
    assert!(plan.contains("schema = \"bigfund\""), "{plan}");
}

#[test]
fn should_fail_with_env_30_when_the_store_is_missing_without_analyze() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (dir, cw) = project(root, "pkg_a.prc_x");
    let (code, out, err) = cli(&["compile", "--dir", &dir, "--codeweb-project", &cw]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("codeweb 图索引缺失"), "{out}");
    assert!(!out.contains("summary:"), "{out}");
    assert!(
        plan_text(root, "pkg_a.prc_x").is_empty(),
        "缺图时不得退回词法 build 写计划"
    );
}

#[test]
fn should_fail_with_env_30_when_sources_changed_after_analyze_without_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (dir, cw) = project(root, "pkg_a.prc_x");
    // 先 --analyze 建 store
    let (code, out, _) = cli(&[
        "compile",
        "--dir",
        &dir,
        "--codeweb-project",
        &cw,
        "--analyze",
    ]);
    assert_eq!(code, EXIT_OK, "{out}");
    // 改源文件（code web 项目侧）后再以只读模式取图
    let sql = root.join("cw/sql/pkg_a.sql");
    let mut text = std::fs::read_to_string(&sql).unwrap();
    text.push_str("\n-- touch\n");
    std::fs::write(&sql, text).unwrap();

    let (code, out, err) = cli(&["compile", "--dir", &dir, "--codeweb-project", &cw]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("codeweb 图与源码不一致"), "{out}");
}

#[test]
fn should_fail_with_env_30_when_codeweb_toml_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"p\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_a.prc_x\"\n",
    );
    write(root, "sql/routines/pkg_a.sql", PKG);
    let (code, out, err) = cli(&["compile", "--dir", &root.to_string_lossy()]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("未找到 codeweb 项目"), "{out}");
}

#[test]
fn should_fail_with_env_30_when_the_entry_is_absent_from_the_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (dir, cw) = project(root, "pkg_a.prc_x");
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &dir,
        "--entry",
        "pkg_a.prc_other",
        "--codeweb-project",
        &cw,
        "--analyze",
    ]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[BIZ-0150]"), "{out}");
    assert!(
        plan_text(root, "pkg_a.prc_other").is_empty(),
        "入口不在图里不得写计划"
    );
}

#[test]
fn should_refuse_project_compile_when_the_project_has_no_entry() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "spinduck.toml", "[project]\nname = \"p\"\n");
    write(
        tmp.path(),
        "sql/routines/p.sql",
        "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;\n",
    );
    let (code, out, err) = cli(&["compile", "--dir", &tmp.path().to_string_lossy()]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("入口"), "{out}");
    assert!(!out.contains("summary:"), "{out}");
}

#[test]
fn should_exit_2_when_a_path_is_mixed_with_a_project_flag() {
    let (code, out, err) = cli(&["compile", "a.sql", "--dir", "proj"]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_USAGE, "{out}");
    assert!(out.contains("[ENG-0010]"), "{out}");
    assert!(out.contains("项目旗标"), "{out}");
}
