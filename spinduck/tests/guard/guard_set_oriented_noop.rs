//! 跨例程 NoOp 摘要守护（V2 设计 §4.3 A7 / §6.2）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        "CREATE TABLE target_rows(val DECIMAL(15,3) NOT NULL)",
        "INSERT INTO source_rows VALUES (6, 3), (NULL, 1)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query("SELECT val FROM target_rows ORDER BY val", &BindMap::new())
        .unwrap()
        .fetch_many(20)
}

const COPY_WITH_HANDLER: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT a, b FROM source_rows LOOP \
    BEGIN INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
    EXCEPTION WHEN OTHERS THEN p_log('skip'); END; \
    END LOOP; END;";

#[test]
fn should_eliminate_noop_handler_call_and_skip_bad_rows_in_batch_atomic() {
    // 空过程处理器调用无可观察效果（实参字面量可证 Str），跳行语义
    // 与列级守卫过滤坏行等价：BatchAtomic 必须 Converted 且与 Off 逐行一致。
    let mut baseline = engine();
    baseline
        .register("CREATE PROCEDURE p_log(p_msg VARCHAR2) AS BEGIN NULL; END;")
        .unwrap();
    baseline.register(COPY_WITH_HANDLER).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(expected, vec![vec![SpinValue::Decimal(2000, 38, 3)]]);

    let mut eng = engine();
    eng.register("CREATE PROCEDURE p_log(p_msg VARCHAR2) AS BEGIN NULL; END;")
        .unwrap();
    eng.register(COPY_WITH_HANDLER).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
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

#[test]
fn should_reject_handler_call_with_inout_coercion_writeback() {
    // 设计 §6.2：v NUMBER := 21 传入 IN OUT VARCHAR2 空过程，写回 '21'
    // 可观察（p_o 输出）；NoOp 证明必须拒绝，整段退回解释执行。
    let sql = "CREATE PROCEDURE copy_rows(p_o OUT VARCHAR2) IS v NUMBER := 21; BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
        EXCEPTION WHEN OTHERS THEN p_touch(v); END; \
        END LOOP; p_o := v; END;";
    let setup = || {
        let mut eng = engine();
        eng.register("CREATE PROCEDURE p_touch(p_x IN OUT VARCHAR2) AS BEGIN NULL; END;")
            .unwrap();
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    let expected = baseline.call("copy_rows", &[SpinValue::Null]).unwrap();
    assert_eq!(expected.outs.get("p_o"), Some(&SpinValue::Str("21".into())));

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let result = eng.call("copy_rows", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs, expected.outs);
    assert_eq!(rows(&eng), rows(&baseline));
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedCall
        }
    );
}

#[test]
fn should_not_treat_noop_body_with_inout_coercion_as_noop() {
    for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
        let mut eng = engine();
        eng.register("CREATE PROCEDURE p_touch(p_x IN OUT VARCHAR2) AS BEGIN NULL; END;")
            .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_touch(before_call OUT NUMBER, after_call OUT NUMBER) \
             IS v NUMBER := 21; BEGIN \
             before_call := CASE WHEN v < '3' THEN 1 ELSE 0 END; \
             FOR r IN SELECT a, b FROM source_rows LOOP \
             BEGIN INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
             EXCEPTION WHEN OTHERS THEN p_touch(v); END; END LOOP; \
             IF v < '3' THEN after_call := 1; ELSE after_call := 0; END IF; END;",
        )
        .unwrap();
        eng.set_oriented = mode;
        let result = eng
            .call("copy_touch", &[SpinValue::Null, SpinValue::Null])
            .unwrap();
        assert_eq!(result.outs.get("before_call"), Some(&SpinValue::Int(0)));
        assert_eq!(result.outs.get("after_call"), Some(&SpinValue::Int(1)));
        assert_eq!(rows(&eng), vec![vec![SpinValue::Decimal(2000, 38, 3)]]);
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
fn should_reject_handler_call_to_unregistered_routine() {
    // 目录未命中：保留原分派与报错时机（坏行触发处理器内 BIZ-0150），
    // 不得按 NoOp 消除。
    let mut baseline = engine();
    baseline.register(COPY_WITH_HANDLER).unwrap();
    let expected_err = baseline.call("copy_rows", &[]).unwrap_err();

    let mut eng = engine();
    eng.register(COPY_WITH_HANDLER).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let err = eng.call("copy_rows", &[]).unwrap_err();
    assert_eq!(err.message, expected_err.message);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedCall
        }
    );
}
