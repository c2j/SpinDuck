//! 项目态 `compile` 的真子进程契约：一次 codeweb argv 序列、缺二进制不退回词法 `build`。
//!
//! 桩是 `/bin/sh`。Windows 开发机不编译这组测试；二进制名本身由 `path_bin_names` 覆盖。

#![cfg(unix)]

use super::{cli, write, EXIT_FAIL, EXIT_OK, EXIT_USAGE};
use std::path::{Path, PathBuf};

const PKG: &str = "CREATE OR REPLACE PACKAGE pkg_a AS\n  PROCEDURE prc_x(p_o OUT NUMBER);\nEND;\n/\n\
     CREATE OR REPLACE PACKAGE BODY pkg_a AS\n  PROCEDURE prc_x(p_o OUT NUMBER) IS\n  BEGIN\n    p_o := 1;\n  END;\nEND;\n/\n";

const STUB: &str = r#"#!/bin/sh
here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
{
  for arg in "$@"; do
    printf '%s\n' "$arg"
  done
  printf '\n'
} >> "$here/argv.log"
case "${1:-}" in
  --version) cat "$here/version.txt" ;;
  diff) cat "$here/diff.stderr" >&2 ;;
  export)
    if [ -f "$here/export.exit" ]; then
      cat "$here/export.stderr" >&2
      exit "$(cat "$here/export.exit")"
    fi
    cat "$here/export.json"
    ;;
  *) printf '%s\n' "unexpected: ${1:-}" >&2; exit 9 ;;
esac
"#;

fn stub(dir: &Path, version: &str, diff_stderr: &str) -> PathBuf {
    let bin = dir.join("codeweb");
    std::fs::write(&bin, STUB).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(dir.join("version.txt"), version).unwrap();
    std::fs::write(dir.join("diff.stderr"), diff_stderr).unwrap();
    bin
}

fn project_sql(dir: &Path, entry: &str) -> String {
    write(
        dir,
        "spinduck.toml",
        &format!(
            "[project]\nname = \"p\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"{entry}\"\n"
        ),
    );
    write(dir, "codeweb.toml", "");
    let sql = write(dir, "sql/routines/pkg_a.sql", PKG);
    sql.to_string_lossy().replace('\\', "/")
}

fn graph(file: &str) -> String {
    serde_json::json!({
        "nodes": [{
            "id": 1,
            "type": "procedure",
            "name": "prc_x",
            "schema": "pkg_a",
            "file": file,
            "line": 1
        }],
        "edges": []
    })
    .to_string()
}

fn plan_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(".spinduck/plan/pkg_a.prc_x.toml")).unwrap_or_default()
}

#[test]
fn should_export_codeweb_once_and_keep_its_stderr_off_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let file = project_sql(root, "pkg_a.prc_other");
    let bin = stub(
        root,
        "codeweb 0.9.7\n",
        "Up to date. No changes detected.\n",
    );
    std::fs::write(root.join("export.json"), graph(&file)).unwrap();
    let dir = root.to_string_lossy().into_owned();
    let bin_s = bin.to_string_lossy().into_owned();
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &dir,
        "--schema",
        "BIGFUND",
        "--entry",
        "pkg_a.prc_x",
        "--codeweb-bin",
        &bin_s,
    ]);
    assert_eq!(err, "", "codeweb 的 stderr 不得漏到 CLI: {err}");
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains("plan: target=pkg_a.prc_x routines=1 tables=0 missing=0"),
        "{out}"
    );
    assert!(
        out.contains("compile: entries=1 missing=0 failed=0 dynamic=0"),
        "{out}"
    );
    let plan = plan_text(root);
    assert!(plan.contains("generator = \"codeweb 0.9.7\""), "{plan}");
    assert!(plan.contains("schema = \"bigfund\""), "{plan}");
    assert!(!root.join(".spinduck/plan/pkg_a.prc_other.toml").exists());
    let log = std::fs::read_to_string(root.join("argv.log")).unwrap();
    let expected = format!(
        "--version\n\ndiff\n--project\n{dir}\n\nexport\n--format\njson\n--project\n{dir}\n\n"
    );
    assert_eq!(log, expected);
    assert!(!log.contains("--filter") && !log.contains("-o"), "{log}");
}

#[test]
fn should_reject_a_codeweb_version_that_is_not_0_9_7() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _file = project_sql(root, "pkg_a.prc_x");
    let bin = stub(
        root,
        "codeweb 0.9.6\n",
        "Up to date. No changes detected.\n",
    );
    std::fs::write(root.join("export.json"), "{}\n").unwrap();
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &root.to_string_lossy(),
        "--codeweb-bin",
        &bin.to_string_lossy(),
    ]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("codeweb 版本不是 0.9.7:"), "{out}");
    assert!(!std::fs::read_to_string(root.join("argv.log"))
        .unwrap()
        .contains("diff"));
    assert!(plan_text(root).is_empty());
}

#[test]
fn should_reject_diff_stderr_that_is_not_the_sentinel() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _file = project_sql(root, "pkg_a.prc_x");
    let bin = stub(
        root,
        "codeweb 0.9.7\n",
        "1 modified, 0 added, 0 deleted, 1 unchanged\n",
    );
    std::fs::write(root.join("export.json"), "{}\n").unwrap();
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &root.to_string_lossy(),
        "--codeweb-bin",
        &bin.to_string_lossy(),
    ]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("codeweb 图与源码不一致:"), "{out}");
    let log = std::fs::read_to_string(root.join("argv.log")).unwrap();
    assert!(log.contains("diff\n"), "{log}");
    assert!(!log.contains("export\n"), "{log}");
    assert!(plan_text(root).is_empty());
}

#[test]
fn should_reject_export_nonzero_without_writing_a_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _file = project_sql(root, "pkg_a.prc_x");
    let bin = stub(
        root,
        "codeweb 0.9.7\n",
        "Up to date. No changes detected.\n",
    );
    std::fs::write(root.join("export.exit"), "3\n").unwrap();
    std::fs::write(root.join("export.stderr"), "export broke\n").unwrap();
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &root.to_string_lossy(),
        "--codeweb-bin",
        &bin.to_string_lossy(),
    ]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("codeweb export 失败:"), "{out}");
    assert!(out.contains("exit 3"), "{out}");
    let log = std::fs::read_to_string(root.join("argv.log")).unwrap();
    assert!(log.contains("export\n"), "{log}");
    assert!(plan_text(root).is_empty(), "{out}");
}

#[test]
fn should_reject_export_json_that_does_not_parse() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _file = project_sql(root, "pkg_a.prc_x");
    let bin = stub(
        root,
        "codeweb 0.9.7\n",
        "Up to date. No changes detected.\n",
    );
    std::fs::write(root.join("export.json"), "{\n").unwrap();
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &root.to_string_lossy(),
        "--codeweb-bin",
        &bin.to_string_lossy(),
    ]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("codeweb JSON 解析失败:"), "{out}");
    assert!(plan_text(root).is_empty(), "{out}");
}

#[test]
fn should_not_call_lexical_build_when_the_codeweb_binary_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _file = project_sql(root, "pkg_a.prc_x");
    let missing = root.join("no-such-codeweb");
    let (code, out, err) = cli(&[
        "compile",
        "--dir",
        &root.to_string_lossy(),
        "--codeweb-bin",
        &missing.to_string_lossy(),
    ]);
    assert_eq!(err, "");
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(out.contains("codeweb 二进制不可用:"), "{out}");
    assert!(!out.contains("summary:"), "{out}");
    assert!(plan_text(root).is_empty(), "词法 build 会写下这份计划");
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
