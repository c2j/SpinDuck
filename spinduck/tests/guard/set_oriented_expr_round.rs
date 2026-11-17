//! ROUND 的 NULL、无穷值与目标转换边界守护。

use super::*;

#[test]
fn should_preserve_literal_null_div_bad_inside_round() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
        seed(&eng, "(1, 1)");
        eng.register(
            "CREATE PROCEDURE copy_null IS BEGIN \
            FOR r IN SELECT a, b FROM source_rows LOOP BEGIN \
            INSERT INTO target_rows(val) VALUES (round(NULL::BIGINT / 'bad', 2)); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_null", &[]).unwrap();
        assert_eq!(rows(&eng), vec![vec![SpinValue::Null]], "{mode:?}");
        if mode == SetOrientedMode::BatchAtomic {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: super::SkipReason::UnprovenWriteCompleteness,
                }
            );
        }
    }
}

#[test]
fn should_round_inf_nan_before_applying_exact_target_conversion() {
    for (target, expected) in [
        (
            "DECIMAL(15,3)",
            vec![vec![SpinValue::Decimal(2000, 38, 3)], vec![SpinValue::Null]],
        ),
        (
            "VARCHAR",
            vec![
                vec![SpinValue::Str("inf".into())],
                vec![SpinValue::Str("nan".into())],
                vec![SpinValue::Str("2.0".into())],
                vec![SpinValue::Null],
            ],
        ),
    ] {
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = engine(&format!("CREATE TABLE target_rows(val {target})"));
            seed(&eng, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
            eng.register(
                "CREATE PROCEDURE copy_round IS BEGIN \
                FOR r IN SELECT a, b FROM source_rows LOOP BEGIN \
                INSERT INTO target_rows(val) VALUES (round(r.a / r.b, 2)); COMMIT; \
                EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
            )
            .unwrap();
            eng.set_oriented = mode;
            eng.call("copy_round", &[]).unwrap();
            assert_eq!(rows(&eng), expected, "{mode:?} {target}");
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
