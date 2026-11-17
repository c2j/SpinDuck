//! 前缀异常块守护（V2 设计 §5-1：DELETE/COMMIT + WHEN OTHERS + RETURN
//! 前缀按原 IR 执行；异常路径终结例程，区域不可达）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT)",
        "CREATE TABLE t_copy(val BIGINT)",
        "INSERT INTO source_rows VALUES (10), (20)",
        "INSERT INTO t_copy VALUES (99)",
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

const PREFIX_TRAP: &str = "CREATE PROCEDURE copy_prefix(p_o OUT VARCHAR2) IS BEGIN \
    p_o := '0'; \
    BEGIN DELETE FROM t_copy WHERE val < 0; COMMIT; \
    EXCEPTION WHEN OTHERS THEN p_o := 'cleanup failed'; RETURN; END; \
    FOR r IN SELECT a FROM source_rows LOOP \
    INSERT INTO t_copy(val) VALUES (r.a); \
    END LOOP; END;";

#[test]
fn should_convert_with_prefix_delete_commit_trap_and_return() {
    // 正常路径：前缀 DELETE+COMMIT 后事务干净，BatchAtomic 折叠转换；
    // 前缀已写入的 99 不被重放也不被回滚。
    let mut baseline = engine();
    baseline.register(PREFIX_TRAP).unwrap();
    let expected = baseline.call("copy_prefix", &[SpinValue::Null]).unwrap();
    assert_eq!(expected.outs.get("p_o"), Some(&SpinValue::Str("0".into())));
    let expected_rows = rows(&baseline);
    assert_eq!(
        expected_rows,
        vec![
            vec![SpinValue::Int(10)],
            vec![SpinValue::Int(20)],
            vec![SpinValue::Int(99)]
        ]
    );

    let mut eng = engine();
    eng.register(PREFIX_TRAP).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let result = eng.call("copy_prefix", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs, expected.outs);
    assert_eq!(rows(&eng), expected_rows);
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
fn should_run_prefix_trap_error_path_without_reaching_region() {
    // 异常路径：DELETE 目标表不存在 → 处理器赋 p_o 后 RETURN，
    // 区域不可达；与 Off 的 outs/行集完全一致，不重放前缀。
    let sql = "CREATE PROCEDURE copy_prefix(p_o OUT VARCHAR2) IS BEGIN \
        p_o := '0'; \
        BEGIN DELETE FROM missing_table; COMMIT; \
        EXCEPTION WHEN OTHERS THEN p_o := 'cleanup failed'; RETURN; END; \
        FOR r IN SELECT a FROM source_rows LOOP \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    let expected = baseline.call("copy_prefix", &[SpinValue::Null]).unwrap();
    assert_eq!(
        expected.outs.get("p_o"),
        Some(&SpinValue::Str("cleanup failed".into()))
    );
    let expected_rows = rows(&baseline);
    assert_eq!(expected_rows, vec![vec![SpinValue::Int(99)]]);

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let result = eng.call("copy_prefix", &[SpinValue::Null]).unwrap();
    assert_eq!(result.outs, expected.outs);
    assert_eq!(rows(&eng), expected_rows);
}

#[test]
fn should_reject_prefix_trap_with_fallthrough_handler() {
    // 处理器不终结（无 RETURN）：异常路径会落回区域入口，
    // 干净度不可证，保守拒绝并与 Off 逐行一致。
    let sql = "CREATE PROCEDURE copy_prefix(p_o OUT VARCHAR2) IS BEGIN \
        p_o := '0'; \
        BEGIN DELETE FROM t_copy WHERE val < 0; COMMIT; \
        EXCEPTION WHEN OTHERS THEN p_o := 'cleanup failed'; END; \
        FOR r IN SELECT a FROM source_rows LOOP \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_prefix", &[SpinValue::Null]).unwrap();
    let expected_rows = rows(&baseline);

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_prefix", &[SpinValue::Null]).unwrap();
    assert_eq!(rows(&eng), expected_rows);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::ExceptionScopePresent
        }
    );
}

#[test]
fn should_convert_prefix_trap_in_strict_mode() {
    // Strict 不折叠提交，前缀异常块同样按原 IR 执行后可转换。
    let mut baseline = engine();
    baseline.register(PREFIX_TRAP).unwrap();
    baseline.call("copy_prefix", &[SpinValue::Null]).unwrap();
    let expected_rows = rows(&baseline);

    let mut eng = engine();
    eng.register(PREFIX_TRAP).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_prefix", &[SpinValue::Null]).unwrap();
    assert_eq!(rows(&eng), expected_rows);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0].status
    );
}
