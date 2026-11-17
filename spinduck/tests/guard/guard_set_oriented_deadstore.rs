//! 死存储消除守护（V2 设计 §5 清单 g：语料 v_step_no 形态）。
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
        "CREATE TABLE t_copy(val BIGINT)",
        "INSERT INTO source_rows VALUES (10, 1), (20, 2)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query("SELECT val FROM t_copy ORDER BY val", &BindMap::new())
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_eliminate_dead_constant_store_in_copy_loop() {
    // v_step 全程序无读取（语料 v_step_no 形态）：常量赋值整体消除，
    // BatchAtomic 必须 Converted 且与 Off 逐行一致。
    let sql = "CREATE PROCEDURE copy_dead IS v_step NUMBER; BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        v_step := '2.2'; \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_dead", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(
        expected,
        vec![vec![SpinValue::Int(10)], vec![SpinValue::Int(20)]]
    );

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_dead", &[]).unwrap();
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
fn should_eliminate_dead_varchar_store_with_str_constant() {
    // Varchar 槽 + Str 常量：coerce_for_slot 无损，可消除。
    let sql = "CREATE PROCEDURE copy_dead IS v_step VARCHAR2(10); BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        v_step := '2.2'; \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_dead", &[]).unwrap();
    let expected = rows(&baseline);

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_dead", &[]).unwrap();
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
fn should_keep_constant_store_when_variable_is_read_later() {
    // v_step 被 INSERT 绑定读取：不是死存储，保守退回原区域解释执行。
    let sql = "CREATE PROCEDURE copy_live IS v_step NUMBER; BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        v_step := 7; \
        INSERT INTO t_copy(val) VALUES (v_step); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_live", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(
        expected,
        vec![vec![SpinValue::Int(7)], vec![SpinValue::Int(7)]]
    );

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_live", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(eng.set_oriented_reports()[0].status, Status::Skipped { .. }),
        "report: {:?}",
        eng.set_oriented_reports()[0].status
    );
}

#[test]
fn should_reject_dead_store_writeback_to_out_param() {
    // 死变量本身是 OUT 形参：调用结束写回可观察，消除即改语义，
    // 必须 LiveOutState 退回且写回值与 Off 一致。
    let sql = "CREATE PROCEDURE copy_out(p_step OUT NUMBER) IS BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        p_step := '2.2'; \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine();
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    let expected = baseline.call("copy_out", &[SpinValue::Null]).unwrap();
    assert_eq!(
        expected.outs.get("p_step"),
        Some(&SpinValue::Str("2.2".into()))
    );

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let result = eng.call("copy_out", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs, expected.outs);
    assert_eq!(rows(&eng), rows(&baseline));
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::LiveOutState
        }
    );
}
