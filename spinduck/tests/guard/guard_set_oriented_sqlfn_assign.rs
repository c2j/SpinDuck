//! 直接 SqlFn 赋值准入（§5 步骤 5 的 v_repurchase_date 形态最小化）：
//! `v_x := to_char(to_date(r.d, 'YYYYMMDD'), 'YYYY-MM-DD')` 不经注册函数
//! 包装，直接以 IrExpr::SqlFn 赋值。准入后走纯函数内联投影（prepared-aware
//! 渲染 + 入口 TRY_ 全定义守卫）。区别于 FuncCall 内联（f_getday 包装）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const PROC: &str = "CREATE PROCEDURE copy_rows IS v_x VARCHAR2(40); BEGIN \
    FOR r IN SELECT id, d FROM source_rows LOOP \
    BEGIN \
    v_x := to_char(to_date(r.d, 'YYYYMMDD'), 'YYYY-MM-DD'); \
    INSERT INTO target_rows(id, s) VALUES (r.id, v_x); \
    EXCEPTION WHEN OTHERS THEN NULL; END; \
    END LOOP; END;";

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, d VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, s VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn seed(eng: &Engine, rows: &[(i64, &str)]) {
    for (id, d) in rows {
        eng.ex
            .execute(
                &format!("INSERT INTO source_rows VALUES ({id}, '{d}')"),
                &BindMap::new(),
            )
            .unwrap();
    }
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, s FROM target_rows ORDER BY id NULLS LAST, s",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

#[test]
fn should_exclude_prior_skip_lookup_misses_from_inline_date_guard() {
    let setup = || {
        let mut eng = engine();
        seed(&eng, &[(1, "20240101"), (2, "bad")]);
        eng.ex
            .execute("CREATE TABLE dim(id BIGINT, n BIGINT)", &BindMap::new())
            .unwrap();
        eng.ex
            .execute("INSERT INTO dim VALUES (1,7)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_reachable IS v_n NUMBER; v_s VARCHAR2; BEGIN \
            FOR r IN SELECT id, d FROM source_rows LOOP BEGIN \
            SELECT n INTO v_n FROM dim WHERE id = r.id; \
            v_s := to_char(to_date(r.d, 'YYYYMMDD'), 'YYYY-MM-DD'); \
            INSERT INTO target_rows(id,s) VALUES(v_n,v_s); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng
    };
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = setup();
        eng.set_oriented = mode;
        eng.call("copy_reachable", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Int(7), SpinValue::Str("2024-01-01".into())]]
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
fn should_filter_prior_skip_lookup_even_when_its_value_is_not_inserted() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine();
        seed(&eng, &[(1, "20240101"), (2, "bad")]);
        eng.ex
            .execute("CREATE TABLE dim(id BIGINT, n BIGINT)", &BindMap::new())
            .unwrap();
        eng.ex
            .execute("INSERT INTO dim VALUES (1,7)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_unused IS v_n NUMBER; v_s VARCHAR2; BEGIN \
            FOR r IN SELECT id, d FROM source_rows LOOP BEGIN \
            SELECT n INTO v_n FROM dim WHERE id = r.id; \
            v_s := to_char(to_date(r.d, 'YYYYMMDD'), 'YYYY-MM-DD'); \
            INSERT INTO target_rows(id,s) VALUES(r.id,v_s); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_unused", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Int(1), SpinValue::Str("2024-01-01".into())]],
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
fn should_not_hide_bare_date_failure_behind_a_later_skip_lookup() {
    let setup = || {
        let mut eng = engine();
        seed(&eng, &[(1, "bad")]);
        eng.ex
            .execute("CREATE TABLE dim(id BIGINT, n BIGINT)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_order IS v_n NUMBER; v_s VARCHAR2; BEGIN \
            FOR r IN SELECT id, d FROM source_rows LOOP \
            v_s := to_char(to_date(r.d, 'YYYYMMDD'), 'YYYY-MM-DD'); BEGIN \
            SELECT n INTO v_n FROM dim WHERE id = r.id; \
            INSERT INTO target_rows(id,s) VALUES(v_n,v_s); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng
    };
    let mut off = setup();
    let expected = off.call("copy_order", &[]).unwrap_err();
    assert_eq!(expected.code.0, "KERNEL-0010");
    assert!(expected.message.contains("bad"), "{}", expected.message);
    let mut batch = setup();
    batch.set_oriented = SetOrientedMode::BatchAtomic;
    let actual = batch.call("copy_order", &[]).unwrap_err();
    assert_eq!(actual.code, expected.code);
    assert_eq!(actual.message, expected.message);
    assert_eq!(rows(&batch), Vec::<Vec<SpinValue>>::new());
    assert_eq!(
        batch.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: spinduck_core::ir::set_oriented::SkipReason::UnprovenQueryTotalness,
        }
    );
}

#[test]
fn should_downshift_direct_sqlfn_date_assignment_into_batch_insert() {
    // 全部源行为合法日期：直接 SqlFn 赋值准入 → 内联投影渲染 → Converted，
    // 输出与 Off 逐字一致。
    let mut baseline = engine();
    baseline.register(PROC).unwrap();
    seed(&baseline, &[(1, "20240101"), (2, "20240215")]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(
        want,
        vec![
            vec![SpinValue::Int(1), SpinValue::Str("2024-01-01".into())],
            vec![SpinValue::Int(2), SpinValue::Str("2024-02-15".into())],
        ]
    );

    let mut eng = engine();
    eng.register(PROC).unwrap();
    seed(&eng, &[(1, "20240101"), (2, "20240215")]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
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
fn should_preserve_string_zero_fallback_in_date_arithmetic() {
    let expected = vec![
        vec![SpinValue::Int(1), SpinValue::Str("2024-01-01".into())],
        vec![SpinValue::Int(2), SpinValue::Str("2024-01-03".into())],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine();
        seed(&eng, &[(1, "20240101"), (2, "20240101")]);
        eng.ex
            .execute(
                "CREATE TABLE offsets(id BIGINT, days BIGINT); INSERT INTO offsets VALUES (2, 2)",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_zero IS v_days NUMBER; v_s VARCHAR2; BEGIN \
             FOR r IN SELECT id, d FROM source_rows LOOP \
             BEGIN SELECT days INTO v_days FROM offsets WHERE id = r.id; \
             EXCEPTION WHEN OTHERS THEN v_days := '0'; END; \
             v_s := to_char(to_date(r.d, 'YYYYMMDD') + v_days, 'YYYY-MM-DD'); \
             BEGIN INSERT INTO target_rows(id, s) VALUES (r.id, v_s); COMMIT; \
             EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_zero", &[]).unwrap();
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

fn date_divisor_engine() -> Engine {
    let mut eng = engine();
    for sql in [
        "ALTER TABLE source_rows ADD COLUMN e VARCHAR",
        "ALTER TABLE source_rows ADD COLUMN amount DECIMAL(15,3)",
        "ALTER TABLE target_rows ADD COLUMN val DECIMAL(15,3)",
        "INSERT INTO source_rows VALUES \
         (1, '20240110', '20240101', 18), \
         (2, '20240301', '20240228', 10), \
         (3, '20240101', '20240110', 18), \
         (4, '20240101', '20240101', 18), \
         (5, '20240101', '20240101', NULL)",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE FUNCTION duration_days(a VARCHAR2, b VARCHAR2) RETURN INTEGER AS \
        BEGIN RETURN INTERVALTONUM(to_date(a, 'YYYYMMDD') - to_date(b, 'YYYYMMDD')); END;",
    )
    .unwrap();
    eng.register(
        "CREATE PROCEDURE copy_date_divisor IS v INTEGER; BEGIN \
        FOR r IN SELECT id, d, e, amount FROM source_rows LOOP BEGIN \
        v := duration_days(r.d, r.e); \
        INSERT INTO target_rows(id, val) VALUES (r.id, round(r.amount / v, 2)); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
    )
    .unwrap();
    eng
}

#[test]
fn should_inline_date_difference_as_numeric_divisor() {
    let expected = vec![
        vec![SpinValue::Int(1), SpinValue::Decimal(2000, 38, 3)],
        vec![SpinValue::Int(2), SpinValue::Decimal(5000, 38, 3)],
        vec![SpinValue::Int(3), SpinValue::Decimal(-2000, 38, 3)],
        vec![SpinValue::Int(5), SpinValue::Null],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = date_divisor_engine();
        eng.set_oriented = mode;
        eng.call("copy_date_divisor", &[]).unwrap();
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
fn should_preserve_date_divisor_null_and_invalid_input_through_fallback() {
    let expected = vec![
        vec![SpinValue::Int(1), SpinValue::Null],
        vec![SpinValue::Int(3), SpinValue::Decimal(-2000, 38, 3)],
        vec![SpinValue::Int(5), SpinValue::Null],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = date_divisor_engine();
        eng.ex
            .execute(
                "UPDATE source_rows SET d = NULL WHERE id = 1",
                &BindMap::new(),
            )
            .unwrap();
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'not-a-date' WHERE id = 2",
                &BindMap::new(),
            )
            .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_date_divisor", &[]).unwrap();
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
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: spinduck_core::ir::set_oriented::SkipReason::UnprovenQueryTotalness,
                }
            );
        }
    }
}

#[test]
fn should_bind_lookup_parameters_in_inline_entry_guard() {
    let expected = vec![
        vec![SpinValue::Int(1), SpinValue::Str("20240108".into())],
        vec![SpinValue::Int(2), SpinValue::Str("20240216".into())],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine();
        seed(&eng, &[(1, "20240101"), (2, "20240215")]);
        for sql in [
            "CREATE TABLE durations(id BIGINT, n INTEGER, beg VARCHAR, endd VARCHAR)",
            "INSERT INTO durations VALUES (1, 7, '20240101', '20241231'), (2, NULL, '20240101', '20241231')",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng.register(
            "CREATE PROCEDURE copy_guard(p_d VARCHAR2) IS v_n INTEGER; v_s VARCHAR2; BEGIN \
            FOR r IN SELECT id, d FROM source_rows LOOP BEGIN \
            SELECT nvl(n, 1) INTO v_n FROM durations WHERE id = r.id AND p_d BETWEEN beg AND endd; \
            v_s := to_char(to_date(r.d, 'YYYYMMDD') + v_n, 'YYYYMMDD'); \
            INSERT INTO target_rows(id, s) VALUES (r.id, v_s); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_guard", &[SpinValue::Str("20240715".into())])
            .unwrap();
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
fn should_fall_back_when_entry_try_guard_finds_undateable_row() {
    // 含非法日期行：入口 TRY_STRPTIME 全定义守卫保守预筛失败 → 无损退回
    // 原区域解释执行（Skipped），数据与 Off 逐字一致（Off 的异常屏障会
    // 回滚同事务内先前未提交行，故以 Off 实测输出为基线，不硬编码行集）。
    let mut baseline = engine();
    baseline.register(PROC).unwrap();
    seed(
        &baseline,
        &[(1, "20240101"), (2, "notadate"), (3, "20240215")],
    );
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);

    let mut eng = engine();
    eng.register(PROC).unwrap();
    seed(&eng, &[(1, "20240101"), (2, "notadate"), (3, "20240215")]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(eng.set_oriented_reports()[0].status, Status::Skipped { .. }),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}
