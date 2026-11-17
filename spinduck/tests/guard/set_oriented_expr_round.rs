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
    const ROUND_DIV: &str = "CREATE PROCEDURE copy_round IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP BEGIN \
        INSERT INTO target_rows(val) VALUES (round(r.a / r.b, 2)); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;";

    // DECIMAL 分支：期望静态，Off 与 BatchAtomic 各跑一遍逐字对齐。
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
        seed(&eng, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
        eng.register(ROUND_DIV).unwrap();
        eng.set_oriented = mode;
        eng.call("copy_round", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Decimal(2000, 38, 3)], vec![SpinValue::Null]],
            "{mode:?} DECIMAL(15,3)"
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

    // VARCHAR 分支：0/0 的 nan/-nan 拼法随平台不同。Off 先跑做形状检查，
    // BatchAtomic 必须与同次运行的 Off 逐行一致（含 NaN 符号）。
    let mut off = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&off, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
    off.register(ROUND_DIV).unwrap();
    off.call("copy_round", &[]).unwrap();
    let expected = rows(&off);
    assert_varchar_div_rows(&expected);

    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
    eng.register(ROUND_DIV).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_round", &[]).unwrap();
    assert_eq!(rows(&eng), expected, "BatchAtomic VARCHAR");
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()
    );
}
