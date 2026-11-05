//! `verify` 链路核验（真实内核，`--features native`）：入口声明 → 语料部署 → 逐对象核验。
//!
//! 用一个临时项目锁定核验面：入口命中/未命中、链路例程断点、链路表缺席、
//! 场景可执行、无场景入口显式 Skip、运行态快照落盘与再核验。
//! 全部资产来自项目声明（零硬编码路径，§12.1）。

#![cfg(feature = "native")]

use spinduck_core::project::ProjectConfig;
use spinduck_core::verify::{verify_project, CheckKind, EntryReport, Status};
use std::path::Path;

const CONFIG: &str = r#"
[project]
name = "verify_chain"
kernel = "1.5.5"

[[entry]]
routine = "pkg_a.prc_clean"

[[entry]]
routine = "pkg_a.prc_gap"

[[entry]]
routine = "pkg_a.prc_typo"
"#;

const DDL: &str = "-- 表清单\nCREATE TABLE t_ok (a INTEGER);\n";

const ROUTINES: &str = r#"
CREATE OR REPLACE PACKAGE pkg_a AS
  PROCEDURE prc_clean(p_dt IN VARCHAR2, p_n OUT NUMBER);
  PROCEDURE prc_gap(p_dt IN VARCHAR2);
END;
/
CREATE OR REPLACE PACKAGE BODY pkg_a AS
  PROCEDURE prc_clean(p_dt IN VARCHAR2, p_n OUT NUMBER) IS
  BEGIN
    SELECT count(*) INTO p_n FROM t_ok WHERE a > 0;
  END;

  PROCEDURE prc_gap(p_dt IN VARCHAR2) IS
  BEGIN
    INSERT INTO t_missing (a) VALUES (1);
    pkg_a.prc_gone();
  END;
END;
/
"#;

const CASE: &str = r#"
[case]
name = "clean"
kind = "procedure"
entry = "pkg_a.prc_clean"
baseline = "expected.json"
[case.output]
sql = "SELECT a FROM t_ok ORDER BY a"
[[case.scenarios]]
why = "种子两行计数"
args = ["'20240105'", "0"]
"#;

const SEED: &str = "INSERT INTO t_ok (a) VALUES (1);\nINSERT INTO t_ok (a) VALUES (2);\n";

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn fixture(root: &Path) -> ProjectConfig {
    write(root, "spinduck.toml", CONFIG);
    write(root, "sql/ddl/tables.sql", DDL);
    write(root, "sql/routines/pkg_a.sql", ROUTINES);
    write(root, "cases/x.case.toml", CASE);
    write(root, "cases/datasets/seed.sql", SEED);
    ProjectConfig::load(root).unwrap()
}

fn report<'a>(reports: &'a [EntryReport], entry: &str) -> &'a EntryReport {
    reports
        .iter()
        .find(|r| r.entry == entry)
        .unwrap_or_else(|| {
            panic!(
                "报告缺少入口 {entry}：{:?}",
                reports.iter().map(|r| r.entry.clone()).collect::<Vec<_>>()
            )
        })
}

fn status(r: &EntryReport, kind: CheckKind, object: &str) -> Status {
    r.checks
        .iter()
        .find(|c| c.kind == kind && c.object == object)
        .unwrap_or_else(|| {
            panic!(
                "缺核验项 {kind:?}/{object}：{:?}",
                r.checks
                    .iter()
                    .map(|c| (c.kind, c.object.clone(), c.status))
                    .collect::<Vec<_>>()
            )
        })
        .status
}

#[test]
fn should_pass_clean_entry_with_scenario_and_flag_chain_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let project = fixture(tmp.path());
    let reports = verify_project(&project).unwrap_or_else(|e| panic!("verify: {}", e.message));

    let clean = report(&reports, "pkg_a.prc_clean");
    assert_eq!(
        status(clean, CheckKind::Entry, "pkg_a.prc_clean"),
        Status::Pass
    );
    assert_eq!(
        status(clean, CheckKind::Relation, "t_ok"),
        Status::Pass,
        "链路表已部署"
    );
    assert_eq!(
        status(clean, CheckKind::Scenario, "clean#1"),
        Status::Pass,
        "场景（含数据集种子）应可执行"
    );
    assert_eq!(
        clean.failed().len(),
        0,
        "干净链路不应有失败项：{:?}",
        clean.checks
    );

    let gap = report(&reports, "pkg_a.prc_gap");
    assert_eq!(
        status(gap, CheckKind::Relation, "t_missing"),
        Status::Fail,
        "链路表缺席必须显式判失败"
    );
    assert_eq!(
        status(gap, CheckKind::Routine, "pkg_a.prc_gone"),
        Status::Fail,
        "链路例程未登记 = 断点"
    );
    assert_eq!(
        status(gap, CheckKind::Scenario, "-"),
        Status::Skip,
        "无声明该入口的用例：显式 Skip，不伪装成通过"
    );

    let typo = report(&reports, "pkg_a.prc_typo");
    assert_eq!(
        status(typo, CheckKind::Entry, "pkg_a.prc_typo"),
        Status::Fail,
        "声明的入口在语料中不存在"
    );
    assert_eq!(typo.checks.len(), 1, "入口未命中时不展开链路");
}

#[test]
fn should_render_reports_and_exit_1_when_chain_has_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let mut buf: Vec<u8> = Vec::new();
    let code = spinduck_core::cli::run::run(
        &[
            "verify".to_string(),
            tmp.path().to_string_lossy().to_string(),
        ],
        &mut buf,
    );
    let text = String::from_utf8(buf).unwrap();
    assert_eq!(code, 1, "有链路缺口必须非零退出：{text}");
    assert!(text.contains("== entry pkg_a.prc_clean"), "{text}");
    assert!(text.contains("FAIL Relation t_missing"), "{text}");
    assert!(text.contains("PASS Scenario clean#1"), "{text}");
    assert!(
        text.contains("verify: entries=3 checks=") && text.contains("failed=3"),
        "汇总行需给出条目数与失败数：{text}"
    );
}

#[test]
fn should_exit_0_when_every_entry_is_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"clean\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_a.prc_clean\"\n",
    );
    write(root, "sql/ddl/tables.sql", DDL);
    let body = ROUTINES.replace("  PROCEDURE prc_gap(p_dt IN VARCHAR2) IS\n  BEGIN\n    INSERT INTO t_missing (a) VALUES (1);\n    pkg_a.prc_gone();\n  END;\n", "");
    write(root, "sql/routines/pkg_a.sql", &body);
    write(root, "cases/x.case.toml", CASE);
    write(root, "cases/datasets/seed.sql", SEED);
    let mut buf: Vec<u8> = Vec::new();
    let code = spinduck_core::cli::run::run(
        &["verify".to_string(), root.to_string_lossy().to_string()],
        &mut buf,
    );
    let text = String::from_utf8(buf).unwrap();
    assert_eq!(code, 0, "干净链路应零退出：{text}");
    assert!(text.contains("failed=0"), "{text}");
}

#[test]
fn should_reject_project_without_entry_declarations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"no_entry\"\nkernel = \"1.5.5\"\n",
    );
    let project = ProjectConfig::load(root).unwrap();
    let e = verify_project(&project).unwrap_err();
    assert_eq!(e.domain, spinduck_core::FaultDomain::Env);
    assert!(
        e.message.contains("[[entry]]"),
        "诊断需指向入口声明：{}",
        e.message
    );
}

#[test]
fn should_not_write_manifest_when_verify_fails_before_reporting() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"no_entry\"\nkernel = \"1.5.5\"\n",
    );
    let mut buf: Vec<u8> = Vec::new();
    let code = spinduck_core::cli::run::run(
        &["verify".to_string(), root.to_string_lossy().to_string()],
        &mut buf,
    );
    let text = String::from_utf8(buf).unwrap();
    assert_eq!(code, 1, "缺入口声明必须非零退出：{text}");
    assert!(
        !root.join(".spinduck/conf/spinduck.toml").exists(),
        "核验未跑通就落快照＝运行态声称了一个没被证明的可调用面"
    );
}

#[test]
fn should_snapshot_manifest_in_home_conf_and_verify_runtime_dir_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fixture(root);
    let home = root.join(".spinduck");
    let snap = home.join("conf/spinduck.toml");

    let mut buf: Vec<u8> = Vec::new();
    spinduck_core::cli::run::run(
        &["verify".to_string(), root.to_string_lossy().to_string()],
        &mut buf,
    );
    let first = String::from_utf8(buf).unwrap();
    assert!(
        first.contains(&format!("manifest: {}", snap.display())) && first.contains("(updated)"),
        "研发态 verify 必须落运行态快照并报告路径与变化：{first}"
    );

    let text =
        std::fs::read_to_string(&snap).unwrap_or_else(|e| panic!("读取 {}: {e}", snap.display()));
    assert!(
        text.contains("routine = \"pkg_a.prc_clean\""),
        "快照承载完整可调用面：{text}"
    );
    assert!(
        text.contains(&format!("source = \"{}\"", root.display())),
        "快照标注来源为研发态根：{text}"
    );

    // 运行态目录独立核验：同一结论，且不回写快照（真源仍 = 研发态 spinduck.toml）
    let mut buf2: Vec<u8> = Vec::new();
    let code2 = spinduck_core::cli::run::run(
        &["verify".to_string(), home.to_string_lossy().to_string()],
        &mut buf2,
    );
    let second = String::from_utf8(buf2).unwrap();
    assert!(
        second.contains("verify: entries=3") && second.contains("failed=3"),
        "运行态目录必须复现同一核验结论：{second}"
    );
    assert!(
        second.contains("(未改写"),
        "快照不得被运行态反向覆盖：{second}"
    );
    assert_eq!(
        std::fs::read_to_string(&snap).unwrap(),
        text,
        "两次 verify 后快照内容一字不变"
    );
    assert_eq!(code2, 1, "缺口仍在，退出码不变");
}
