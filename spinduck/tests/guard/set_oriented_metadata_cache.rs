//! 离线完整候选的运行库漂移与快照重挂载契约。
#![cfg(feature = "native")]

use super::export;
use spinduck_core::catalog::compile_sources;
use spinduck_core::ir::set_oriented::{
    compile_set_oriented_plans, metadata::SchemaSnapshot, SetOrientedMode, SkipReason, Status,
};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const DDL: &str = "CREATE TABLE source_rows(id BIGINT, payload VARCHAR(24)); \
    CREATE TABLE target_rows(id BIGINT, payload VARCHAR(8));";
const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;";

fn offline_engine() -> (Engine, String, SchemaSnapshot) {
    let rules = RuleRegistry::default();
    let snapshot = SchemaSnapshot::from_codeweb(&export(DDL), &rules);
    let compiled = compile_sources(&[COPY], rules).unwrap();
    let plans =
        compile_set_oriented_plans(&compiled.catalog, &snapshot, SetOrientedMode::Strict).unwrap();
    let sql = plans
        .get("copy_rows")
        .unwrap()
        .as_ref()
        .unwrap()
        .sql()
        .to_string();
    let mut engine =
        Engine::from_compiled(Box::new(DuckExecutor::open_in_memory().unwrap()), compiled).unwrap();
    engine.attach_set_oriented_plans(plans);
    engine.set_oriented = SetOrientedMode::Strict;
    engine.ex.execute(DDL, &BindMap::new()).unwrap();
    engine
        .ex
        .execute(
            "CREATE TABLE sys_dummy AS SELECT 1; \
         INSERT INTO source_rows VALUES (7, 'a'), (7, 'a'), (NULL, NULL)",
            &BindMap::new(),
        )
        .unwrap();
    (engine, sql, snapshot)
}

fn rows(engine: &Engine) -> Vec<Vec<SpinValue>> {
    engine
        .ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id NULLS LAST, payload",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_recheck_indexes_and_columns_after_reusing_a_complete_offline_candidate() {
    for drift in [
        "CREATE UNIQUE INDEX target_key ON target_rows(id)",
        "ALTER TABLE target_rows ADD COLUMN extra BIGINT",
    ] {
        let (mut eng, sql, _) = offline_engine();
        let (mut baseline, _, _) = offline_engine();
        baseline.set_oriented = SetOrientedMode::Off;
        for engine in [&mut eng, &mut baseline] {
            engine.call("copy_rows", &[]).unwrap();
        }
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { sql, arm: None }
        );
        assert_eq!(
            rows(&eng),
            vec![
                vec![SpinValue::Int(7), SpinValue::Str("a".into())],
                vec![SpinValue::Int(7), SpinValue::Str("a".into())],
                vec![SpinValue::Null, SpinValue::Null],
            ]
        );
        for engine in [&mut eng, &mut baseline] {
            engine
                .ex
                .execute("DELETE FROM target_rows", &BindMap::new())
                .unwrap();
            engine.ex.execute(drift, &BindMap::new()).unwrap();
        }
        let expected = baseline
            .call("copy_rows", &[])
            .map(|r| (r.outs, r.return_value))
            .map_err(|e| (e.domain, e.code, e.message));
        let actual = eng
            .call("copy_rows", &[])
            .map(|r| (r.outs, r.return_value))
            .map_err(|e| (e.domain, e.code, e.message));
        assert_eq!(actual, expected, "{drift}");
        assert_eq!(rows(&eng), rows(&baseline), "{drift}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnprovenTargetConstraints,
            }
        );
    }
}

#[test]
fn should_fall_back_when_sys_dummy_exists_only_in_another_schema() {
    let mut expected = None;
    for mode in [SetOrientedMode::Off, SetOrientedMode::Strict] {
        let (mut eng, _, _) = offline_engine();
        eng.set_oriented = mode;
        eng.ex
            .execute(
                "DROP TABLE sys_dummy; CREATE SCHEMA elsewhere; \
                 CREATE TABLE elsewhere.sys_dummy AS SELECT 1 AS dummy",
                &BindMap::new(),
            )
            .unwrap();
        let result = eng.call("copy_rows", &[]).unwrap();
        let actual = (rows(&eng), result.outs, result.return_value);
        if mode == SetOrientedMode::Off {
            assert_eq!(
                actual.0,
                vec![
                    vec![SpinValue::Int(7), SpinValue::Str("a".into())],
                    vec![SpinValue::Int(7), SpinValue::Str("a".into())],
                    vec![SpinValue::Null, SpinValue::Null],
                ]
            );
            expected = Some(actual);
        } else {
            assert_eq!(Some(actual), expected);
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenSysDummyInvariant,
                }
            );
        }
    }
}

#[test]
fn should_fall_back_without_evaluating_an_unproven_sys_dummy_view() {
    let mut expected = None;
    for mode in [SetOrientedMode::Off, SetOrientedMode::Strict] {
        let (mut eng, _, _) = offline_engine();
        eng.set_oriented = mode;
        eng.ex
            .execute(
                "DROP TABLE sys_dummy; CREATE TABLE dummy_input(value VARCHAR); \
                 INSERT INTO dummy_input VALUES ('bad'); \
                 CREATE VIEW sys_dummy AS SELECT 1 AS dummy FROM dummy_input \
                 WHERE CAST(value AS INTEGER) > 0",
                &BindMap::new(),
            )
            .unwrap();
        let result = eng.call("copy_rows", &[]).unwrap();
        let actual = (rows(&eng), result.outs, result.return_value);
        if mode == SetOrientedMode::Off {
            assert_eq!(
                actual.0,
                vec![
                    vec![SpinValue::Int(7), SpinValue::Str("a".into())],
                    vec![SpinValue::Int(7), SpinValue::Str("a".into())],
                    vec![SpinValue::Null, SpinValue::Null],
                ]
            );
            expected = Some(actual);
        } else {
            assert_eq!(Some(actual), expected);
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenSysDummyInvariant,
                }
            );
        }
    }
}

#[test]
fn should_invalidate_replaced_column_evidence_even_with_the_same_fingerprint() {
    let (mut eng, sql, mut snapshot) = offline_engine();
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { sql, arm: None }
    );
    let fingerprint = snapshot.fingerprint.clone();
    let target = snapshot
        .tables
        .iter_mut()
        .find(|t| t.name == ["target_rows"])
        .unwrap();
    target.columns[0].nullable = false;
    assert_eq!(snapshot.fingerprint, fingerprint);
    eng.attach_schema_snapshot(snapshot);
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenTargetConstraints,
        }
    );
    assert_eq!(
        rows(&eng),
        vec![vec![SpinValue::Int(7), SpinValue::Str("a".into())]; 4]
            .into_iter()
            .chain(vec![vec![SpinValue::Null, SpinValue::Null]; 2])
            .collect::<Vec<_>>()
    );
}
