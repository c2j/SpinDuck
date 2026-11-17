//! 集合化 schema 语义守护。

use super::*;

#[test]
fn should_convert_proven_static_insert_loop() {
    let mut eng = engine();
    eng.register(COPY).unwrap();
    let before = format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog);
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    let report = &eng.set_oriented_reports()[0];
    assert_eq!(report.routine, "copy_rows");
    assert!(
        matches!(
            report.status,
            spinduck_core::ir::set_oriented::Status::Converted { .. }
        ),
        "{report:?}"
    );
    assert_eq!(
        format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog),
        before
    );
}

#[test]
fn should_reject_unique_indexes_not_present_in_table_ddl() {
    let mut eng = engine();
    eng.ex
        .execute(
            "CREATE UNIQUE INDEX target_key ON target_rows(id)",
            &BindMap::new(),
        )
        .unwrap();
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    let error = eng.call("copy_rows", &[]).unwrap_err();
    assert_eq!(error.code.0, "KERNEL-0010");
    assert!(
        error.message.to_ascii_lowercase().contains("duplicate key"),
        "{}",
        error.message
    );
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        spinduck_core::ir::set_oriented::Status::Skipped {
            reason: spinduck_core::ir::set_oriented::SkipReason::UnprovenTargetConstraints,
        }
    );
    assert_eq!(rows(&eng), Vec::<Vec<SpinValue>>::new());
}

#[test]
fn should_preserve_output_parameter_aliasing_cursor_record() {
    let sql = COPY.replace("copy_rows IS", "copy_rows(r OUT RECORD) IS");
    let mut original = engine();
    original.register(&sql).unwrap();
    let expected_result = original.call("copy_rows", &[SpinValue::Null]).unwrap();
    let mut optimized = engine();
    optimized.register(&sql).unwrap();
    optimized.set_oriented = SetOrientedMode::Strict;
    let result = optimized.call("copy_rows", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs, expected_result.outs);
    assert_eq!(rows(&optimized), expected());
    assert_eq!(
        optimized.set_oriented_reports()[0].status,
        spinduck_core::ir::set_oriented::Status::Skipped {
            reason: spinduck_core::ir::set_oriented::SkipReason::LiveOutState,
        }
    );
}

#[test]
fn should_reprove_schema_and_clear_reports_when_switching_off() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let mut eng = engine();
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    // 触发重新证明的_schema 变更_：把 id 改为 INTEGER（源 BIGINT → 目标 INTEGER
    // 类型不一致 → UnprovenTargetConstraints）。原 fixture 用 SET DEFAULT，但
    // 全列 INSERT 不触发默认值，设计 §4.1 A3 准入后该变更不再退回（默认无关），
    // 故改用仍合法触发退回的类型漂移，保留“每次调用重新证明”的守护意图。
    eng.ex
        .execute(
            "ALTER TABLE target_rows ALTER COLUMN id TYPE INTEGER",
            &BindMap::new(),
        )
        .unwrap();
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenTargetConstraints
        }
    );
    eng.set_oriented = SetOrientedMode::Off;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(eng.set_oriented_reports(), &[]);
    let mut want: Vec<_> = (0..3).flat_map(|_| expected()).collect();
    want.sort_by_key(|r| if r[0] == SpinValue::Null { 1 } else { 0 });
    assert_eq!(rows(&eng), want);
}

#[test]
fn should_fall_back_with_identical_errors_for_unproven_target_schemas() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    // 注：`payload VARCHAR DEFAULT 'x'`（全列 INSERT，默认不触发）与
    // `extra BIGINT`（省略可空列）经设计 §4.1 A3 准入后改为转换，分别由
    // should_convert_when_target_omits_constant_default_column /
    // should_convert_when_target_omits_nullable_column 守护，不再属此拒绝集。
    for ddl in [
        "CREATE TABLE target_rows(id BIGINT NOT NULL, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT PRIMARY KEY, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT CHECK(id > 0), payload VARCHAR)",
        "CREATE TABLE target_rows(id INTEGER, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, payload VARCHAR COLLATE NOCASE)",
    ] {
        let mut baseline = engine_with_target(ddl);
        baseline.register(COPY).unwrap();
        let expected_result = baseline
            .call("copy_rows", &[])
            .map(|r| r.outs)
            .map_err(|e| (e.code, e.message));
        let mut eng = engine_with_target(ddl);
        eng.register(COPY).unwrap();
        eng.set_oriented = SetOrientedMode::Strict;
        let result = eng
            .call("copy_rows", &[])
            .map(|r| r.outs)
            .map_err(|e| (e.code, e.message));
        assert_eq!(result, expected_result, "{ddl}");
        assert_eq!(rows(&eng), rows(&baseline), "{ddl}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnprovenTargetConstraints
            },
            "{ddl}"
        );
    }
}

#[test]
fn should_convert_when_target_omits_constant_default_column() {
    use spinduck_core::ir::set_oriented::Status;
    let setup = |eng: &Engine| {
        for sql in [
            "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
            "CREATE TABLE target_rows(id BIGINT, payload VARCHAR, \
             extra DECIMAL(15,3) DEFAULT 0 NOT NULL)",
            "INSERT INTO source_rows VALUES (7, 'a'), (8, 'b')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
    };
    let mut baseline = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    setup(&baseline);
    baseline.register(COPY).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    setup(&eng);
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
    let q = "SELECT id, payload, extra FROM target_rows ORDER BY id";
    let got = eng.ex.query(q, &BindMap::new()).unwrap().fetch_many(10);
    let want = baseline
        .ex
        .query(q, &BindMap::new())
        .unwrap()
        .fetch_many(10);
    assert_eq!(got, want);
}

#[test]
fn should_convert_when_source_table_has_default_columns() {
    use spinduck_core::ir::set_oriented::Status;
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR DEFAULT 'x' NOT NULL)",
        "CREATE TABLE target_rows(id BIGINT, payload VARCHAR)",
        "INSERT INTO source_rows(id) VALUES (7), (8)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
    assert_eq!(
        rows(&eng),
        vec![
            vec![SpinValue::Int(7), SpinValue::Str("x".into())],
            vec![SpinValue::Int(8), SpinValue::Str("x".into())],
        ]
    );
}

#[test]
fn should_convert_when_target_omits_nullable_column() {
    use spinduck_core::ir::set_oriented::Status;
    let mut eng =
        engine_with_target("CREATE TABLE target_rows(id BIGINT, payload VARCHAR, extra BIGINT)");
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
    assert_eq!(rows(&eng), expected());
    let extra = eng
        .ex
        .query(
            "SELECT extra FROM target_rows ORDER BY id NULLS LAST",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10);
    assert_eq!(extra, vec![vec![SpinValue::Null]; 3]);
}

#[test]
fn should_skip_when_target_omits_non_deterministic_default_column() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let ddl = "CREATE TABLE target_rows(id BIGINT, payload VARCHAR, extra DOUBLE DEFAULT random())";
    let mut baseline = engine_with_target(ddl);
    baseline.register(COPY).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let mut eng = engine_with_target(ddl);
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::NonDeterministicDefault
        }
    );
    assert_eq!(rows(&eng), rows(&baseline));
}
