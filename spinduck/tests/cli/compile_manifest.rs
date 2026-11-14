//! `compile --sync-manifest`：运行态快照同步契约（进程内假导出，零 codeweb 依赖）。
//!
//! issue #38 决策 1：快照名 `conf/spinduck.runtime.toml`；旗标默认关，开启时
//! `manifest:` 行先于 `plan:`；快照覆盖全部 `[[entry]]`（与 `--entry` 子集无关）；
//! 运行态目录不回写；写新名成功后删除旧名 `conf/spinduck.toml` 并打一行说明。

use super::{
    abs_file, compile_project_in_process, cw_graph, write, EXIT_FAIL, EXIT_OK, PACKAGE_X, PROJECT_X,
};

fn project(root: &std::path::Path) -> String {
    write(root, "spinduck.toml", PROJECT_X);
    write(root, "sql/routines/pkg_a.sql", PACKAGE_X);
    abs_file(&root.join("sql/routines/pkg_a.sql"))
}

fn graph(file: &str) -> serde_json::Value {
    cw_graph(
        serde_json::json!([
            {"id": 1, "type": "package", "name": "pkg_a"},
            {"id": 2, "type": "procedure", "name": "prc_x", "file": file}
        ]),
        serde_json::json!([
            {"source": 1, "target": 2, "type": "contains_routine"}
        ]),
    )
}

#[test]
fn should_print_manifest_line_before_plan_when_flag_given() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let file = project(root);
    let (code, out) = compile_project_in_process(root, &[], None, true, graph(&file));
    assert_eq!(code, EXIT_OK, "{out}");
    let snap = root.join(".spinduck/conf/spinduck.runtime.toml");
    assert!(
        out.starts_with(&format!("manifest: {} (updated)", snap.display())),
        "首行须是新名快照路径 + updated：{out}"
    );
    assert!(out.contains("plan: target=pkg_a.prc_x"), "{out}");
    assert!(
        out.contains("compile: entries=1 missing=0 failed=0 dynamic=0"),
        "{out}"
    );
    assert!(snap.is_file(), "快照须落到新名路径");
}

#[test]
fn should_not_write_manifest_without_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let file = project(root);
    let (code, out) = compile_project_in_process(root, &[], None, false, graph(&file));
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        !out.contains("manifest: "),
        "默认关闭不得打 manifest 行：{out}"
    );
    assert!(
        !root.join(".spinduck/conf").exists(),
        "默认关闭不得落快照（#33 的 stdout 契约不变）"
    );
}

#[test]
fn should_report_unchanged_on_second_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let file = project(root);
    let (code1, out1) = compile_project_in_process(root, &[], None, true, graph(&file));
    assert_eq!(code1, EXIT_OK, "{out1}");
    assert!(out1.contains("(updated)"), "{out1}");
    let (code2, out2) = compile_project_in_process(root, &[], None, true, graph(&file));
    assert_eq!(code2, EXIT_OK, "{out2}");
    assert!(
        out2.contains("(unchanged)"),
        "同内容重复同步须报 unchanged：{out2}"
    );
    assert!(!out2.contains("(updated)"), "{out2}");
}

#[test]
fn should_not_rewrite_snapshot_when_run_from_runtime_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let file = project(root);
    let (_, first) = compile_project_in_process(root, &[], None, true, graph(&file));
    assert!(first.contains("(updated)"), "{first}");
    let snap = root.join(".spinduck/conf/spinduck.runtime.toml");
    let before = std::fs::read_to_string(&snap).unwrap();

    let home = root.join(".spinduck");
    let (code2, second) = compile_project_in_process(&home, &[], None, true, graph(&file));
    assert_eq!(code2, EXIT_OK, "运行态目录须复现同一份干净结论：{second}");
    assert!(
        second.contains("(未改写：本目录即运行态快照)"),
        "快照不得被运行态反向覆盖：{second}"
    );
    assert_eq!(
        std::fs::read_to_string(&snap).unwrap(),
        before,
        "两次同步后快照内容一字不变"
    );
}

#[test]
fn should_snapshot_all_entries_even_with_entry_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"v\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_a.prc_x\"\n\n[[entry]]\nroutine = \"pkg_a.prc_y\"\n",
    );
    write(root, "sql/routines/pkg_a.sql", PACKAGE_X);
    let file = abs_file(&root.join("sql/routines/pkg_a.sql"));
    let (code, out) = compile_project_in_process(root, &["pkg_a.prc_x"], None, true, graph(&file));
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(
        out.contains("compile: entries=1"),
        "compile 只跑 --entry 子集：{out}"
    );
    let snap = std::fs::read_to_string(root.join(".spinduck/conf/spinduck.runtime.toml")).unwrap();
    assert!(
        snap.contains("pkg_a.prc_x") && snap.contains("pkg_a.prc_y"),
        "快照覆盖全部 [[entry]]，与 --entry 子集无关：{snap}"
    );
}

#[test]
fn should_remove_legacy_snapshot_and_report_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let file = project(root);
    let legacy = root.join(".spinduck/conf/spinduck.toml");
    write(
        root,
        ".spinduck/conf/spinduck.toml",
        "# 旧名快照（生成物）\n",
    );
    let (code, out) = compile_project_in_process(root, &[], None, true, graph(&file));
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(!legacy.exists(), "写新名成功后旧名快照必须删除");
    assert!(out.contains("已删除旧名快照"), "删除须打一行说明：{out}");
    assert!(root.join(".spinduck/conf/spinduck.runtime.toml").is_file());
}

#[test]
fn should_refuse_sync_when_project_has_no_entry_declarations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"v\"\nkernel = \"1.5.5\"\n",
    );
    write(root, "sql/routines/pkg_a.sql", PACKAGE_X);
    let file = abs_file(&root.join("sql/routines/pkg_a.sql"));
    let (code, out) = compile_project_in_process(root, &["pkg_a.prc_x"], None, true, graph(&file));
    assert_eq!(code, EXIT_FAIL, "{out}");
    assert!(out.contains("[ENV-0030]"), "{out}");
    assert!(
        !root.join(".spinduck/conf").exists(),
        "没有 [[entry]] 声明不得落快照：运行态不能声称一个没被点名的可调用面"
    );
}
