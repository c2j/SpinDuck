//! PureExpr 常量折叠与静态死分支剪枝守护（V2 设计 §4.3 A7）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

#[path = "set_oriented_pure_case.rs"]
mod case;
#[path = "set_oriented_pure_evaluation.rs"]
mod evaluation;
#[path = "set_oriented_pure_slots.rs"]
mod slots;

fn engine(flag_body: &str) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT)",
        "CREATE TABLE target_rows(val BIGINT)",
        "INSERT INTO source_rows VALUES (10), (20)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(&format!(
        "CREATE FUNCTION f_flag(p_a VARCHAR2) RETURN NUMBER IS BEGIN RETURN {flag_body}; END;"
    ))
    .unwrap();
    eng.register(
        "CREATE PROCEDURE copy_flag IS v NUMBER; BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        v := f_flag('x'); \
        IF v = 1 THEN INSERT INTO target_rows(val) VALUES (r.a + 1000); \
        ELSE INSERT INTO target_rows(val) VALUES (r.a); \
        END IF; \
        END LOOP; END;",
    )
    .unwrap();
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query("SELECT val FROM target_rows ORDER BY val", &BindMap::new())
        .unwrap()
        .fetch_many(20)
}

fn range_lookup_engine() -> Engine {
    let mut eng = engine("0");
    for sql in [
        "CREATE TABLE range_rows(k VARCHAR, day VARCHAR)",
        "CREATE TABLE ranges(k VARCHAR, lo VARCHAR, hi VARCHAR, val BIGINT)",
        "INSERT INTO range_rows VALUES ('a','20250101'), ('a','20250131'), \
         ('a','20250201'), ('b','20250115'), ('c','20250115'), ('a',NULL)",
        "INSERT INTO ranges VALUES ('a','20250101','20250131',7), \
         ('b','20250101','20250131',NULL), ('c','20250101','20250131',8), \
         ('c','20250101','20250131',9)",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE FUNCTION f_range(p_k VARCHAR2, p_day VARCHAR2) RETURN NUMBER IS \
         v NUMBER; BEGIN BEGIN SELECT val INTO v FROM ranges \
         WHERE k = p_k AND p_day BETWEEN lo AND hi; \
         EXCEPTION WHEN OTHERS THEN v := 0; END; RETURN v; END;",
    )
    .unwrap();
    eng.register(
        "CREATE PROCEDURE copy_range IS v NUMBER; BEGIN \
         FOR r IN SELECT k, day FROM range_rows LOOP v := f_range(r.k, r.day); \
         BEGIN INSERT INTO target_rows(val) VALUES (v); COMMIT; \
         EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
    )
    .unwrap();
    eng
}

#[test]
fn should_keep_inline_fallback_group_separate_from_outer_skip_lookup() {
    let expected = vec![
        vec![SpinValue::Int(0), SpinValue::Int(1)],
        vec![SpinValue::Int(0), SpinValue::Int(1)],
        vec![SpinValue::Int(0), SpinValue::Int(3)],
        vec![SpinValue::Int(7), SpinValue::Int(1)],
        vec![SpinValue::Int(7), SpinValue::Int(1)],
        vec![SpinValue::Null, SpinValue::Int(2)],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = range_lookup_engine();
        for sql in [
            "CREATE TABLE outer_dim(k VARCHAR, val BIGINT)",
            "INSERT INTO outer_dim VALUES ('a', 1), ('b', 2), ('c', 3)",
            "ALTER TABLE target_rows ADD COLUMN aux BIGINT",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng.register(
            "CREATE PROCEDURE copy_isolated IS v NUMBER; v_aux NUMBER; BEGIN \
            FOR r IN SELECT k, day FROM range_rows LOOP BEGIN \
            SELECT val INTO v_aux FROM outer_dim WHERE k = r.k; \
            v := f_range(r.k, r.day); \
            INSERT INTO target_rows(val, aux) VALUES (v, v_aux); COMMIT; \
            EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_isolated", &[]).unwrap();
        let actual = eng
            .ex
            .query(
                "SELECT val, aux FROM target_rows ORDER BY val NULLS LAST, aux",
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
fn should_skip_inline_range_lookup_with_mixed_bound_types() {
    let setup = || {
        let eng = range_lookup_engine();
        eng.ex
            .execute(
                "ALTER TABLE ranges ALTER COLUMN hi TYPE BIGINT",
                &BindMap::new(),
            )
            .unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_range", &[]).unwrap();
    let expected = vec![vec![SpinValue::Int(0)]; 6];
    assert_eq!(rows(&baseline), expected);
    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_range", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenTargetConstraints,
        }
    );
}

#[test]
fn should_inline_cursor_date_between_lookup_bounds() {
    let mut baseline = range_lookup_engine();
    baseline.call("copy_range", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Int(0)],
        vec![SpinValue::Int(0)],
        vec![SpinValue::Int(0)],
        vec![SpinValue::Int(7)],
        vec![SpinValue::Int(7)],
        vec![SpinValue::Null],
    ];
    assert_eq!(rows(&baseline), expected);
    let mut eng = range_lookup_engine();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_range", &[]).unwrap();
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

#[test]
fn should_prune_static_dead_then_branch_after_pure_expr_constant_folding() {
    // f_flag 体为唯一 RETURN 0（PureExpr）：v := f_flag('x') 折叠为 0，
    // IF v = 1 静态不可达，THEN 剪枝无需入口守卫；ELSE 集合化与 Off 一致。
    let mut baseline = engine("0");
    baseline.call("copy_flag", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(expected.len(), 2);

    let mut eng = engine("0");
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_flag", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0].status
    );
}

fn conditional_engine(predicate: &str) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, d VARCHAR)",
        "CREATE TABLE target_rows(val VARCHAR)",
        "INSERT INTO source_rows VALUES (10, '20260101'), (20, '20260101'), (NULL, '20260101')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE FUNCTION f_next(p VARCHAR2, n NUMBER) RETURN VARCHAR2 IS v VARCHAR2; \
         BEGIN v := to_char(to_date(p, 'YYYYMMDD') + n, 'YYYYMMDD'); RETURN v; END;",
    )
    .unwrap();
    eng.register(&format!(
        "CREATE PROCEDURE copy_cond_call IS v VARCHAR2; BEGIN \
         FOR r IN SELECT a, d FROM source_rows LOOP BEGIN \
         v := f_next(r.d, 1); \
         IF {predicate} THEN v := f_next(r.d, 2); END IF; \
         INSERT INTO target_rows(val) VALUES (v); COMMIT; \
         EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;"
    ))
    .unwrap();
    eng
}

#[test]
fn should_inline_pure_call_in_reachable_conditional_reassignment() {
    let expected = vec![
        vec![SpinValue::Str("20260102".into())],
        vec![SpinValue::Str("20260102".into())],
        vec![SpinValue::Str("20260103".into())],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
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
fn should_use_prior_inline_value_in_conditional_predicate() {
    let expected = vec![
        vec![SpinValue::Str("20260103".into())],
        vec![SpinValue::Str("20260103".into())],
        vec![SpinValue::Str("20260111".into())],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("v = '20260102'");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = '20260110' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_cond_call", &[]).unwrap();
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
fn should_not_hide_prior_date_error_with_conditional_overwrite() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        eng.ex
            .execute(
                "UPDATE source_rows SET d = 'bad' WHERE a = 20",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_overwrite IS v VARCHAR2; BEGIN \
             FOR r IN SELECT a, d FROM source_rows LOOP BEGIN \
             v := f_next(r.d, 1); \
             IF r.a > 15 THEN v := f_next('20260101', 2); END IF; \
             INSERT INTO target_rows(val) VALUES (v); COMMIT; \
             EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_overwrite", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![vec![SpinValue::Str("20260102".into())]; 2],
            "mode={mode:?}"
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
fn should_preserve_earlier_consumers_and_self_arguments_when_reassigning() {
    let expected = vec![
        vec![
            SpinValue::Str("20260104".into()),
            SpinValue::Str("20260103".into()),
        ],
        vec![
            SpinValue::Str("20260104".into()),
            SpinValue::Str("20260103".into()),
        ],
        vec![
            SpinValue::Str("20260111".into()),
            SpinValue::Str("20260112".into()),
        ],
    ];
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = conditional_engine("r.a > 15");
        for sql in [
            "ALTER TABLE target_rows ADD COLUMN prior VARCHAR",
            "UPDATE source_rows SET d = '20260110' WHERE a = 20",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng.register(
            "CREATE PROCEDURE copy_prior IS v VARCHAR2; w VARCHAR2; BEGIN \
             FOR r IN SELECT a, d FROM source_rows LOOP BEGIN \
             v := f_next(r.d, 1); w := f_next(v, 1); \
             IF w = '20260103' THEN v := f_next(v, 2); END IF; \
             INSERT INTO target_rows(val, prior) VALUES (v, w); COMMIT; \
             EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        eng.call("copy_prior", &[]).unwrap();
        let actual = eng
            .ex
            .query(
                "SELECT val, prior FROM target_rows ORDER BY val",
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
fn should_not_fold_constant_return_past_a_discarded_fallible_assignment() {
    let setup = || {
        let mut eng = engine("0");
        eng.register(
            "CREATE FUNCTION f_flag(p_a VARCHAR2) RETURN NUMBER IS v NUMBER; \
            BEGIN v := 1 / 0; RETURN 0; END;",
        )
        .unwrap();
        eng
    };
    let mut off = setup();
    let expected = off.call("copy_flag", &[]).unwrap_err();
    assert_eq!(expected.code.0, "BIZ-0124");
    assert!(rows(&off).is_empty());
    let mut batch = setup();
    batch.set_oriented = SetOrientedMode::BatchAtomic;
    let actual = batch.call("copy_flag", &[]).unwrap_err();
    assert_eq!(actual.code, expected.code);
    assert_eq!(actual.domain, expected.domain);
    assert_eq!(actual.message, expected.message);
    assert!(rows(&batch).is_empty());
    assert_eq!(
        batch.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedCall,
        }
    );
}

#[test]
fn should_not_inline_nonconstant_return_past_a_discarded_date_error() {
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
            BEGIN v := to_char(to_date(p, 'YYYYMMDD'), 'YYYYMMDD'); RETURN p; END;",
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
                    reason: SkipReason::UnsupportedCall,
                }
            );
        }
    }
}

#[test]
fn should_skip_when_folded_constant_makes_then_branch_live() {
    // RETURN 1：IF v = 1 静态恒真，THEN 必然执行；仅覆盖 ELSE 的
    // 集合化计划不等价，必须整段退回解释执行。
    let mut baseline = engine("1");
    baseline.call("copy_flag", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(expected[0][0], SpinValue::Int(1010));

    let mut eng = engine("1");
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_flag", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        }
    );
}
