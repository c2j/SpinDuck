//! 集合化 modes 语义守护。

use super::*;

#[test]
fn should_convert_cursor_filter_with_substring_call() {
    use spinduck_core::ir::set_oriented::Status;
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id, payload FROM source_rows \
        WHERE substring(payload, 1, 1) = 'a' LOOP \
        INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
        END LOOP; END;";
    let setup = |eng: &Engine| {
        for s in [
            "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
            "CREATE TABLE target_rows(id BIGINT, payload VARCHAR)",
            "INSERT INTO source_rows VALUES (1, 'a1'), (2, 'b2'), (3, 'a3')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(s, &BindMap::new()).unwrap();
        }
    };
    let mut baseline = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    setup(&baseline);
    baseline.register(sql).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    setup(&eng);
    eng.register(sql).unwrap();
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
    assert_eq!(rows(&eng), rows(&baseline));
    assert_eq!(
        rows(&eng),
        vec![
            vec![SpinValue::Int(1), SpinValue::Str("a1".into())],
            vec![SpinValue::Int(3), SpinValue::Str("a3".into())],
        ]
    );
}

#[test]
fn should_preserve_partial_durability_of_loop_commits() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let sql = COPY.replace("END LOOP", "COMMIT; END LOOP");
    for enabled in [false, true] {
        let mut eng =
            engine_with_target("CREATE TABLE target_rows(id BIGINT PRIMARY KEY, payload VARCHAR)");
        eng.register(&sql).unwrap();
        eng.set_oriented = enabled.into();
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
    eng.set_oriented = SetOrientedMode::Strict;
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
    eng.set_oriented = SetOrientedMode::Strict;
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

#[test]
fn should_select_exactly_one_execution_path_for_each_switch_combination() {
    use spinduck_core::exec::TraceEvent;
    use spinduck_core::ir::set_oriented::Status;
    for (set, batch) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut eng = engine();
        eng.register(COPY).unwrap();
        eng.set_oriented = set.into();
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
        } else {
            // §2.2-3：batch_commit 不再互斥，set 为真即 Converted
            // （Strict/BatchAtomic 同断言），Fetch 归零。
            let Status::Converted { sql } = &eng.set_oriented_reports()[0].status else {
                panic!("expected conversion: {:?}", eng.set_oriented_reports());
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
        assert_eq!(fetches, if set { 0 } else { 4 });
    }
}

#[test]
fn should_not_change_off_percent_type_coercion_semantics() {
    let mut eng = engine();
    for (name, ty, expected) in [
        ("anchor_slot", "target_rows.payload%TYPE", SpinValue::Int(0)),
        ("varchar_slot", "VARCHAR2(40)", SpinValue::Int(1)),
    ] {
        eng.register(&format!(
            "CREATE PROCEDURE {name}(result OUT NUMBER) IS v {ty}; \
             BEGIN v := 21; IF v < '3' THEN result := 1; ELSE result := 0; END IF; END;"
        ))
        .unwrap();
        let before = format!("{:?}", eng.catalog.lookup(name).unwrap().prog);
        let result = eng.call(name, &[SpinValue::Null]).unwrap();
        assert_eq!(result.outs.get("result"), Some(&expected));
        assert_eq!(eng.set_oriented_reports(), &[]);
        assert_eq!(
            format!("{:?}", eng.catalog.lookup(name).unwrap().prog),
            before
        );
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

#[test]
fn should_fold_tail_commit_in_batch_atomic_mode() {
    use spinduck_core::ir::set_oriented::Status;
    let sql = COPY.replace("END LOOP", "COMMIT; END LOOP");
    let mut eng = engine();
    eng.register(&sql).unwrap();
    let before = format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
    assert_eq!(
        format!("{:?}", eng.catalog.lookup("copy_rows").unwrap().prog),
        before
    );
}

#[test]
fn should_reject_batch_atomic_with_dirty_prefix() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        DELETE FROM target_rows; \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
        COMMIT; END LOOP; END;";
    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::TransactionBoundaryInsideLoop
        }
    );
}

#[test]
fn should_convert_batch_atomic_with_clean_prefix() {
    use spinduck_core::ir::set_oriented::Status;
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        DELETE FROM target_rows; COMMIT; \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
        COMMIT; END LOOP; END;";
    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_convert_batch_atomic_with_skip_trap_body() {
    use spinduck_core::ir::set_oriented::Status;
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}
