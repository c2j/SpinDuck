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

/// D7 双臂判定行契约（§3.2 同源铁律文本面）：admit 数据面求值
/// 判别式后，ADMIT 行透出与 `SET_ORIENTED` 同源的 `arm=` 片段——
/// 渲染路径漏传 arm 时本断言变红。
const DUAL_ARM_SQL: &str = "CREATE PROCEDURE copy_partial IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
     FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
     BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
     EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
     BEGIN \
     IF v_route = '1' THEN v_yj := 'U'; \
     ELSE \
     SELECT ct INTO v_ct FROM dim_tab WHERE dim_tab.k = r.k; \
     SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_a END INTO v_yj FROM sys_dummy; \
     END IF; \
     EXCEPTION WHEN OTHERS THEN v_yj := '0'; END; \
     INSERT INTO target_rows(val, yj) VALUES (r.a, v_yj); \
     END LOOP; END;\n";

const DUAL_ARM_DDL: &str =
    "CREATE TABLE source_rows(a BIGINT, k VARCHAR, yj_a VARCHAR, yj_b VARCHAR);\n\
     CREATE TABLE target_rows(val BIGINT, yj VARCHAR);\n\
     CREATE TABLE route_tab(kind VARCHAR, op VARCHAR);\n\
     CREATE TABLE dim_tab(k VARCHAR, ct VARCHAR);\n\
     CREATE TABLE sys_dummy(dummy VARCHAR);\n";

fn typed_table(name: &str, cols: &[(&str, SpinType)]) -> TableMetadata {
    TableMetadata {
        name: vec![name.to_string()],
        columns: cols
            .iter()
            .map(|(n, t)| ColumnMetadata {
                name: (*n).into(),
                source_type: String::new(),
                canonical_type: Some(t.clone()),
                nullable: true,
                default_expr_raw: None,
            })
            .collect(),
        constraints: Vec::new(),
        indexes: Vec::new(),
        indexes_complete: true,
        has_opaque_ddl: false,
    }
}

fn dual_arm_snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![
            typed_table(
                "source_rows",
                &[
                    ("a", SpinType::BigInt),
                    ("k", SpinType::Varchar),
                    ("yj_a", SpinType::Varchar),
                    ("yj_b", SpinType::Varchar),
                ],
            ),
            typed_table(
                "target_rows",
                &[("val", SpinType::BigInt), ("yj", SpinType::Varchar)],
            ),
            typed_table(
                "route_tab",
                &[("kind", SpinType::Varchar), ("op", SpinType::Varchar)],
            ),
            typed_table(
                "dim_tab",
                &[("k", SpinType::Varchar), ("ct", SpinType::Varchar)],
            ),
            typed_table("sys_dummy", &[("dummy", SpinType::Varchar)]),
        ],
        diagnostics: Vec::new(),
        fingerprint: "t".into(),
    }
}

fn dual_arm_graph(file: &str) -> serde_json::Value {
    let tables = [
        ("source_rows", "read"),
        ("target_rows", "write"),
        ("route_tab", "read"),
        ("dim_tab", "read"),
        ("sys_dummy", "read"),
    ];
    let nodes: Vec<serde_json::Value> = std::iter::once(serde_json::json!({
        "id": 1, "type": "procedure", "name": "copy_partial", "file": file
    }))
    .chain(
        tables
            .iter()
            .enumerate()
            .map(|(i, (name, _))| serde_json::json!({"id": i + 2, "type": "table", "name": name})),
    )
    .collect();
    let edges: Vec<serde_json::Value> = tables
        .iter()
        .enumerate()
        .map(|(i, (_, mode))| {
            serde_json::json!({
                "source": 1, "target": i + 2, "type": "table_access",
                "modes": [mode], "file": file, "line": 1
            })
        })
        .collect();
    serde_json::json!({ "nodes": nodes, "edges": edges })
}

#[test]
fn compile_admit_should_render_selected_arm_for_dual_arm_plan() {
    use spinduck_core::exec::{BindMap, SqlExecutor};
    use spinduck_core::sql::DuckExecutor;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "spinduck.toml", PROJECT_PLAN);
    write(root, "codeweb.toml", "");
    write(root, "sql/routines/copy_partial.sql", DUAL_ARM_SQL);
    write(root, "sql/ddl/tables.sql", DUAL_ARM_DDL);
    let file = root
        .join("sql/routines/copy_partial.sql")
        .to_string_lossy()
        .replace('\\', "/");
    let export = JsonCodewebSource {
        generator: "codeweb 0.9.9".to_string(),
        json: serde_json::to_vec(&dual_arm_graph(&file)).unwrap(),
    };
    // admit 数据面：真实 DuckDB，route='0' → 判别式选 ELSE 臂。
    let db = root.join("t.duckdb");
    let ex = DuckExecutor::open(&db.to_string_lossy()).unwrap();
    for s in [
        "CREATE TABLE source_rows(a BIGINT, k VARCHAR, yj_a VARCHAR, yj_b VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, yj VARCHAR)",
        "CREATE TABLE route_tab(kind VARCHAR, op VARCHAR)",
        "CREATE TABLE dim_tab(k VARCHAR, ct VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'K1', 'AA1', 'BB1')",
        "INSERT INTO route_tab VALUES ('0', 'X')",
        "INSERT INTO dim_tab VALUES ('K1', 'A')",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        ex.execute(s, &BindMap::new()).unwrap();
    }
    let input = AdmitInput {
        snapshot: dual_arm_snapshot(),
        executor: Some(Box::new(ex)),
        macro_names: BTreeSet::new(),
    };
    let mut opts = project_opts(root);
    opts.entries = vec!["copy_partial".to_string()];
    opts.set_oriented = Some(spinduck_core::ir::set_oriented::SetOrientedMode::BatchAtomic);
    let mut buf = Vec::new();
    let code = compile_project_admit_with_source(opts, &export, Some(input), &mut buf)
        .unwrap_or_else(|e| panic!("{}", e.message));
    let out = String::from_utf8(buf).unwrap();
    assert_eq!(code, EXIT_OK, "{out}");
    assert!(out.contains("ADMIT copy_partial"), "{out}");
    assert!(out.contains("arm=else"), "{out}");
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
