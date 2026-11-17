//! CASE 摘要保留条件求值与分支可达性。

use super::*;

#[test]
fn should_evaluate_a_simple_case_when_value_before_comparing_it() {
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
             RETURN CASE p WHEN to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD') THEN p ELSE p END; END;",
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
fn should_check_case_conditions_without_evaluating_unreachable_results() {
    for (condition, result, fails) in [
        ("to_date(p, 'YYYYMMDD') IS NULL", "p", true),
        (
            "p = 'never'",
            "to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD')",
            false,
        ),
    ] {
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = conditional_engine("r.a > 15");
            eng.ex
                .execute(
                    "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                    &BindMap::new(),
                )
                .unwrap();
            eng.register(&format!(
                "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS BEGIN \
                 RETURN CASE WHEN {condition} THEN {result} ELSE p END; END;",
            ))
            .unwrap();
            eng.set_oriented = mode;
            eng.call("copy_cond_call", &[]).unwrap();
            let mut expected = vec![vec![SpinValue::Str("20260101".into())]; 2];
            if !fails {
                expected.push(vec![SpinValue::Str("bad".into())]);
            }
            assert_eq!(rows(&eng), expected, "{mode:?}, {condition}");
            if mode == SetOrientedMode::BatchAtomic {
                if fails {
                    assert_eq!(
                        eng.set_oriented_reports()[0].status,
                        Status::Skipped {
                            reason: SkipReason::UnprovenQueryTotalness,
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
