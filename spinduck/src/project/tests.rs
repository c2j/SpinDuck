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
fn should_load_runtime_snapshot_from_conf_and_prefer_dev_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 运行态目录：只有 `compile --sync-manifest` 生成的 conf/ 快照，没有手写 spinduck.toml
    write(root, "conf/spinduck.runtime.toml", PROJ);
    assert_eq!(
        ProjectConfig::load(root).unwrap().name,
        "demo-procs",
        "运行态目录必须自带可加载的项目声明"
    );
    assert_eq!(
        ProjectConfig::discover(&root.join("logs/engine")).map(|p| p.name),
        Some("demo-procs".to_string()),
        "从运行态子目录向上查找也应命中快照"
    );

    // 同一目录同时存在两份时，手写声明是唯一真源，优先于快照
    write(
        root,
        "spinduck.toml",
        &PROJ.replace("demo-procs", "dev-root"),
    );
    assert_eq!(ProjectConfig::load(root).unwrap().name, "dev-root");
}

#[test]
fn should_return_none_when_no_project_file_above() {
    let tmp = tempfile::tempdir().unwrap();
    let orphan = tmp.path().join("corpus/routines");
    std::fs::create_dir_all(&orphan).unwrap();
    assert!(ProjectConfig::discover(&orphan).is_none());
}

#[test]
fn should_locate_error_mention_only_project_declaration() {
    // F7：locate 跨命令共用，错误消息不得绑定具体子命令名
    let e = ProjectConfig::locate(std::path::Path::new("/nonexistent/spinduck-f7")).unwrap_err();
    let msg = e.message;
    assert!(msg.contains("项目声明"), "{msg}");
    assert!(!msg.contains("verify"), "消息不得带 verify 字样: {msg}");
}

#[test]
fn should_not_recognize_legacy_snapshot_name() {
    // issue #38：彻底换名，不留旧名 fallback——旧名快照不再是项目声明
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "conf/spinduck.toml", PROJ);
    assert!(ProjectConfig::load(root).is_err());
    assert!(ProjectConfig::discover(root).is_none());
}

#[test]
fn should_parse_entry_declarations_and_reject_unqualified_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        r#"
[project]
name = "chain"

[[entry]]
routine = "pkg_split_trade_step2.prc_pre_deal_shjycf"
kind = "procedure"

[[entry]]
routine = "PKG_QQ_SPLIT.PRC_GENERAL_DEAL_INST_SH_BUY"
"#,
    );
    let p = ProjectConfig::load(root).unwrap();
    assert_eq!(p.entries.len(), 2, "[[entry]] 允许多个");
    assert_eq!(
        p.entries[0].routine,
        "pkg_split_trade_step2.prc_pre_deal_shjycf"
    );
    assert_eq!(p.entries[0].kind, CaseKind::Procedure);
    assert_eq!(
        p.entries[1].routine, "pkg_qq_split.prc_general_deal_inst_sh_buy",
        "入口全名归一小写（与例程目录键同口径）"
    );
    assert_eq!(
        p.entries[1].kind,
        CaseKind::Procedure,
        "kind 缺省为 procedure"
    );
    assert_eq!(
        ProjectConfig::from_toml_str(
            "[project]\nname = \"x\"\n\n[[entry]]\nroutine = \"pkg_a.fn_x\"\nkind = \"function\"\n",
            Path::new("/tmp"),
        )
        .unwrap()
        .entries[0]
            .kind,
        CaseKind::Function,
        "入口可声明函数"
    );

    let e = ProjectConfig::from_toml_str(
        "[project]\nname = \"x\"\n\n[[entry]]\nroutine = \"prc_x\"\n",
        Path::new("/tmp"),
    )
    .unwrap_err();
    assert_eq!(e.code.to_string(), "ENV-0030");
    assert!(
        e.message.contains("package.procedure"),
        "必须说明拒绝原因：缺包限定 {}",
        e.message
    );
}

#[test]
fn should_derive_case_routine_from_entry_or_reject_both_missing() {
    let c = CaseConfig::from_toml_str(
        r#"
[case]
name = "chain-buy"
kind = "procedure"
entry = "pkg_qq_split.prc_general_deal_inst_sh_buy"
baseline = "expected.json"
[case.output]
sql = "SELECT a FROM t ORDER BY a"
[[case.scenarios]]
why = "买入"
args = ["'20240105'", "1"]
"#,
    )
    .unwrap();
    assert_eq!(
        c.routine, "pkg_qq_split.prc_general_deal_inst_sh_buy",
        "[case].entry 可作为被测例程全名来源"
    );
    assert_eq!(
        c.entry.as_deref(),
        Some("pkg_qq_split.prc_general_deal_inst_sh_buy"),
        "entry 保留原声明，供 verify 反查链路"
    );

    let e = CaseConfig::from_toml_str(
        "[case]\nname = \"x\"\nkind = \"function\"\n[[case.scenarios]]\nwhy = \"w\"\nargs = []\n",
    )
    .unwrap_err();
    assert_eq!(e.code.to_string(), "ENV-0031");
    assert!(
        e.message.contains("routine"),
        "必须提示 routine/entry 至少填一个：{}",
        e.message
    );
}

#[test]
fn should_parse_case_assert_blocks_by_kind() {
    let c = CaseConfig::from_toml_str(
        r#"
[case]
name = "asserts"
kind = "procedure"
routine = "p"
[case.output]
sql = "SELECT a FROM t ORDER BY a"
[[case.scenarios]]
why = "w"
args = ["'20250715'"]

[[case.assert]]
why = "输出表行集（粒度：过程产物）"
kind = "rows"
target = "SELECT id, fund FROM t_out ORDER BY id"
want = "[1, F01] [2, F01]"

[[case.assert]]
why = "函数返回值（粒度：单值口径）"
kind = "call"
target = "pkg.f"
args = ["'S001'", "1000"]
want = "0.1"

[[case.assert]]
why = "OUT 参（粒度：调用面）"
kind = "out"
target = "pkg.p"
args = ["null"]
param = "o"
want = "rows=3"

[[case.assert]]
why = "未处理异常（粒度：错误契约）"
kind = "error"
target = "pkg.bad"
want = "BIZ-0100|no data found"
"#,
    )
    .unwrap();
    assert_eq!(c.asserts.len(), 4, "四条断言全部识别");
    assert_eq!(c.asserts[0].kind, AssertKind::Rows);
    assert_eq!(
        c.asserts[0].target,
        "SELECT id, fund FROM t_out ORDER BY id"
    );
    assert_eq!(c.asserts[0].want, "[1, F01] [2, F01]");
    assert_eq!(c.asserts[1].kind, AssertKind::Call);
    assert_eq!(
        c.asserts[1].args,
        vec!["'S001'".to_string(), "1000".to_string()]
    );
    assert_eq!(c.asserts[2].kind, AssertKind::Out);
    assert_eq!(c.asserts[2].param, "o", "OUT 参名按声明取用");
    assert_eq!(c.asserts[3].kind, AssertKind::Error);
    assert_eq!(c.asserts[3].want, "BIZ-0100|no data found");
}

#[test]
fn should_reject_case_assert_with_unknown_kind_or_missing_fields() {
    let head = "[case]\nname = \"x\"\nkind = \"function\"\nroutine = \"p\"\n\
                [[case.scenarios]]\nwhy = \"w\"\nargs = []\n";
    let cases = [
        (
            r#"kind = "rowz"
target = "SELECT 1"
want = "1""#,
            "未知 kind 必须显式拒绝",
            "kind",
        ),
        (
            r#"kind = "rows"
target = ""
want = "1""#,
            "target 必填",
            "target",
        ),
        (
            r#"kind = "rows"
target = "SELECT 1""#,
            "want 必填（空期望等于永真断言）",
            "want",
        ),
        (
            r#"kind = "out"
target = "pkg.p"
want = "1""#,
            "out 形态缺 OUT 参名",
            "param",
        ),
    ];
    for (body, why, needle) in cases {
        let e =
            CaseConfig::from_toml_str(&format!("{head}\n[[case.assert]]\nwhy = \"w\"\n{body}\n"))
                .err()
                .unwrap_or_else(|| panic!("{why}：应报错，实际解析成功"));
        assert_eq!(e.code.to_string(), "ENV-0031", "{why}");
        assert!(
            e.message.contains(needle),
            "{why}：消息要点名缺哪项，实际 {}",
            e.message
        );
    }
}

#[test]
fn should_read_optional_codeweb_path_and_ignore_unknown_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "spinduck.toml",
        "[project]\nname = \"p\"\n[paths]\ncodeweb = \"cw\"\nnot_a_key = \"x\"\n",
    );
    let project = ProjectConfig::load(root).unwrap();
    assert_eq!(
        project.codeweb_dir.as_deref(),
        Some(root.join("cw").as_path())
    );
    let bare = ProjectConfig::from_toml_str("[project]\nname = \"p\"\n[paths]\nextra = 1\n", root)
        .unwrap();
    assert_eq!(bare.codeweb_dir, None);
}
