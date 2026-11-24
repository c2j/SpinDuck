//! 编译期 codeweb 结构元数据契约（零 DuckDB）。

use spinduck_core::ir::set_oriented::metadata::SchemaSnapshot;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::SpinType;

#[path = "guard/set_oriented_metadata_cache.rs"]
mod cache;

fn export(sql: &str) -> codeweb::embed::SchemaExport {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("codeweb.toml"),
        "[project]\nname = 'metadata'\n[analysis]\npaths = ['schema.sql']\n",
    )
    .unwrap();
    std::fs::write(root.path().join("schema.sql"), sql).unwrap();
    codeweb::embed::EmbeddedProject::open_exact(root.path())
        .unwrap()
        .export_table_schemas()
        .unwrap()
}

fn catalog(sql: &str) -> spinduck_core::catalog::RoutineCatalog {
    let rules = RuleRegistry::default();
    let mut catalog = spinduck_core::catalog::RoutineCatalog::new();
    catalog.set_sql_rules(rules.clone());
    let tokens = ogsql_parser::token::tokenizer::Tokenizer::new(sql)
        .tokenize()
        .unwrap();
    for stmt in ogsql_parser::parser::Parser::new(tokens).parse() {
        for routine in spinduck_core::catalog::compile_statement_routine(&stmt, &rules).unwrap() {
            catalog.register(routine);
        }
    }
    catalog
}

#[test]
fn should_generate_a_complete_candidate_without_an_executor() {
    use spinduck_core::ir::set_oriented::{compile_set_oriented_plans, SetOrientedMode};
    let snapshot = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE source_rows(id BIGINT); CREATE TABLE target_rows(id BIGINT);"),
        &RuleRegistry::default(),
    );
    let catalog = catalog(
        "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id FROM source_rows LOOP \
        INSERT INTO target_rows(id) VALUES(r.id); END LOOP; END;",
    );
    let plans = compile_set_oriented_plans(&catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    let plan = plans.get("copy_rows").unwrap().as_ref().unwrap();
    assert_eq!(
        plan.sql(),
        "WITH sd_cur AS (SELECT \"id\" FROM \"source_rows\") \
        INSERT INTO \"target_rows\" (\"id\") SELECT sd_cur.\"id\" FROM sd_cur"
    );
    assert_eq!(plan.schema_fingerprint(), snapshot.fingerprint);
}

#[cfg(feature = "native")]
#[test]
fn should_reuse_offline_candidate_and_recheck_live_data_guards() {
    use spinduck_core::ir::set_oriented::{
        compile_set_oriented_plans, SetOrientedMode, SkipReason, Status,
    };
    use spinduck_core::{BindMap, DuckExecutor, Engine, SpinValue};
    let snapshot = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE source_rows(id BIGINT); CREATE TABLE target_rows(id BIGINT);"),
        &RuleRegistry::default(),
    );
    let catalog = catalog(
        "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id FROM source_rows LOOP \
        INSERT INTO target_rows(id) VALUES(r.id); END LOOP; END;",
    );
    let plans = compile_set_oriented_plans(&catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    let sql = plans
        .get("copy_rows")
        .unwrap()
        .as_ref()
        .unwrap()
        .sql()
        .to_string();
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.catalog = catalog;
    eng.set_oriented = SetOrientedMode::Strict;
    eng.attach_set_oriented_plans(plans);
    for ddl in [
        "CREATE TABLE source_rows(id BIGINT)",
        "CREATE TABLE target_rows(id BIGINT)",
        "INSERT INTO source_rows VALUES (1), (1), (NULL)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Converted {
            sql: sql.clone(),
            arm: None
        }
    );
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { sql, arm: None }
    );
    eng.ex
        .execute("INSERT INTO sys_dummy VALUES ('Y')", &BindMap::new())
        .unwrap();
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenSysDummyInvariant
        }
    );
    let rows = eng
        .ex
        .query(
            "SELECT id FROM target_rows ORDER BY id NULLS LAST",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20);
    assert_eq!(
        rows,
        vec![vec![SpinValue::Int(1)]; 6]
            .into_iter()
            .chain(vec![vec![SpinValue::Null]; 3])
            .collect::<Vec<_>>()
    );
}

#[cfg(feature = "native")]
#[test]
fn should_reuse_offline_not_null_and_constant_default_evidence() {
    use spinduck_core::ir::set_oriented::{compile_set_oriented_plans, SetOrientedMode, Status};
    use spinduck_core::{BindMap, DuckExecutor, Engine, SpinValue};
    let ddl = "CREATE TABLE source_rows(id BIGINT); \
        CREATE TABLE target_rows(id BIGINT, label VARCHAR(20) DEFAULT 'ready' NOT NULL);";
    let snapshot = SchemaSnapshot::from_codeweb(&export(ddl), &RuleRegistry::default());
    let catalog = catalog(
        "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id FROM source_rows LOOP \
        INSERT INTO target_rows(id) VALUES(r.id); END LOOP; END;",
    );
    let plans = compile_set_oriented_plans(&catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    let sql = plans
        .get("copy_rows")
        .unwrap()
        .as_ref()
        .unwrap()
        .sql()
        .to_string();
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.catalog = catalog;
    eng.set_oriented = SetOrientedMode::Strict;
    eng.attach_set_oriented_plans(plans);
    for stmt in ddl.split(';').filter(|s| !s.trim().is_empty()).chain([
        "INSERT INTO source_rows VALUES (1), (NULL)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ]) {
        eng.ex.execute(stmt, &BindMap::new()).unwrap();
    }
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { sql, arm: None },
        "snapshot={snapshot:?}, runtime={:?}",
        eng.ex
            .query(
                "SELECT sql FROM duckdb_tables() WHERE table_name = 'target_rows'",
                &BindMap::new()
            )
            .unwrap()
            .fetch_many(1)
    );
    let rows = eng
        .ex
        .query(
            "SELECT id, label FROM target_rows ORDER BY id NULLS LAST",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10);
    assert_eq!(
        rows,
        vec![
            vec![SpinValue::Int(1), SpinValue::Str("ready".into())],
            vec![SpinValue::Null, SpinValue::Str("ready".into())]
        ]
    );
}

#[cfg(feature = "native")]
#[test]
fn should_invalidate_candidate_when_public_catalog_is_replaced() {
    use spinduck_core::ir::set_oriented::{
        compile_set_oriented_plans, SetOrientedMode, SkipReason, Status,
    };
    use spinduck_core::{BindMap, DuckExecutor, Engine, SpinValue};
    let snapshot = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE source_rows(id BIGINT); CREATE TABLE target_rows(id BIGINT);"),
        &RuleRegistry::default(),
    );
    let original = "CREATE PROCEDURE copy_rows IS BEGIN FOR r IN SELECT id FROM source_rows LOOP \
        INSERT INTO target_rows(id) VALUES(r.id); END LOOP; END;";
    let old_catalog = catalog(original);
    let plans =
        compile_set_oriented_plans(&old_catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.catalog = old_catalog;
    eng.set_oriented = SetOrientedMode::Strict;
    eng.attach_set_oriented_plans(plans);
    for ddl in [
        "CREATE TABLE source_rows(id BIGINT)",
        "CREATE TABLE target_rows(id BIGINT)",
        "INSERT INTO source_rows VALUES (1)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.call("copy_rows", &[]).unwrap();
    let replacement = catalog(&original.replace("VALUES(r.id)", "VALUES(r.id + 10)"));
    assert_eq!(eng.catalog.len(), replacement.len());
    eng.catalog = replacement;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedWrite
        }
    );
    let rows = eng
        .ex
        .query("SELECT id FROM target_rows ORDER BY id", &BindMap::new())
        .unwrap()
        .fetch_many(10);
    assert_eq!(
        rows,
        vec![vec![SpinValue::Int(1)], vec![SpinValue::Int(11)]]
    );
}

#[test]
fn should_compile_package_catalog_and_candidates_before_opening_a_database() {
    use spinduck_core::catalog::compile_sources;
    use spinduck_core::ir::set_oriented::{compile_set_oriented_plans, SetOrientedMode};
    let source = "CREATE OR REPLACE PACKAGE BODY pack_copy IS \
        PROCEDURE copy_rows IS BEGIN FOR r IN SELECT id FROM source_rows LOOP \
        INSERT INTO target_rows(id) VALUES(r.id); END LOOP; END; \
        FUNCTION get_number(p_i IN NUMBER) RETURN NUMBER IS BEGIN RETURN p_i; END; END;";
    let compiled = compile_sources(&[source], RuleRegistry::default()).unwrap();
    assert_eq!(
        compiled.catalog.names(),
        vec!["pack_copy.copy_rows", "pack_copy.get_number"]
    );
    assert_eq!(
        compiled
            .catalog
            .lookup("pack_copy.get_number")
            .unwrap()
            .params[0]
            .name,
        "p_i"
    );
    let snapshot = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE source_rows(id BIGINT); CREATE TABLE target_rows(id BIGINT);"),
        compiled.catalog.sql_rules(),
    );
    let plans =
        compile_set_oriented_plans(&compiled.catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    assert_eq!(plans.get("pack_copy.copy_rows").unwrap().as_ref().unwrap().sql(),
        "WITH sd_cur AS (SELECT \"id\" FROM \"source_rows\") INSERT INTO \"target_rows\" (\"id\") SELECT sd_cur.\"id\" FROM sd_cur");
}

#[cfg(feature = "native")]
#[test]
fn should_reuse_offline_postfix_cast_defaults_and_native_unparameterized_numeric() {
    use spinduck_core::ir::set_oriented::{compile_set_oriented_plans, SetOrientedMode, Status};
    use spinduck_core::{BindMap, DuckExecutor, Engine, SpinValue};
    let ddl = "CREATE TABLE source_rows(id NUMERIC); CREATE TABLE target_rows(id NUMERIC, label VARCHAR DEFAULT 'ready'::character varying NOT NULL);";
    let snapshot = SchemaSnapshot::from_codeweb(&export(ddl), &RuleRegistry::default());
    assert_eq!(
        snapshot.table(&["source_rows"]).unwrap().columns[0].canonical_type,
        Some(SpinType::Decimal {
            precision: 18,
            scale: 3
        })
    );
    let catalog = catalog("CREATE PROCEDURE copy_rows IS BEGIN FOR r IN SELECT id FROM source_rows LOOP INSERT INTO target_rows(id) VALUES(r.id); END LOOP; END;");
    let plans = compile_set_oriented_plans(&catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    let sql = plans
        .get("copy_rows")
        .unwrap()
        .as_ref()
        .unwrap()
        .sql()
        .to_string();
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.catalog = catalog;
    eng.set_oriented = SetOrientedMode::Strict;
    eng.attach_set_oriented_plans(plans);
    eng.ex.execute(ddl, &BindMap::new()).unwrap();
    eng.ex
        .execute(
            "CREATE TABLE sys_dummy AS SELECT 1; INSERT INTO source_rows VALUES (1.234), (NULL)",
            &BindMap::new(),
        )
        .unwrap();
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { sql, arm: None }
    );
    assert_eq!(
        eng.ex
            .query(
                "SELECT id, label FROM target_rows ORDER BY id NULLS LAST",
                &BindMap::new()
            )
            .unwrap()
            .fetch_many(10),
        vec![
            vec![
                SpinValue::Decimal(1234, 38, 3),
                SpinValue::Str("ready".into())
            ],
            vec![SpinValue::Null, SpinValue::Str("ready".into())]
        ]
    );
}

#[cfg(feature = "native")]
#[test]
fn should_handoff_offline_catalog_with_ordered_macros_and_unchanged_sql_dispatch() {
    use spinduck_core::catalog::compile_sources;
    use spinduck_core::{BindMap, DuckExecutor, Engine, SpinValue};
    let sources = [
        "CREATE FUNCTION double_it(p IN NUMBER) RETURN NUMBER IS BEGIN RETURN p * 2; END;",
        "CREATE FUNCTION quadruple_it(p IN NUMBER) RETURN NUMBER IS BEGIN RETURN double_it(p) * 2; END;",
        "CREATE PROCEDURE copy_rows IS BEGIN INSERT INTO target_rows SELECT quadruple_it(id) FROM source_rows; END;",
    ];
    let compiled = compile_sources(&sources, RuleRegistry::default()).unwrap();
    let mut legacy = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for source in sources {
        legacy.register(source).unwrap();
    }
    assert_eq!(
        format!("{:?}", compiled.catalog.lookup("copy_rows").unwrap().prog),
        format!("{:?}", legacy.catalog.lookup("copy_rows").unwrap().prog)
    );
    let mut eng =
        Engine::from_compiled(Box::new(DuckExecutor::open_in_memory().unwrap()), compiled).unwrap();
    for engine in [&mut legacy, &mut eng] {
        for sql in [
            "CREATE TABLE source_rows(id BIGINT)",
            "CREATE TABLE target_rows(id BIGINT)",
            "INSERT INTO source_rows VALUES (7), (NULL)",
        ] {
            engine.ex.execute(sql, &BindMap::new()).unwrap();
        }
        engine.call("copy_rows", &[]).unwrap();
        assert_eq!(
            engine
                .ex
                .query(
                    "SELECT id FROM target_rows ORDER BY id NULLS LAST",
                    &BindMap::new()
                )
                .unwrap()
                .fetch_many(10),
            vec![vec![SpinValue::Int(28)], vec![SpinValue::Null]]
        );
    }
}

#[test]
fn should_change_fingerprint_when_a_unique_index_is_added() {
    let rules = RuleRegistry::default();
    let base = SchemaSnapshot::from_codeweb(&export("CREATE TABLE items(id BIGINT);"), &rules);
    let indexed = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE items(id BIGINT); CREATE UNIQUE INDEX ux ON items(id);"),
        &rules,
    );
    assert_ne!(base.fingerprint, indexed.fingerprint);
    assert!(indexed
        .table(&["items"])
        .unwrap()
        .indexes
        .iter()
        .any(|i| i.unique));
}

#[test]
fn should_carry_incomplete_ddl_flags_and_diagnostics_into_the_snapshot() {
    let rules = RuleRegistry::default();
    let snapshot = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE items(id BIGINT); ALTER TABLE items ADD CONSTRAINT uq UNIQUE(id);"),
        &rules,
    );
    assert!(!snapshot.diagnostics.is_empty());
    let table = snapshot.table(&["items"]).unwrap();
    assert!(table.has_opaque_ddl && !table.indexes_complete);
}

#[test]
fn should_normalize_varchar_and_numeric_without_duckdb_and_ignore_varchar_length() {
    let rules = RuleRegistry::default();
    let a = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE items(id VARCHAR(24), n NUMERIC(10,2));"),
        &rules,
    );
    let b = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE items(id VARCHAR(8), n DECIMAL(10,2));"),
        &rules,
    );
    let table = a.table(&["items"]).unwrap();
    assert_eq!(table.columns[0].canonical_type, Some(SpinType::Varchar));
    assert_eq!(
        table.columns[1].canonical_type,
        Some(SpinType::Decimal {
            precision: 10,
            scale: 2
        })
    );
    assert_eq!(a.fingerprint, b.fingerprint);
}

#[path = "guard/set_oriented_metadata_drift.rs"]
mod drift;

#[cfg(feature = "native")]
#[test]
fn should_replan_live_when_offline_static_candidate_is_unproven() {
    // 离线 Static 快照没有执行器：verbatim 游标（聚合/GROUP BY）的
    // DESCRIBE 列探测、视图定义与对象清单一律不可证 → 候选恒为
    // Skipped。挂计划后运行期必须以 Runtime 源重证（与 --file 路径
    // 同一证明器）：证明成功则 Converted，而不是沿用离线保守结论。
    use spinduck_core::ir::set_oriented::{compile_set_oriented_plans, SetOrientedMode, Status};
    use spinduck_core::{BindMap, DuckExecutor, Engine};
    let rules = RuleRegistry::default();
    let snapshot = SchemaSnapshot::from_codeweb(
        &export("CREATE TABLE src(id BIGINT); CREATE TABLE dst(id BIGINT, c BIGINT);"),
        &rules,
    );
    let catalog = catalog(
        "CREATE PROCEDURE agg_copy IS BEGIN \
        FOR r IN (SELECT id, COUNT(*) AS c FROM src GROUP BY id) LOOP \
        INSERT INTO dst(id, c) VALUES(r.id, r.c); END LOOP; END;",
    );
    let plans =
        compile_set_oriented_plans(&catalog, &snapshot, SetOrientedMode::BatchAtomic).unwrap();
    assert!(
        plans.get("agg_copy").unwrap().is_err(),
        "verbatim 游标在 Static 快照下必须保守跳过"
    );
    let mut eng = Engine::new(Box::new(DuckExecutor::open_in_memory().unwrap()), rules);
    eng.catalog = catalog;
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.attach_set_oriented_plans(plans);
    eng.attach_schema_snapshot(snapshot);
    for ddl in [
        "CREATE TABLE src(id BIGINT)",
        "CREATE TABLE dst(id BIGINT, c BIGINT)",
        "INSERT INTO src VALUES (1),(1),(2)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.call("agg_copy", &[]).unwrap();
    let reports = eng.set_oriented_reports();
    assert!(
        matches!(reports[0].status, Status::Converted { .. }),
        "运行期重证必须 Converted: {reports:?}"
    );
    let rows = eng
        .ex
        .query("SELECT * FROM dst ORDER BY id", &BindMap::new())
        .unwrap()
        .fetch_many(10);
    assert_eq!(rows.len(), 2, "dst 应有两组聚合行: {rows:?}");
}
