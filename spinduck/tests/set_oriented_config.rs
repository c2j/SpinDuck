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
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            ex.execute(sql, &BindMap::new()).unwrap();
        }
        root
    }

    #[test]
    fn should_override_project_mode_from_cli_and_convert_under_batch_commit() {
        for (configured, flags, expected) in [
            (true, vec!["--set-oriented=off"], "CALL copy_rows OK\n"),
            (
                false,
                vec!["--set-oriented"],
                "SET_ORIENTED copy_rows converted\nCALL copy_rows OK\n",
            ),
            (
                false,
                vec!["--set-oriented=batch-atomic"],
                "SET_ORIENTED copy_rows converted\nCALL copy_rows OK\n",
            ),
            (
                false,
                vec!["--set-oriented=batch-atomic", "--batch-commit"],
                "SET_ORIENTED copy_rows converted\nCALL copy_rows OK\n",
            ),
        ] {
            let root = fixture(configured);
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_spinduck"))
                .arg("call")
                .args(flags)
                .arg("--db")
                .arg(root.path().join("test.db"))
                .arg("--file")
                .arg(root.path().join("copy.sql"))
                .arg("copy_rows")
                .output()
                .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(output.status.success(), "{stdout}");
            assert_eq!(stdout, expected);
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

    /// codeweb 计划形态的编译期证据：codeweb.toml + 与库结构一致的 DDL 源。
    fn codeweb_evidence(root: &std::path::Path, target_ddl: &str) {
        std::fs::write(
            root.join("codeweb.toml"),
            "[project]\nname = 'copy'\n[analysis]\npaths = ['ddl/']\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("ddl")).unwrap();
        std::fs::write(
            root.join("ddl/schema.sql"),
            format!("CREATE TABLE source_rows(id BIGINT);\n{target_ddl}\n"),
        )
        .unwrap();
    }

    #[test]
    fn should_reject_offline_source_compilation_before_creating_database_or_trace() {
        let root = fixture(true);
        codeweb_evidence(root.path(), "CREATE TABLE target_rows(id BIGINT);");
        std::fs::write(root.path().join("bad.sql"),
            "CREATE PROCEDURE bad IS BEGIN FORALL i IN 1..2 INSERT INTO target_rows VALUES(i); END;").unwrap();
        let plan = root.path().join("bad.toml");
        std::fs::write(
            &plan,
            "[plan]\nschema_version = 1\ntarget = 'bad'\n\
            generator = 'codeweb test'\nproject_dir = '.'\n\
            [sources]\nddl_routines = ['bad.sql']\n",
        )
        .unwrap();
        let database = root.path().join("unopened.db");
        let trace = root.path().join("trace.jsonl");
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_spinduck"))
            .arg("call")
            .arg("--db")
            .arg(&database)
            .arg("--plan")
            .arg(plan)
            .arg("--trace")
            .arg(&trace)
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{stdout}");
        assert!(
            stdout.contains("ENG-0022") && stdout.contains("ForAll"),
            "{stdout}"
        );
        assert!(!database.exists(), "静态编译失败不得打开数据库");
        assert!(!trace.exists());
    }

    #[test]
    fn should_check_precomputed_dependencies_before_optimization_or_trace_creation() {
        let root = fixture(true);
        codeweb_evidence(root.path(), "CREATE TABLE target_rows(id BIGINT);");
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
        codeweb_evidence(root.path(), "CREATE TABLE target_rows(id BIGINT);");
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
    fn should_attach_codeweb_schema_snapshot_on_plan_call_and_skip_on_drift() {
        // CLI 接线（设计 §4.1）：codeweb 计划形态的 call 必须自带编译期结构快照。
        // 这里的分歧是运行期自检查不出来的：DDL 声明 target.id VARCHAR(24)，
        // 库里实际是 BIGINT——运行期两侧都是 BIGINT、形状检查全绿，只有
        // 编译期快照 × 运行期目录的复核能拦下。逐行回退照常写满 3 行。
        let root = fixture(true);
        codeweb_evidence(root.path(), "CREATE TABLE target_rows(id VARCHAR(24));");
        let plan = root.path().join("copy.toml");
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
            .arg(&plan)
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{stdout}");
        assert_eq!(
            stdout,
            "SET_ORIENTED copy_rows skipped reason=UnprovenTargetConstraints\nCALL copy_rows OK\n"
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
fn should_roundtrip_named_set_oriented_modes_without_losing_atomic_authorization() {
    use spinduck_core::ir::set_oriented::SetOrientedMode;
    let root = tempfile::tempdir().unwrap();
    for (name, mode) in [
        ("off", SetOrientedMode::Off),
        ("strict", SetOrientedMode::Strict),
        ("batch-atomic", SetOrientedMode::BatchAtomic),
    ] {
        let text = format!("[project]\nname = 'copy'\n[engine]\nset_oriented = '{name}'\n");
        let project = ProjectConfig::from_toml_str(&text, root.path()).unwrap();
        assert_eq!(project.set_oriented, mode);
        let snapshot = manifest_text(&project).unwrap();
        let loaded = ProjectConfig::from_toml_str(&snapshot, root.path()).unwrap();
        assert_eq!(loaded.set_oriented, mode);
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
        assert_eq!(project.set_oriented, enabled.into());
        let snapshot = manifest_text(&project).unwrap();
        let loaded = ProjectConfig::from_toml_str(&snapshot, root.path()).unwrap();
        assert_eq!(loaded.set_oriented, enabled.into());
    }
    let project = ProjectConfig::from_toml_str("[project]\nname = 'copy'", root.path()).unwrap();
    assert_eq!(
        project.set_oriented,
        spinduck_core::ir::set_oriented::SetOrientedMode::Off
    );
}
