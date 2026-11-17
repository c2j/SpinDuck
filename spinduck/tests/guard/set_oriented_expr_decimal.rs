//! DECIMAL 写入转换与 CASE 参数绑定守护。

use super::{engine, rows, seed, BindMap, Engine, SetOrientedMode, SpinValue, Status};

fn case_amount_engine(nullable: bool) -> Engine {
    let mut eng = engine(&format!(
        "CREATE TABLE target_rows(val DECIMAL(15,3){})",
        if nullable { "" } else { " NOT NULL" }
    ));
    for sql in [
        "ALTER TABLE source_rows ALTER COLUMN a TYPE DECIMAL(15,2)",
        "ALTER TABLE source_rows ALTER COLUMN b TYPE DECIMAL(15,2)",
        "ALTER TABLE source_rows ADD COLUMN k BIGINT",
        "CREATE TABLE choices(k BIGINT, kind VARCHAR)",
        "INSERT INTO choices VALUES (1, 'A'), (2, 'B'), (3, NULL)",
        "INSERT INTO source_rows VALUES (2.50, 9999999999999.99, 1), \
         (9999999999999.99, -3.25, 2), (NULL, 1, 1), \
         (9999999999999.99, 1, 1), (9999999999999.99, NULL, 0), (1, 4.75, 3)",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE PROCEDURE copy_case_amount IS v_kind VARCHAR2(10); v NUMBER; BEGIN \
        FOR r IN SELECT a, b, k FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_kind FROM choices WHERE k = r.k; \
        SELECT CASE v_kind WHEN 'A' THEN r.a ELSE r.b END INTO v FROM sys_dummy; \
        EXCEPTION WHEN OTHERS THEN v_kind := '0'; v := 9; END; \
        BEGIN INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
    )
    .unwrap();
    eng
}

#[test]
fn should_guard_case_fields_with_different_decimal_scales() {
    let expected = vec![
        vec![SpinValue::Decimal(-3250, 38, 3)],
        vec![SpinValue::Decimal(2500, 38, 3)],
        vec![SpinValue::Decimal(4751, 38, 3)],
        vec![SpinValue::Decimal(9000, 38, 3)],
        vec![SpinValue::Null],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = case_amount_engine(true);
        for sql in [
            "ALTER TABLE source_rows ALTER COLUMN b TYPE DECIMAL(22,8)",
            "UPDATE source_rows SET b = -3.25123456 WHERE k = 2",
            "UPDATE source_rows SET a = NULL, b = 4.75123456 WHERE k = 3",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng.set_oriented = mode;
        eng.call("copy_case_amount", &[]).unwrap();
        let actual = eng
            .ex
            .query(
                "SELECT val FROM target_rows ORDER BY val NULLS LAST",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20);
        assert_eq!(actual, expected, "mode={mode:?}");
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
fn should_preserve_bound_case_rounding_before_target_rounding() {
    for (value, expected) in [
        ("-3.25600000", -3260),
        ("3.25500000", 3260),
        ("-3.25500000", -3260),
    ] {
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = case_amount_engine(true);
            eng.ex
                .execute(
                    "ALTER TABLE source_rows ALTER COLUMN b TYPE DECIMAL(22,8)",
                    &BindMap::new(),
                )
                .unwrap();
            eng.ex
                .execute(
                    &format!("UPDATE source_rows SET b = {value} WHERE k = 2"),
                    &BindMap::new(),
                )
                .unwrap();
            eng.set_oriented = mode;
            eng.call("copy_case_amount", &[]).unwrap();
            let actual = eng
                .ex
                .query(
                    "SELECT val FROM target_rows WHERE val NOT IN (2.500, 4.750, 9.000)",
                    &BindMap::new(),
                )
                .unwrap()
                .fetch_many(20);
            assert_eq!(
                actual,
                vec![vec![SpinValue::Decimal(expected, 38, 3)]],
                "mode={mode:?}, value={value}"
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
fn should_guard_only_selected_case_value_at_write_sink() {
    for nullable in [true, false] {
        let mut expected = vec![
            vec![SpinValue::Decimal(-3250, 38, 3)],
            vec![SpinValue::Decimal(2500, 38, 3)],
            vec![SpinValue::Decimal(4750, 38, 3)],
            vec![SpinValue::Decimal(9000, 38, 3)],
        ];
        if nullable {
            expected.push(vec![SpinValue::Null]);
        }
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = case_amount_engine(nullable);
            eng.set_oriented = mode;
            eng.call("copy_case_amount", &[]).unwrap();
            let actual = eng
                .ex
                .query(
                    "SELECT val FROM target_rows ORDER BY val NULLS LAST",
                    &BindMap::new(),
                )
                .unwrap()
                .fetch_many(20);
            assert_eq!(actual, expected, "mode={mode:?}, nullable={nullable}");
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

fn lookup_divisor_engine(projection: &str) -> Engine {
    let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
    for sql in [
        "ALTER TABLE source_rows ALTER COLUMN a TYPE DECIMAL(15,3)",
        "ALTER TABLE source_rows ALTER COLUMN b TYPE DECIMAL(15,3)",
        "ALTER TABLE source_rows ADD COLUMN k BIGINT",
        "CREATE TABLE durations(k BIGINT, n DECIMAL(3,0))",
        "INSERT INTO durations VALUES (1, 2), (2, 0), (3, NULL), (5, 1), (5, 2)",
        "INSERT INTO source_rows VALUES (10.125, 2, 1), (NULL, 1, 2), \
         (10, 2, 2), (10, 2, 3), (10, 2, 4), (10, 2, 5)",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(&format!(
        "CREATE PROCEDURE copy_lookup_divisor IS v NUMBER; BEGIN \
        FOR r IN SELECT a, b, k FROM source_rows LOOP \
        BEGIN SELECT {projection} INTO v FROM durations WHERE k = r.k; \
        INSERT INTO target_rows(val) VALUES (round((r.a - r.b) / v, 2)); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;"
    ))
    .unwrap();
    eng
}

#[test]
fn should_conjoin_nullable_column_guards_without_or_leakage() {
    let expected = vec![
        vec![SpinValue::Int(1), SpinValue::Decimal(7000, 38, 3)],
        vec![SpinValue::Int(2), SpinValue::Null],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine("CREATE TABLE target_rows(id BIGINT NOT NULL, val DECIMAL(15,3))");
        seed(&eng, "(1, 7), (NULL, 8), (2, NULL)");
        eng.register(
            "CREATE PROCEDURE copy_guards IS BEGIN \
            FOR r IN SELECT a, b FROM source_rows LOOP BEGIN \
            INSERT INTO target_rows(id, val) VALUES (r.a, r.b); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_guards", &[]).unwrap();
        let actual = eng
            .ex
            .query(
                "SELECT id, val FROM target_rows ORDER BY id",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(10);
        assert_eq!(actual, expected, "mode={mode:?}");
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
fn should_guard_lookup_value_narrowing_at_final_write() {
    for nullable in [true, false] {
        let mut expected = vec![
            vec![SpinValue::Decimal(0, 38, 0)],
            vec![SpinValue::Decimal(0, 38, 0)],
            vec![SpinValue::Decimal(2, 38, 0)],
        ];
        if nullable {
            expected.push(vec![SpinValue::Null]);
        }
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = lookup_divisor_engine("n");
            for sql in [
                "ALTER TABLE target_rows ALTER COLUMN val TYPE DECIMAL(2,0)",
                "INSERT INTO durations VALUES (6, 100)",
                "INSERT INTO source_rows VALUES (1, 1, 6)",
            ] {
                eng.ex.execute(sql, &BindMap::new()).unwrap();
            }
            if !nullable {
                eng.ex
                    .execute(
                        "ALTER TABLE target_rows ALTER COLUMN val SET NOT NULL",
                        &BindMap::new(),
                    )
                    .unwrap();
            }
            eng.register(
                "CREATE PROCEDURE copy_lookup_value IS v NUMBER; BEGIN \
                FOR r IN SELECT a, b, k FROM source_rows LOOP BEGIN \
                SELECT n INTO v FROM durations WHERE k = r.k; \
                INSERT INTO target_rows(val) VALUES (v); COMMIT; \
                EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
            )
            .unwrap();
            eng.set_oriented = mode;
            eng.call("copy_lookup_value", &[]).unwrap();
            let actual = eng
                .ex
                .query(
                    "SELECT val FROM target_rows ORDER BY val NULLS LAST",
                    &BindMap::new(),
                )
                .unwrap()
                .fetch_many(20);
            assert_eq!(actual, expected, "mode={mode:?}, nullable={nullable}");
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
fn should_use_lookup_divisor_without_losing_skip_or_null_semantics() {
    for projection in ["n", "nvl(n, 1)"] {
        let mut expected = vec![vec![SpinValue::Decimal(4060, 38, 3)]];
        if projection == "n" {
            expected.push(vec![SpinValue::Null]);
        } else {
            expected.push(vec![SpinValue::Decimal(8000, 38, 3)]);
        }
        expected.push(vec![SpinValue::Null]);
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            let mut eng = lookup_divisor_engine(projection);
            eng.set_oriented = mode;
            eng.call("copy_lookup_divisor", &[]).unwrap();
            let actual = eng
                .ex
                .query(
                    "SELECT val FROM target_rows ORDER BY val NULLS LAST",
                    &BindMap::new(),
                )
                .unwrap()
                .fetch_many(20);
            assert_eq!(actual, expected, "mode={mode:?}, projection={projection}");
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
fn should_guard_same_scale_decimal_difference_before_native_division() {
    let expected = vec![
        vec![SpinValue::Decimal(4060, 38, 3)],
        vec![SpinValue::Null],
        vec![SpinValue::Decimal(-2250, 38, 3)],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
        for sql in [
            "ALTER TABLE source_rows ALTER COLUMN a TYPE DECIMAL(15,3)",
            "ALTER TABLE source_rows ALTER COLUMN b TYPE DECIMAL(15,3)",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        seed(
            &eng,
            "(10.125, 2), (NULL, 1), \
            (999999999999.999, -999999999999.999), (-1.5, 3)",
        );
        eng.register(
            "CREATE PROCEDURE copy_difference IS BEGIN \
            FOR r IN SELECT a, b FROM source_rows LOOP \
            BEGIN INSERT INTO target_rows(val) VALUES (round((r.a - r.b) / 2, 2)); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_difference", &[]).unwrap();
        assert_eq!(rows(&eng), expected, "mode={mode:?}");
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
fn should_guard_decimal_abs_at_write_sink() {
    let expected = vec![
        vec![SpinValue::Decimal(3, 38, 0)],
        vec![SpinValue::Null],
        vec![SpinValue::Decimal(999999999999999, 38, 0)],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,0))");
        eng.ex
            .execute(
                "ALTER TABLE source_rows ALTER COLUMN a TYPE DECIMAL(18,3)",
                &BindMap::new(),
            )
            .unwrap();
        seed(
            &eng,
            "(-2.500, 0), (NULL, 0), (-999999999999999.999, 0), (-999999999999999.499, 0)",
        );
        eng.register(
            "CREATE PROCEDURE copy_abs IS BEGIN \
            FOR r IN SELECT a FROM source_rows LOOP \
            BEGIN INSERT INTO target_rows(val) VALUES (abs(r.a)); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_abs", &[]).unwrap();
        assert_eq!(rows(&eng), expected, "mode={mode:?}");
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
fn should_guard_decimal_field_rescaling_and_not_null_at_write_sink() {
    let sql = "CREATE PROCEDURE copy_amount IS BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.a); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;";
    for nullable in [true, false] {
        let setup = || {
            let mut eng = engine(&format!(
                "CREATE TABLE target_rows(val DECIMAL(15,3){})",
                if nullable { "" } else { " NOT NULL" }
            ));
            eng.ex
                .execute(
                    "ALTER TABLE source_rows ALTER COLUMN a TYPE DECIMAL(15,2)",
                    &BindMap::new(),
                )
                .unwrap();
            seed(
                &eng,
                "(2.50, 0), (9999999999999.99, 0), (NULL, 0), (-3.25, 0)",
            );
            eng.register(sql).unwrap();
            eng
        };
        let mut baseline = setup();
        baseline.call("copy_amount", &[]).unwrap();
        let mut expected = vec![vec![SpinValue::Decimal(2500, 38, 3)]];
        if nullable {
            expected.push(vec![SpinValue::Null]);
        }
        expected.push(vec![SpinValue::Decimal(-3250, 38, 3)]);
        assert_eq!(rows(&baseline), expected);
        let mut eng = setup();
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_amount", &[]).unwrap();
        assert_eq!(rows(&eng), expected);
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
