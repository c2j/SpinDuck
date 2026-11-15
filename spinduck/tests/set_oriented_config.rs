//! 集合化配置与运行态快照契约。

use spinduck_core::project::{manifest_text, ProjectConfig};

#[cfg(feature = "cli")]
mod cli {
    use spinduck_core::{BindMap, DuckExecutor, SpinValue, SqlExecutor};

    fn fixture(enabled: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("spinduck.toml"),
            format!("[project]\nname = 'copy'\n[engine]\nset_oriented = {enabled}\n"),
        )
        .unwrap();
        std::fs::write(
            root.path().join("copy.sql"),
            "CREATE PROCEDURE copy_rows IS BEGIN FOR r IN SELECT id FROM source_rows LOOP \
             INSERT INTO target_rows(id) VALUES (r.id); END LOOP; END;",
        )
        .unwrap();
        let ex = DuckExecutor::open(root.path().join("test.db").to_str().unwrap()).unwrap();
        for sql in [
            "CREATE TABLE source_rows(id BIGINT)",
            "CREATE TABLE target_rows(id BIGINT)",
            "INSERT INTO source_rows VALUES (7), (7), (NULL)",
        ] {
            ex.execute(sql, &BindMap::new()).unwrap();
        }
        root
    }

    #[test]
    fn should_check_precomputed_dependencies_before_optimization_or_trace_creation() {
        let root = fixture(true);
        let plan = root.path().join("copy.toml");
        std::fs::write(&plan, "[plan]\nschema_version = 1\ntarget = 'copy_rows'\n\
            generator = 'codeweb test'\nproject_dir = '.'\n\
            [sources]\nddl_routines = ['copy.sql']\n\
            [[ref]]\nobject = 'absent'\nkind = 'table'\nrole = 'read'\nfrom = 'copy_rows'\nat = 'copy.sql:1'\n").unwrap();
        let trace = root.path().join("trace.jsonl");
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_spinduck"))
            .arg("call")
            .arg("--db")
            .arg(root.path().join("test.db"))
            .arg("--plan")
            .arg(plan)
            .arg("--trace")
            .arg(&trace)
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{stdout}");
        assert!(
            stdout.contains("MISS") && stdout.contains("absent"),
            "{stdout}"
        );
        assert!(!stdout.contains("SET_ORIENTED"), "{stdout}");
        assert!(!trace.exists());
        let ex = DuckExecutor::open(root.path().join("test.db").to_str().unwrap()).unwrap();
        assert_eq!(
            ex.query("SELECT COUNT(*) FROM target_rows", &BindMap::new())
                .unwrap()
                .fetch_many(2),
            vec![vec![SpinValue::Int(0)]]
        );
    }

    #[test]
    fn should_apply_runtime_snapshot_policy_with_precomputed_plan_dependencies() {
        let root = fixture(true);
        let project = super::ProjectConfig::load(root.path()).unwrap();
        spinduck_core::project::sync_runtime_manifest(&project).unwrap();
        let plan = project.home_dir.join("copy.toml");
        std::fs::write(&plan, "[plan]\nschema_version = 1\ntarget = 'copy_rows'\n\
            generator = 'codeweb test'\nproject_dir = '.'\n\
            [sources]\nddl_routines = ['copy.sql']\n\
            [[ref]]\nobject = 'source_rows'\nkind = 'table'\nrole = 'read'\nfrom = 'copy_rows'\nat = 'copy.sql:1'\n\
            [[ref]]\nobject = 'target_rows'\nkind = 'table'\nrole = 'write'\nfrom = 'copy_rows'\nat = 'copy.sql:1'\n").unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_spinduck"))
            .arg("call")
            .arg("--db")
            .arg(root.path().join("test.db"))
            .arg("--plan")
            .arg(plan)
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{stdout}");
        assert_eq!(
            stdout,
            "SET_ORIENTED copy_rows converted\nCALL copy_rows OK\n"
        );
        let ex = DuckExecutor::open(root.path().join("test.db").to_str().unwrap()).unwrap();
        assert_eq!(
            ex.query("SELECT COUNT(*) FROM target_rows", &BindMap::new())
                .unwrap()
                .fetch_many(2),
            vec![vec![SpinValue::Int(3)]]
        );
    }

    #[test]
    fn should_apply_project_set_oriented_policy_without_changing_off_stdout() {
        for enabled in [false, true] {
            let root = fixture(enabled);
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_spinduck"))
                .arg("call")
                .arg("--db")
                .arg(root.path().join("test.db"))
                .arg("--file")
                .arg(root.path().join("copy.sql"))
                .arg("copy_rows")
                .output()
                .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(output.status.success(), "{stdout}");
            assert_eq!(
                stdout,
                if enabled {
                    "SET_ORIENTED copy_rows converted\nCALL copy_rows OK\n"
                } else {
                    "CALL copy_rows OK\n"
                }
            );
            let ex = DuckExecutor::open(root.path().join("test.db").to_str().unwrap()).unwrap();
            assert_eq!(
                ex.query(
                    "SELECT id FROM target_rows ORDER BY id NULLS LAST",
                    &BindMap::new()
                )
                .unwrap()
                .fetch_many(10),
                vec![
                    vec![SpinValue::Int(7)],
                    vec![SpinValue::Int(7)],
                    vec![SpinValue::Null]
                ]
            );
        }
    }
}

#[test]
fn should_reject_non_boolean_set_oriented_configuration() {
    let root = tempfile::tempdir().unwrap();
    for extra in [
        "[engine]\nset_oriented = 'true'",
        "[engine]\nset_oriented = 1",
        "engine = true",
    ] {
        let text = format!("{extra}\n[project]\nname = 'copy'\n");
        let error = ProjectConfig::from_toml_str(&text, root.path()).unwrap_err();
        assert_eq!(error.code.0, "ENV-0030");
        assert!(error.message.contains("engine"), "{}", error.message);
    }
}

#[test]
fn should_roundtrip_explicit_set_oriented_policy_in_runtime_manifest() {
    let root = tempfile::tempdir().unwrap();
    for enabled in [false, true] {
        let text = format!("[project]\nname = 'copy'\n[engine]\nset_oriented = {enabled}\n");
        let project = ProjectConfig::from_toml_str(&text, root.path()).unwrap();
        assert_eq!(project.set_oriented, enabled);
        let snapshot = manifest_text(&project).unwrap();
        let loaded = ProjectConfig::from_toml_str(&snapshot, root.path()).unwrap();
        assert_eq!(loaded.set_oriented, enabled);
    }
    let project = ProjectConfig::from_toml_str("[project]\nname = 'copy'", root.path()).unwrap();
    assert!(!project.set_oriented);
}
