//! 结构快照的运行期索引漂移守护。

#![cfg(feature = "native")]

use super::*;
use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, SpinValue};

const DDL: &str = "CREATE TABLE source_rows(id BIGINT, payload VARCHAR(24)); \
     CREATE TABLE target_rows(id BIGINT, payload VARCHAR(8));";
const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
    END LOOP; END;";

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id NULLS LAST, payload",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        DDL,
        "INSERT INTO source_rows VALUES (7, 'a'), (7, 'a'), (NULL, NULL)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        for stmt in sql.split(';') {
            if !stmt.trim().is_empty() {
                eng.ex.execute(stmt, &BindMap::new()).unwrap();
            }
        }
    }
    eng
}

#[test]
fn should_compile_without_duckdb_using_codeweb_metadata_and_detect_runtime_index_drift() {
    let expected = vec![
        vec![SpinValue::Int(7), SpinValue::Str("a".into())],
        vec![SpinValue::Int(7), SpinValue::Str("a".into())],
        vec![SpinValue::Null, SpinValue::Null],
    ];

    // 编译期：零 DuckDB，仅凭 codeweb 元数据生成快照（VARCHAR 宽度已归一）
    let rules = RuleRegistry::default();
    let snapshot = SchemaSnapshot::from_codeweb(&export(DDL), &rules);
    assert!(snapshot.diagnostics.is_empty());

    // 宽度差异（24 vs 8）不构成漂移：归一化后必须照常转换
    let mut eng = engine();
    eng.attach_schema_snapshot(snapshot.clone());
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));

    // 运行期结构漂移：指纹拦截无损退回，结果/错误与 Off 基线逐字一致
    for drift in [
        "CREATE UNIQUE INDEX target_key ON target_rows(id)",
        "ALTER TABLE target_rows ADD COLUMN extra BIGINT",
    ] {
        let mut baseline = engine();
        baseline.register(COPY).unwrap();
        baseline.ex.execute(drift, &BindMap::new()).unwrap();
        let want = baseline
            .call("copy_rows", &[])
            .map(|r| r.outs)
            .map_err(|e| (e.code, e.message));
        let mut eng = engine();
        eng.attach_schema_snapshot(snapshot.clone());
        eng.register(COPY).unwrap();
        eng.set_oriented = SetOrientedMode::Strict;
        eng.ex.execute(drift, &BindMap::new()).unwrap();
        let got = eng
            .call("copy_rows", &[])
            .map(|r| r.outs)
            .map_err(|e| (e.code, e.message));
        assert_eq!(got, want, "{drift}");
        assert_eq!(rows(&eng), rows(&baseline), "{drift}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnprovenTargetConstraints
            },
            "{drift}"
        );
    }
}
