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

/// 场景指向别的例程、但断言集直接调用入口的过程——`verify` 不执行断言（断言层在
/// `cargo test --features native --test cases_native`），所以仍记 Skip，但理由不能再写
/// "可执行性未验证"。
const ONLY_ASSERTS_CASE: &str = r#"
[case]
name = "only_asserts"
kind = "function"
routine = "fnc_unrelated"
baseline = "expected.json"

[[case.scenarios]]
why = "本用例的场景不指向入口"
args = ["'20240105'"]

[[case.assert]]
why = "入口 OUT 参逐值"
kind = "out"
target = "pkg_a.prc_clean"
param = "p_n"
args = ["'20240105'", "0"]
want = "2"

[[case.assert]]
why = "入口错误契约"
kind = "error"
target = "PKG_A.PRC_CLEAN"
args = ["'20240105'", "0"]
want = "BIZ-0151|缺少实参"

[[case.assert]]
why = "干扰项：rows 断言的 target 是 SQL，不该计入本入口的覆盖数"
kind = "rows"
target = "SELECT count(*) FROM t_ok"
want = "[2]"
"#;

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

const CLEAN_ONLY: &str = r#"
CREATE OR REPLACE PACKAGE pkg_a AS
  PROCEDURE prc_clean(p_dt IN VARCHAR2, p_n OUT NUMBER);
END;
/
CREATE OR REPLACE PACKAGE BODY pkg_a AS
  PROCEDURE prc_clean(p_dt IN VARCHAR2, p_n OUT NUMBER) IS
  BEGIN
    SELECT count(*) INTO p_n FROM t_ok WHERE a > 0;
  END;
END;
/
"#;

struct FixtureExport(Vec<u8>);

impl spinduck_core::plan::codeweb::CodewebExport for FixtureExport {
    fn export_json(
        &self,
        _project: &Path,
    ) -> Result<Box<dyn std::io::Read + Send>, spinduck_core::EngineError> {
        Ok(Box::new(std::io::Cursor::new(self.0.clone())))
    }
}

fn verify_export(root: &Path, json: serde_json::Value) -> (i32, String) {
    std::fs::write(root.join("codeweb.toml"), b"").unwrap();
    let export = FixtureExport(serde_json::to_vec(&json).unwrap());
    let mut buf = Vec::new();
    let result = spinduck_core::cli::verify_cmd(Some(root), &export, &mut buf);
    match result {
        Ok(code) => (code, String::from_utf8(buf).unwrap()),
        Err(e) => {
            use std::io::Write;
            let _ = writeln!(buf, "[{}] {}", e.code, e.message);
            (1, String::from_utf8(buf).unwrap())
        }
    }
}

fn forward(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

enum ChainShape {
    Clean,
    WithGaps,
}

fn chain_graph(file: &str, shape: ChainShape) -> serde_json::Value {
    let mut nodes = vec![
        serde_json::json!({"id": 1, "type": "package", "name": "pkg_a", "file": file}),
        serde_json::json!({"id": 2, "type": "procedure", "name": "prc_clean", "file": file, "line": 1}),
        serde_json::json!({"id": 5, "type": "table", "name": "t_ok"}),
    ];
    let mut edges = vec![
        serde_json::json!({"source": 1, "target": 2, "type": "contains_routine"}),
        serde_json::json!({
            "source": 2, "target": 5, "type": "table_access",
            "modes": ["read"], "file": file, "line": 4
        }),
    ];
    if matches!(shape, ChainShape::WithGaps) {
        push_gap(&mut nodes, &mut edges, file);
    }
    serde_json::json!({"nodes": nodes, "edges": edges})
}

fn push_gap(nodes: &mut Vec<serde_json::Value>, edges: &mut Vec<serde_json::Value>, file: &str) {
    nodes.push(serde_json::json!({
        "id": 3, "type": "procedure", "name": "prc_gap", "file": file, "line": 8
    }));
    nodes.push(serde_json::json!({"id": 6, "type": "table", "name": "t_missing"}));
    nodes.push(serde_json::json!({
        "id": 7, "type": "unresolved", "raw_expr": "pkg_a.prc_gone", "context": "call"
    }));
    nodes.push(serde_json::json!({
        "id": 4, "type": "procedure", "name": "prc_typo", "file": file, "line": 1
    }));
    edges.push(serde_json::json!({"source": 1, "target": 3, "type": "contains_routine"}));
    edges.push(serde_json::json!({"source": 1, "target": 4, "type": "contains_routine"}));
    edges.push(serde_json::json!({
        "source": 3, "target": 6, "type": "table_access",
        "modes": ["write"], "file": file, "line": 9
    }));
    edges.push(serde_json::json!({
        "source": 3, "target": 7, "type": "direct", "file": file, "line": 10
    }));
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

/// 断言层已覆盖该入口时，Skip 的理由必须说清"本命令不执行断言"，而不是声称
/// 可执行性没被验证——后者与 `[[case.assert]]` 的存在直接矛盾。
#[test]
fn should_credit_assert_layer_when_no_scenario_declares_the_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"asserted\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_a.prc_clean\"\n",
    );
    write(root, "sql/ddl/tables.sql", DDL);
    write(root, "sql/routines/pkg_a.sql", ROUTINES);
    write(root, "cases/only_asserts.case.toml", ONLY_ASSERTS_CASE);
    write(root, "cases/datasets/seed.sql", SEED);

    let project = ProjectConfig::load(root).unwrap();
    let reports = verify_project(&project).unwrap_or_else(|e| panic!("verify: {}", e.message));
    let clean = report(&reports, "pkg_a.prc_clean");
    assert_eq!(
        status(clean, CheckKind::Scenario, "-"),
        Status::Skip,
        "无场景声明该入口：仍记 Skip（verify 不执行断言）"
    );
    let detail = clean
        .checks
        .iter()
        .find(|c| c.kind == CheckKind::Scenario)
        .unwrap()
        .detail
        .clone();
    assert!(
        detail.contains("2 条") && detail.contains("断言"),
        "理由需给出覆盖该入口的断言条数并点名断言层：{detail}"
    );
    assert!(
        detail.contains("only_asserts"),
        "理由需指向承载断言的用例名：{detail}"
    );
    assert!(
        !detail.contains("可执行性未验证"),
        "断言层已覆盖时不得再声称未验证：{detail}"
    );
}

#[test]
fn should_render_reports_and_exit_1_when_chain_has_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fixture(root);
    let file = forward(&root.join("sql/routines/pkg_a.sql"));
    let (code, text) = verify_export(root, chain_graph(&file, ChainShape::WithGaps));
    assert_eq!(code, 1, "有链路缺口必须非零退出：{text}");
    assert!(text.contains("manifest: "), "{text}");
    assert!(
        text.contains("MISS relation:t_missing <- pkg_a.prc_gap"),
        "{text}"
    );
    assert!(
        text.contains("MISS routine:pkg_a.prc_gone <- pkg_a.prc_gap"),
        "{text}"
    );
    assert!(
        text.contains("compile: entries=3 missing=2 failed=0 dynamic=0"),
        "缺件总数须钉死，不能只钉入口数：{text}"
    );
    assert!(!text.contains("PASS Scenario"), "{text}");
    assert!(!text.contains("== entry"), "{text}");
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
    write(root, "sql/routines/pkg_a.sql", CLEAN_ONLY);
    write(root, "cases/x.case.toml", CASE);
    write(root, "cases/datasets/seed.sql", SEED);
    let file = forward(&root.join("sql/routines/pkg_a.sql"));
    let (code, text) = verify_export(root, chain_graph(&file, ChainShape::Clean));
    assert_eq!(code, 0, "干净链路应零退出：{text}");
    assert!(
        text.contains("compile: entries=1 missing=0 failed=0 dynamic=0"),
        "{text}"
    );
    assert!(text.contains("(updated)"), "{text}");
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
    let file = forward(&root.join("sql/routines/pkg_a.sql"));
    let graph = chain_graph(&file, ChainShape::WithGaps);

    let (_, first) = verify_export(root, graph.clone());
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

    // 运行态目录独立核验：同一缺件，且不回写快照（真源仍 = 研发态 spinduck.toml）
    let (code2, second) = verify_export(&home, graph);
    assert!(
        second.contains("MISS relation:t_missing <- pkg_a.prc_gap"),
        "运行态目录必须复现同一缺件：{second}"
    );
    assert!(
        second.contains("compile: entries=3 missing=2 failed=0 dynamic=0"),
        "运行态目录须复现同一份完整结论，不能只钉一条缺件：{second}"
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

#[test]
fn should_build_views_from_real_body_when_kernel_can_parse_it() {
    // 视图体只在内核确实不接受时才降级为占位视图（§12.1：链路 SQL 会读视图列）
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"views\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_v.prc_sum\"\n",
    );
    write(root, "sql/ddl/tables.sql", DDL);
    write(
        root,
        "sql/ddl/views/v_ok.sql",
        "CREATE OR REPLACE VIEW v_ok AS SELECT a FROM t_ok;\n",
    );
    write(
        root,
        "sql/routines/pkg_v.sql",
        r#"
CREATE OR REPLACE PACKAGE pkg_v AS
  PROCEDURE prc_sum(p_i_date IN VARCHAR2, p_o_succeed OUT VARCHAR2);
END;
/
CREATE OR REPLACE PACKAGE BODY pkg_v AS
  PROCEDURE prc_sum(p_i_date IN VARCHAR2, p_o_succeed OUT VARCHAR2) IS
    v_n NUMBER;
  BEGIN
    SELECT count(*) INTO v_n FROM v_ok WHERE a > 0;
    p_o_succeed := '0';
  END;
END;
/
"#,
    );
    write(
        root,
        "cases/v.case.toml",
        r#"
[case]
name = "v"
kind = "procedure"
entry = "pkg_v.prc_sum"
baseline = "expected.json"
[case.output]
sql = "SELECT a FROM t_ok ORDER BY a"
[[case.scenarios]]
why = "视图列可读"
args = ["'20240105'", "null"]
"#,
    );
    write(root, "cases/datasets/seed.sql", SEED);

    let project = ProjectConfig::load(root).unwrap_or_else(|e| panic!("加载: {}", e.message));
    let reports = verify_project(&project).unwrap_or_else(|e| panic!("verify: {}", e.message));
    let sum = report(&reports, "pkg_v.prc_sum");
    assert_eq!(
        status(sum, CheckKind::Relation, "v_ok"),
        Status::Pass,
        "视图须在内核目录可见"
    );
    assert_eq!(
        status(sum, CheckKind::Scenario, "v#1"),
        Status::Pass,
        "视图体真实建立后链路 SQL 才能读列；占位视图（SELECT 1）读不到 a：{:?}",
        sum.checks
    );
}

#[test]
fn should_fall_back_to_placeholder_view_when_kernel_cannot_build_body() {
    // 语料视图体里有 GaussDB 专属函数：内核建不出来，必须退占位而不是让整个核验失败
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"vforce\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_v.prc_sum\"\n",
    );
    write(root, "sql/ddl/tables.sql", DDL);
    write(
        root,
        "sql/ddl/views/v_bad.sql",
        "CREATE OR REPLACE VIEW v_bad AS SELECT gaussdb_only_fn(a) FROM t_ok;\n",
    );
    write(
        root,
        "sql/routines/pkg_v.sql",
        r#"
CREATE OR REPLACE PACKAGE pkg_v AS
  PROCEDURE prc_sum(p_i_date IN VARCHAR2, p_o_succeed OUT VARCHAR2);
END;
/
CREATE OR REPLACE PACKAGE BODY pkg_v AS
  PROCEDURE prc_sum(p_i_date IN VARCHAR2, p_o_succeed OUT VARCHAR2) IS
  BEGIN
    SELECT count(*) INTO p_o_succeed FROM v_bad;
    p_o_succeed := '0';
  END;
END;
/
"#,
    );
    write(
        root,
        "cases/v.case.toml",
        r#"
[case]
name = "v"
kind = "procedure"
entry = "pkg_v.prc_sum"
baseline = "expected.json"
[case.output]
sql = "SELECT a FROM t_ok ORDER BY a"
[[case.scenarios]]
why = "占位视图仍可计数"
args = ["'20240105'", "null"]
"#,
    );
    write(root, "cases/datasets/seed.sql", SEED);

    let project = ProjectConfig::load(root).unwrap_or_else(|e| panic!("加载: {}", e.message));
    let reports = verify_project(&project).unwrap_or_else(|e| panic!("verify: {}", e.message));
    let sum = report(&reports, "pkg_v.prc_sum");
    assert_eq!(
        status(sum, CheckKind::Relation, "v_bad"),
        Status::Pass,
        "视图体不可建时仍须有对象可核验（占位）"
    );
    assert_eq!(
        sum.failed().len(),
        0,
        "退回占位后链路不得判失败：{:?}",
        sum.checks
    );
}
