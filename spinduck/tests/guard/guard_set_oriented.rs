//! 静态游标集合化的逐行基线与等价守护。
#![cfg(feature = "native")]

use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
    END LOOP; END;";

fn engine() -> Engine {
    engine_with_target("CREATE TABLE target_rows(id BIGINT, payload VARCHAR)")
}

fn engine_with_target(target: &str) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
        target,
        "INSERT INTO source_rows VALUES (7, 'a'), (7, 'a'), (NULL, NULL)",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id NULLS LAST, payload",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

fn expected() -> Vec<Vec<SpinValue>> {
    vec![
        vec![SpinValue::Int(7), SpinValue::Str("a".into())],
        vec![SpinValue::Int(7), SpinValue::Str("a".into())],
        vec![SpinValue::Null, SpinValue::Null],
    ]
}

#[test]
fn should_convert_proven_static_insert_loop() {
    let mut eng = engine();
    eng.register(COPY).unwrap();
    let before = format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog);
    eng.set_oriented = true;
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
    eng.set_oriented = true;
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
    optimized.set_oriented = true;
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
    eng.set_oriented = true;
    eng.call("copy_rows", &[]).unwrap();
    eng.ex
        .execute(
            "ALTER TABLE target_rows ALTER COLUMN payload SET DEFAULT 'unused'",
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
    eng.set_oriented = false;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(eng.set_oriented_reports(), &[]);
    let mut want: Vec<_> = (0..3).flat_map(|_| expected()).collect();
    want.sort_by_key(|r| if r[0] == SpinValue::Null { 1 } else { 0 });
    assert_eq!(rows(&eng), want);
}

#[test]
fn should_fall_back_with_identical_errors_for_unproven_target_schemas() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    for ddl in [
        "CREATE TABLE target_rows(id BIGINT NOT NULL, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT PRIMARY KEY, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT CHECK(id > 0), payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, payload VARCHAR DEFAULT 'x')",
        "CREATE TABLE target_rows(id INTEGER, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, payload VARCHAR, extra BIGINT)",
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
        eng.set_oriented = true;
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
fn should_preserve_partial_durability_of_loop_commits() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let sql = COPY.replace("END LOOP", "COMMIT; END LOOP");
    for enabled in [false, true] {
        let mut eng =
            engine_with_target("CREATE TABLE target_rows(id BIGINT PRIMARY KEY, payload VARCHAR)");
        eng.register(&sql).unwrap();
        eng.set_oriented = enabled;
        let error = eng.call("copy_rows", &[]).unwrap_err();
        assert_eq!(error.code.0, "KERNEL-0010");
        assert!(error.message.to_ascii_lowercase().contains("duplicate key"));
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Int(7), SpinValue::Str("a".into())]]
        );
        if enabled {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::TransactionBoundaryInsideLoop
                }
            );
        }
    }
}

#[test]
fn should_reject_exception_scope_even_when_current_data_does_not_throw() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let mut eng = engine();
    let sql = COPY.replace(
        "END LOOP; END;",
        "END LOOP; EXCEPTION WHEN OTHERS THEN NULL; END;",
    );
    eng.register(&sql).unwrap();
    eng.set_oriented = true;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::ExceptionScopePresent
        }
    );
}

#[test]
fn should_preserve_empty_cursor_and_isolate_cte_name_from_physical_table() {
    use spinduck_core::ir::set_oriented::Status;
    let mut eng = engine();
    eng.ex
        .execute("ALTER TABLE source_rows RENAME TO sd_cur", &BindMap::new())
        .unwrap();
    eng.register(&COPY.replace("source_rows", "sd_cur"))
        .unwrap();
    eng.set_oriented = true;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));
    eng.ex
        .execute("DELETE FROM sd_cur", &BindMap::new())
        .unwrap();
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));
}

#[derive(Default)]
struct Trace(Vec<spinduck_core::exec::TraceEvent>);

impl spinduck_core::exec::TraceSink for Trace {
    fn on_event(&mut self, event: spinduck_core::exec::TraceEvent) {
        self.0.push(event);
    }
}

#[test]
fn should_select_exactly_one_execution_path_for_each_switch_combination() {
    use spinduck_core::exec::TraceEvent;
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    for (set, batch) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut eng = engine();
        eng.register(COPY).unwrap();
        eng.set_oriented = set;
        eng.batch_commit = batch;
        let mut trace = Trace::default();
        eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
        assert_eq!(rows(&eng), expected(), "set={set}, batch={batch}");
        let fetches = trace
            .0
            .iter()
            .filter(|e| matches!(e, TraceEvent::Fetch { .. }))
            .count();
        if !set {
            assert_eq!(eng.set_oriented_reports(), &[]);
        } else if batch {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::BatchCommitEnabled
                }
            );
        } else {
            let Status::Converted { sql } = &eng.set_oriented_reports()[0].status else {
                panic!("expected conversion");
            };
            let writes: Vec<_> = trace
                .0
                .iter()
                .filter_map(|e| match e {
                    TraceEvent::SqlExec { sql, binds, .. } => Some((sql, binds)),
                    _ => None,
                })
                .collect();
            assert_eq!(writes, vec![(sql, &BindMap::new())]);
        }
        assert_eq!(fetches, if set && !batch { 0 } else { 4 });
    }
}

#[test]
fn should_preserve_original_cursor_copy_duplicates_and_nulls() {
    let mut eng = engine();
    eng.register(COPY).unwrap();
    let before = format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog);
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert_eq!(
        format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog),
        before
    );
}
