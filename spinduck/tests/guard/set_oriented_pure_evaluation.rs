//! 函数摘要不得因覆盖或内联而消去先前求值的错误。

use super::*;

#[test]
fn should_preserve_boolean_operand_errors_inside_a_comparison() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             IF (p = 'bad' OR to_date(p, 'YYYYMMDD') IS NULL) = TRUE THEN NULL; END IF; RETURN p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness,
                }
            );
        }
    }
}

#[test]
fn should_preserve_date_error_in_a_concat_operand() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             RETURN to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD') || p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("2026010120260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness,
                }
            );
        }
    }
}

#[test]
fn should_preserve_nested_boolean_errors_under_not() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             IF NOT (p = 'bad' OR to_date(p, 'YYYYMMDD') IS NULL) THEN NULL; END IF; RETURN p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness,
                }
            );
        }
    }
}

#[test]
fn should_evaluate_both_pl_boolean_operands_before_discarding_the_condition() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             IF p = 'bad' OR to_date(p, 'YYYYMMDD') IS NULL THEN NULL; END IF; RETURN p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}

#[test]
fn should_preserve_date_error_inside_a_discarded_is_null_condition() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             IF to_date(p, 'YYYYMMDD') IS NULL THEN NULL; END IF; RETURN p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}

#[test]
fn should_evaluate_a_then_assignment_even_when_its_slot_is_never_read() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS v VARCHAR2; BEGIN \
             IF p = 'bad' THEN v := to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD'); END IF; \
             RETURN p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}

#[test]
fn should_bind_lookup_arguments_from_the_preceding_callee_local_environment() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = range_lookup_engine();
        eng.register(
            "CREATE FUNCTION f_range(p_k VARCHAR2, p_day VARCHAR2) RETURN NUMBER IS \
             local_key VARCHAR2; local_day VARCHAR2; v NUMBER; BEGIN \
             local_key := p_k; local_day := p_day; \
             BEGIN SELECT val INTO v FROM ranges WHERE k = local_key AND local_day BETWEEN lo AND hi; \
             EXCEPTION WHEN OTHERS THEN v := 0; END; RETURN v; END;",
        ).unwrap();
        eng.set_oriented = mode;
        eng.call("copy_range", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![
                vec![SpinValue::Int(0)],
                vec![SpinValue::Int(0)],
                vec![SpinValue::Int(0)],
                vec![SpinValue::Int(7)],
                vec![SpinValue::Int(7)],
                vec![SpinValue::Null],
            ],
            "{mode:?}"
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

#[test]
fn should_evaluate_unused_nested_arguments_before_entering_the_callee() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register("CREATE FUNCTION f_second(p VARCHAR2, d VARCHAR2) RETURN VARCHAR2 IS BEGIN RETURN d; END;").unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             RETURN f_second(to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD'), p); END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}

#[test]
fn should_preserve_fallible_condition_even_when_then_has_no_assignment() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
             IF to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD') = '20260101' THEN NULL; END IF; \
             RETURN p; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}

#[test]
fn should_preserve_overwritten_evaluation_only_inside_reachable_then() {
    for predicate in ["p = 'bad'", "p = 'never'"] {
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = conditional_engine("r.a > 15");
            eng.ex
                .execute(
                    "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                    &BindMap::new(),
                )
                .unwrap();
            eng.register(&format!(
                "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS v VARCHAR2; \
                 BEGIN v := p; IF {predicate} THEN \
                 v := to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD'); v := p; END IF; RETURN v; END;",
            ))
            .unwrap();
            eng.set_oriented = mode;
            eng.call("copy_cond_call", &[]).unwrap();
            let mut expected = vec![vec![SpinValue::Str("20260101".into())]; 2];
            if predicate == "p = 'never'" {
                expected.push(vec![SpinValue::Str("bad".into())]);
            }
            assert_eq!(rows(&eng), expected, "{mode:?}, {predicate}");
            if mode == SetOrientedMode::BatchAtomic {
                if predicate == "p = 'bad'" {
                    assert_eq!(
                        eng.set_oriented_reports()[0].status,
                        Status::Skipped {
                            reason: SkipReason::UnprovenQueryTotalness
                        }
                    );
                } else {
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
}

#[test]
fn should_preserve_date_error_before_conditionally_overwriting_a_local() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS v VARCHAR2; \
             BEGIN v := to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD'); \
             IF p = 'bad' THEN v := p; END IF; RETURN v; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}

#[test]
fn should_preserve_date_error_before_overwriting_a_later_read_local() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS v VARCHAR2; \
             BEGIN v := to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD'); \
             v := p; RETURN v; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260101".into())]; 2],
            "{mode:?}"
        );
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenQueryTotalness
                }
            );
        }
    }
}
