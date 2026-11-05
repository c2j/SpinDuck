//! 项目/用例声明单测（自 `project.rs` 拆分，AGENTS §3；断言一字不改）。

use super::*;
use std::path::Path;

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

const PROJ: &str = r#"
[project]
name = "demo-procs"
kernel = "1.5.5"

[paths]
ddl = "sql/ddl"
routines = "sql/routines"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
probes = "probes"
home = ".spinduck"

[cases]
datasets = "cases/datasets"
baselines = "cases/baselines"
include = ["cases/*.case.toml"]
"#;

#[test]
fn should_load_project_paths_and_case_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "spinduck.toml", PROJ);
    write(root, "sql/ddl/tables.sql", "CREATE TABLE t(a INT);");
    write(root, "sql/ddl/views/v.sql", "CREATE VIEW v AS SELECT 1;");
    write(
        root,
        "sql/routines/r1.sql",
        "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;",
    );
    write(
        root,
        "sql/stubs/s1.sql",
        "CREATE OR REPLACE PACKAGE pk AS END;",
    );
    write(root, "rules/spinduck-rules.toml", "[functions]\n");
    write(root, "cases/datasets/seed.sql", "INSERT INTO t VALUES (1);");
    write(root, "cases/baselines/expected.json", "{}");
    write(
        root,
        "cases/x.case.toml",
        r#"
[case]
name = "x"
kind = "procedure"
routine = "p"
baseline = "expected.json"
expect_succeed = "0"
[case.output]
sql = "SELECT a FROM t ORDER BY a"
[[case.scenarios]]
why = "单行"
args = ["'20240105'"]
"#,
    );

    let p = ProjectConfig::load(root).unwrap();
    assert_eq!(p.name, "demo-procs");
    assert_eq!(p.kernel, "1.5.5");
    assert_eq!(p.ddl_files().unwrap().len(), 1, "子目录视图不算表 DDL");
    assert_eq!(p.view_files().unwrap().len(), 1);
    assert_eq!(p.routine_files().unwrap().len(), 1);
    assert_eq!(p.stub_files().unwrap().len(), 1);
    assert_eq!(p.rules_files.len(), 1);
    assert_eq!(p.case_files().unwrap().len(), 1);

    let c = CaseConfig::load(&p.case_files().unwrap()[0]).unwrap();
    assert_eq!(c.name, "x");
    assert_eq!(c.kind, CaseKind::Procedure);
    assert_eq!(c.routine, "p");
    assert_eq!(c.scenarios.len(), 1);
    assert_eq!(c.scenarios[0].args, vec!["'20240105'".to_string()]);
    assert_eq!(c.output_sql.as_deref(), Some("SELECT a FROM t ORDER BY a"));
    assert_eq!(c.expect_succeed.as_deref(), Some("0"));
    assert_eq!(c.baseline, "expected.json");
    assert!(c.reference_routine.is_none());
    assert!(c.materialize.is_empty());
}

#[test]
fn should_default_optional_paths_when_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "spinduck.toml", "[project]\nname = \"m\"\n");
    let p = ProjectConfig::load(root).unwrap();
    assert_eq!(p.ddl_dir, root.join("sql/ddl"));
    assert_eq!(p.routines_dir, root.join("sql/routines"));
    assert_eq!(p.datasets_dir, root.join("cases/datasets"));
    assert_eq!(p.baselines_dir, root.join("cases/baselines"));
    assert_eq!(p.case_includes, vec!["cases/*.case.toml".to_string()]);
    assert!(p.stubs_dirs.is_empty());
    assert!(p.rules_files.is_empty());
}

#[test]
fn should_reject_procedure_case_without_output_sql() {
    let e = CaseConfig::from_toml_str(
        r#"
[case]
name = "bad"
kind = "procedure"
routine = "p"
[[case.scenarios]]
why = "x"
args = []
"#,
    )
    .unwrap_err();
    assert_eq!(e.code.to_string(), "ENV-0031");
}

#[test]
fn should_parse_function_case_without_output_sql() {
    let c = CaseConfig::from_toml_str(
        r#"
[case]
name = "f"
kind = "function"
routine = "fnc_x"
[[case.scenarios]]
why = "命中"
args = ["'a'", "1"]
[[case.scenarios]]
why = "未命中"
args = ["'b'", "2"]
"#,
    )
    .unwrap();
    assert_eq!(c.kind, CaseKind::Function);
    assert_eq!(c.scenarios.len(), 2);
    assert!(c.output_sql.is_none());
    assert_eq!(c.baseline, "expected.json", "默认基线名");
}

#[test]
fn should_report_missing_project_section() {
    let e = ProjectConfig::from_toml_str("[paths]\nddl = \"x\"\n", Path::new("/tmp")).unwrap_err();
    assert_eq!(e.code.to_string(), "ENV-0030");
    assert!(e.message.contains("[project]"));
}

#[test]
fn should_discover_project_root_from_nested_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("spinduck.toml"), "[project]\nname = \"p\"\n").unwrap();
    let nested = root.join("sql/routines/pkg");
    std::fs::create_dir_all(&nested).unwrap();

    let found = ProjectConfig::discover(&nested).expect("向上查找必须命中项目根");
    assert_eq!(found.root, root);
    assert_eq!(found.name, "p");
}

#[test]
fn should_return_none_when_no_project_file_above() {
    let tmp = tempfile::tempdir().unwrap();
    let orphan = tmp.path().join("corpus/routines");
    std::fs::create_dir_all(&orphan).unwrap();
    assert!(ProjectConfig::discover(&orphan).is_none());
}
