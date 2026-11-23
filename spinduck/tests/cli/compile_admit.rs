//! `compile --admit` 契约（设计 v3.1 §3.1/§3.2/§3.4）：
//! 旗标纪律（从属旗标需 `--admit`、D9 强制 `--db`）、注入接缝
//! （`compile_project_admit_with_source` + 字面量 `AdmitInput`）、
//! 无旗标零输出回归。

use super::{cli, write, EXIT_OK, EXIT_USAGE};
use spinduck_core::cli::args::AdmitMode;
use spinduck_core::cli::{compile_project_admit_with_source, ProjectCompile};
use spinduck_core::ir::set_oriented::admit::AdmitInput;
use spinduck_core::ir::set_oriented::metadata::{ColumnMetadata, SchemaSnapshot, TableMetadata};
use spinduck_core::plan::codeweb::JsonCodewebSource;
use spinduck_core::types::SpinType;
use std::collections::BTreeSet;
use std::path::Path;

const PROJECT_PLAN: &str = "[project]\nname = \"p\"\nkernel = \"1.5.5\"\n\n[paths]\nddl = \"sql/ddl\"\nroutines = \"sql/routines\"\nhome = \".spinduck\"\n";

const COPY_ROWS: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
     FOR r IN SELECT id FROM source_rows LOOP \
     INSERT INTO target_rows(id) VALUES (r.id); END LOOP; END;\n";

fn int_table(name: &str) -> TableMetadata {
    TableMetadata {
        name: vec![name.to_string()],
        columns: vec![ColumnMetadata {
            name: "id".into(),
            source_type: "BIGINT".into(),
            canonical_type: Some(SpinType::BigInt),
            nullable: true,
            default_expr_raw: None,
        }],
        constraints: Vec::new(),
        indexes: Vec::new(),
        indexes_complete: true,
        has_opaque_ddl: false,
    }
}

fn admit_input() -> AdmitInput {
    AdmitInput {
        snapshot: SchemaSnapshot {
            tables: vec![int_table("source_rows"), int_table("target_rows")],
            diagnostics: Vec::new(),
            fingerprint: "t".into(),
        },
        executor: None,
        macro_names: BTreeSet::new(),
    }
}

const TABLES_DDL: &str =
    "CREATE TABLE source_rows (id BIGINT);\nCREATE TABLE target_rows (id BIGINT);\n";

/// 例程 + 两表的最小图：procedure 节点 + read/write 边（与 compile_entry 夹具同形）。
fn graph(file: &str) -> serde_json::Value {
    serde_json::json!({
        "nodes": [
            {"id": 1, "type": "procedure", "name": "copy_rows", "file": file},
            {"id": 2, "type": "table", "name": "source_rows"},
            {"id": 3, "type": "table", "name": "target_rows"},
        ],
        "edges": [
            {"source": 1, "target": 2, "type": "table_access", "modes": ["read"], "file": file, "line": 1},
            {"source": 1, "target": 3, "type": "table_access", "modes": ["write"], "file": file, "line": 2},
        ]
    })
}

/// 项目骨架 + 例程/DDL/空 codeweb.toml；返回注入用的 JsonCodewebSource。
fn fixture(root: &Path) -> JsonCodewebSource {
    write(root, "spinduck.toml", PROJECT_PLAN);
    write(root, "codeweb.toml", "");
    write(root, "sql/routines/copy_rows.sql", COPY_ROWS);
    write(root, "sql/ddl/tables.sql", TABLES_DDL);
    let file = root
        .join("sql/routines/copy_rows.sql")
        .to_string_lossy()
        .replace('\\', "/");
    JsonCodewebSource {
        generator: "codeweb 0.9.9".to_string(),
        json: serde_json::to_vec(&graph(&file)).unwrap(),
    }
}

fn project_opts(dir: &Path) -> ProjectCompile {
    ProjectCompile {
        dir: Some(dir.to_path_buf()),
        schema: None,
        entries: vec!["copy_rows".to_string()],
        analyze: false,
        codeweb_project: None,
        sync_manifest: false,
        admit: Some(AdmitMode::Report),
        db: None,
        set_oriented: Some(spinduck_core::ir::set_oriented::SetOrientedMode::Strict),
        admit_record: None,
    }
}

#[test]
fn compile_admit_should_reject_db_and_record_without_admit() {
    // §3.1 约束 1 + 测试 9：从属旗标不带 --admit = ENG-0010 用法错（退出码 2）。
    for args in [
        vec!["compile", "--db", "x.duckdb"],
        vec!["compile", "--admit-record", "r.toml"],
        vec!["compile", "--set-oriented=strict"],
    ] {
        let (code, out, err) = cli(&args);
        assert_eq!(code, EXIT_USAGE, "{args:?}: {out} {err}");
        assert!(out.contains("ENG-0010"), "{args:?}: {out}");
    }
}

#[test]
fn compile_admit_should_reject_missing_db_per_d9() {
    // D9 定稿：--admit 强制 --db；无数据面只剩静态体检，CLI 层用法错。
    for args in [
        vec!["compile", "--admit"],
        vec!["compile", "--admit=error"],
        vec!["compile", "--admit=bogus", "--db", "x"],
    ] {
        let (code, out, err) = cli(&args);
        assert_eq!(code, EXIT_USAGE, "{args:?}: {out} {err}");
        assert!(out.contains("ENG-0010"), "{args:?}: {out}");
    }
}

#[test]
fn should_admit_via_injected_snapshot_without_codeweb_project() {
    // 测试 2b：字面量 AdmitInput（无真实 codeweb 项目/无 --db）经
    // compile_project_admit_with_source 产出判定块；executor=None → StaticOnly。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let export = fixture(root);
    let mut buf = Vec::new();
    let code = compile_project_admit_with_source(
        project_opts(root),
        &export,
        Some(admit_input()),
        &mut buf,
    )
    .unwrap_or_else(|e| panic!("{}", e.message));
    let out = String::from_utf8(buf).unwrap();
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.contains("admit: targets=1"), "{out}");
    assert!(out.contains("ADMIT copy_rows static-only"), "{out}");
    assert!(out.contains("state=unchecked"), "{out}");
    assert!(
        !out.contains("state=passed"),
        "无数据面不得出现 Passed: {out}"
    );
}

#[test]
fn compile_admit_error_should_not_fail_on_conservative() {
    // 测试 8a：executor=None → StaticOnly/conservative，--admit=error 不判红。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let export = fixture(root);
    let mut opts = project_opts(root);
    opts.admit = Some(AdmitMode::Error);
    let mut buf = Vec::new();
    let code = compile_project_admit_with_source(opts, &export, Some(admit_input()), &mut buf)
        .unwrap_or_else(|e| panic!("{}", e.message));
    let out = String::from_utf8(buf).unwrap();
    assert_eq!(code, EXIT_OK, "conservative 不得触发 --admit=error: {out}");
    assert!(out.contains("skipped=0"), "{out}");
}

#[test]
fn compile_admit_error_should_fail_on_firm_skipped() {
    // 测试 8b：空库 executor=Some → 运行期探测得到真实缺陷（firm Skipped），
    // --admit=error 判红；同输入 --admit=report 仍 EXIT_OK。
    use spinduck_core::sql::exec_native::DuckExecutor;
    fn runtime_input() -> AdmitInput {
        let mut input = admit_input();
        input.executor = Some(Box::new(
            DuckExecutor::open_in_memory().unwrap_or_else(|e| panic!("{:?}", e)),
        ));
        input
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let export = fixture(root);
    for (mode, want) in [(AdmitMode::Error, 1), (AdmitMode::Report, 0)] {
        let mut opts = project_opts(root);
        opts.admit = Some(mode);
        let mut buf = Vec::new();
        let code =
            compile_project_admit_with_source(opts, &export, Some(runtime_input()), &mut buf)
                .unwrap_or_else(|e| panic!("{}", e.message));
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(code, want, "{mode:?}: {out}");
        if mode == AdmitMode::Error {
            assert!(out.contains("skipped="), "{out}");
        }
    }
}

#[test]
fn compile_without_admit_should_render_unchanged() {
    // 测试 7（库级面）：不带 admit 请求时 stdout 无 ADMIT 行、退出码不变。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let export = fixture(root);
    let mut opts = project_opts(root);
    opts.admit = None;
    opts.set_oriented = None;
    let mut buf = Vec::new();
    let code = compile_project_admit_with_source(opts, &export, None, &mut buf)
        .unwrap_or_else(|e| panic!("{}", e.message));
    let out = String::from_utf8(buf).unwrap();
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(!out.contains("ADMIT"), "无 --admit 不得输出判定行: {out}");
    assert!(out.contains("compile: entries=1"), "{out}");
}
