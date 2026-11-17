//! 纯函数调用边界的槽位与形参代入守护。

use super::*;

#[test]
fn should_preserve_local_varchar_slot_comparison_inside_pure_functions() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.register(
            "CREATE FUNCTION f_local(p NUMBER) RETURN VARCHAR2 IS v VARCHAR2; \
             BEGIN v := p; RETURN CASE WHEN v < '3' THEN 'yes' ELSE 'no' END; END;",
        )
        .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_local IS v VARCHAR2; BEGIN \
             FOR r IN SELECT a, d FROM source_rows LOOP BEGIN v := f_local(r.a); \
             INSERT INTO target_rows(val) VALUES (v); COMMIT; \
             EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_local", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![
                vec![SpinValue::Str("no".into())],
                vec![SpinValue::Str("yes".into())],
                vec![SpinValue::Str("yes".into())]
            ],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnsupportedCall
                }
            );
        }
    }
}

#[test]
fn should_accept_string_substring_arguments_in_nested_pure_calls() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.register("CREATE FUNCTION f_second(p VARCHAR2, d VARCHAR2) RETURN VARCHAR2 IS BEGIN RETURN d; END;").unwrap();
        eng.register("CREATE FUNCTION f_outer(p VARCHAR2) RETURN VARCHAR2 IS BEGIN RETURN f_second(substr(p, 1, 3), p); END;").unwrap();
        eng.register(
            "CREATE PROCEDURE copy_substring IS v VARCHAR2; BEGIN \
             FOR r IN SELECT a, d FROM source_rows LOOP BEGIN v := f_outer(r.d); \
             INSERT INTO target_rows(val) VALUES (v); COMMIT; \
             EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_substring", &[]).unwrap();
        assert_eq!(rows(&eng), vec![vec![SpinValue::Str("20260101".into())]; 3]);
        if mode == SetOrientedMode::BatchAtomic {
            assert!(
                matches!(
                    eng.set_oriented_reports()[0].status,
                    Status::Converted { .. }
                ),
                "{:?}",
                eng.set_oriented_reports()
            );
        }
    }
}

#[test]
fn should_preserve_varchar_parameter_comparison_for_numeric_cursor_arguments() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.register(
            "CREATE FUNCTION f_compare(p VARCHAR2) RETURN VARCHAR2 IS \
             BEGIN RETURN CASE WHEN p < '3' THEN 'yes' ELSE 'no' END; END;",
        )
        .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_comparison IS v VARCHAR2; BEGIN \
             FOR r IN SELECT a, d FROM source_rows LOOP BEGIN v := f_compare(r.a); \
             INSERT INTO target_rows(val) VALUES (v); COMMIT; \
             EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_comparison", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![
                vec![SpinValue::Str("no".into())],
                vec![SpinValue::Str("yes".into())],
                vec![SpinValue::Str("yes".into())]
            ],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnsupportedCall
                }
            );
        }
    }
}

#[test]
fn should_substitute_arguments_simultaneously_without_capturing_caller_names() {
    for call in ["f_join(b, 'Z')", "f_outer(b)"] {
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = conditional_engine("r.a > 15");
            eng.register(
                "CREATE FUNCTION f_join(a VARCHAR2, b VARCHAR2) RETURN VARCHAR2 IS \
                 BEGIN RETURN a || b; END;",
            )
            .unwrap();
            eng.register(
                "CREATE FUNCTION f_outer(p VARCHAR2) RETURN VARCHAR2 IS \
                 BEGIN RETURN f_join(p, 'Z'); END;",
            )
            .unwrap();
            eng.register(&format!(
                "CREATE PROCEDURE copy_names IS b VARCHAR2; v VARCHAR2; BEGIN \
                 FOR r IN SELECT a, d FROM source_rows LOOP BEGIN \
                 b := f_next(r.d, 1); v := {call}; \
                 INSERT INTO target_rows(val) VALUES (v); COMMIT; \
                 EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;"
            ))
            .unwrap();
            eng.set_oriented = mode;
            eng.call("copy_names", &[]).unwrap();
            assert_eq!(
                rows(&eng),
                vec![vec![SpinValue::Str("20260102Z".into())]; 3],
                "{call} {mode:?}"
            );
            if mode == SetOrientedMode::BatchAtomic {
                assert!(
                    matches!(
                        eng.set_oriented_reports()[0].status,
                        Status::Converted { .. }
                    ),
                    "{:?}",
                    eng.set_oriented_reports()
                );
            }
        }
    }
}

#[test]
fn should_not_fold_constant_call_past_varchar_parameter_installation_error() {
    let setup = || {
        let mut eng = engine("0");
        eng.rules.implicit_assignment_loose = false;
        eng.catalog.set_sql_rules(eng.rules.clone());
        eng.register(
            "CREATE PROCEDURE copy_argument IS v NUMBER; BEGIN \
             FOR r IN SELECT a FROM source_rows LOOP v := f_flag(21); \
             IF v = 1 THEN INSERT INTO target_rows(val) VALUES (r.a + 1000); \
             ELSE INSERT INTO target_rows(val) VALUES (r.a); END IF; END LOOP; END;",
        )
        .unwrap();
        eng
    };
    let mut off = setup();
    let expected = off.call("copy_argument", &[]).unwrap_err();
    assert_eq!(expected.code.0, "BIZ-0111");
    assert!(expected.message.contains("cannot assign 21 to VARCHAR"));
    assert!(rows(&off).is_empty());
    let mut batch = setup();
    batch.set_oriented = SetOrientedMode::BatchAtomic;
    let actual = batch.call("copy_argument", &[]).unwrap_err();
    assert_eq!(actual.code, expected.code);
    assert_eq!(actual.domain, expected.domain);
    assert_eq!(actual.message, expected.message);
    assert!(rows(&batch).is_empty());
    assert_eq!(
        batch.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedCall
        }
    );
}
