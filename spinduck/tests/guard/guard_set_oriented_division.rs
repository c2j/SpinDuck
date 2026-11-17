//! PL 除法与原生 SQL 除零的语义边界守护（V2 设计 §4.5 第 3 项 / §6.2）。
//!
//! 锁定 Off 基线：原生 SQL `/` 除零产生 inf/nan（VARCHAR 合法落库、DECIMAL
//! 转换报错），PL IR 除法整数截断、NULL 传播、非空除零抛 BIZ-0124 并回滚
//! 先前写入。优化器在证明义务补齐前必须整段跳过且逐行结果/错误逐字一致——
//! 未来的坏行过滤守卫不得删除合法的 inf/nan 行，也不得把 PL 除零守卫
//! 误套到原生 SQL 表达式上。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const NATIVE_DIV: &str = "CREATE PROCEDURE copy_div IS BEGIN \
    FOR r IN SELECT a, b FROM source_rows LOOP \
    INSERT INTO target_rows(val) VALUES (r.a / r.b); \
    END LOOP; END;";

const PL_DIV: &str = "CREATE PROCEDURE copy_pl IS \
    v NUMBER; \
    BEGIN \
    FOR r IN SELECT a, b FROM source_rows LOOP \
    v := r.a / r.b; \
    INSERT INTO target_rows(val) VALUES (v); \
    END LOOP; END;";

fn engine(target: &str) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        target,
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn seed(eng: &Engine, values: &str) {
    eng.ex
        .execute(
            &format!("INSERT INTO source_rows VALUES {values}"),
            &BindMap::new(),
        )
        .unwrap();
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val FROM target_rows ORDER BY rowid",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

fn str_rows(vals: &[&str]) -> Vec<Vec<SpinValue>> {
    vals.iter()
        .map(|v| vec![SpinValue::Str((*v).into())])
        .collect()
}

/// 原生 SQL `0/0` 写进 VARCHAR 的内核拼法在平台间不一致（macOS "nan"，
/// Linux "-nan"）；其余三行（inf / 2.0 / NULL）逐字锁定。
fn assert_native_divergence_rows(got: &[Vec<SpinValue>]) {
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0], vec![SpinValue::Str("inf".into())]);
    assert!(
        matches!(&got[1][0], SpinValue::Str(v) if v == "nan" || v == "-nan"),
        "0/0 row must be nan or -nan: {got:?}"
    );
    assert_eq!(got[2], vec![SpinValue::Str("2.0".into())]);
    assert_eq!(got[3], vec![SpinValue::Null]);
}

#[test]
fn should_store_native_sql_division_inf_nan_in_varchar_target() {
    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
    eng.register(NATIVE_DIV).unwrap();
    eng.call("copy_div", &[]).unwrap();
    assert_native_divergence_rows(&rows(&eng));
}

#[test]
fn should_fail_native_sql_division_into_decimal_target() {
    let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
    seed(&eng, "(1, 0), (6, 3)");
    eng.register(NATIVE_DIV).unwrap();
    let error = eng.call("copy_div", &[]).unwrap_err();
    assert_eq!(error.code.0, "KERNEL-0010");
    assert!(error.message.contains("inf"), "{}", error.message);
    assert!(error.message.contains("DECIMAL(15,3)"), "{}", error.message);
    assert_eq!(rows(&eng), Vec::<Vec<SpinValue>>::new());
}

#[test]
fn should_truncate_pl_integer_division_and_propagate_null() {
    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(1, 2), (9007199254740993, 1), (NULL, 0)");
    eng.register(PL_DIV).unwrap();
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(
        rows(&eng),
        vec![
            vec![SpinValue::Str("0".into())],
            vec![SpinValue::Str("9007199254740993".into())],
            vec![SpinValue::Null],
        ]
    );
}

#[test]
fn should_raise_biz_0124_and_rollback_prior_rows_on_pl_division_by_zero() {
    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(6, 3), (7, 0)");
    eng.register(PL_DIV).unwrap();
    let error = eng.call("copy_pl", &[]).unwrap_err();
    assert_eq!(error.code.0, "BIZ-0124");
    assert!(
        error.message.contains("division by zero"),
        "{}",
        error.message
    );
    assert_eq!(rows(&eng), Vec::<Vec<SpinValue>>::new());
}

#[test]
fn should_keep_fraction_when_assigning_to_number_scale_zero_slot() {
    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(1, 1)");
    eng.register(
        "CREATE PROCEDURE copy_num IS \
        v NUMBER(3,0); \
        BEGIN \
        v := 1.5; \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        INSERT INTO target_rows(val) VALUES (v); \
        END LOOP; END;",
    )
    .unwrap();
    eng.call("copy_num", &[]).unwrap();
    assert_eq!(rows(&eng), str_rows(&["1.5"]));
}

/// 设计 §6.2 命名守护：同一含零分母语料，PL 除法与原生 SQL 除零语义必须
/// 分流；Strict/BatchAtomic 在表达式证明补齐前整段跳过，行与错误逐字对齐。
#[test]
fn should_distinguish_pl_division_from_native_sql_division_by_zero() {
    const SEED: &str = "(1, 0), (0, 0), (6, 3), (NULL, 0)";

    let mut off_native = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&off_native, SEED);
    off_native.register(NATIVE_DIV).unwrap();
    off_native.call("copy_div", &[]).unwrap();
    let native_rows = rows(&off_native);
    assert_native_divergence_rows(&native_rows);

    let mut off_pl = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&off_pl, SEED);
    off_pl.register(PL_DIV).unwrap();
    let pl_error = off_pl.call("copy_pl", &[]).unwrap_err();
    assert_eq!(pl_error.code.0, "BIZ-0124");
    assert_eq!(rows(&off_pl), Vec::<Vec<SpinValue>>::new());

    for mode in [SetOrientedMode::Strict, SetOrientedMode::BatchAtomic] {
        let mut native = engine("CREATE TABLE target_rows(val VARCHAR)");
        seed(&native, SEED);
        native.register(NATIVE_DIV).unwrap();
        native.set_oriented = mode;
        native.call("copy_div", &[]).unwrap();
        assert_eq!(rows(&native), native_rows, "{mode:?}");
        assert_eq!(
            native.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnsupportedWrite,
            },
            "{mode:?}"
        );

        let mut pl = engine("CREATE TABLE target_rows(val VARCHAR)");
        seed(&pl, SEED);
        pl.register(PL_DIV).unwrap();
        pl.set_oriented = mode;
        let error = pl.call("copy_pl", &[]).unwrap_err();
        assert_eq!(error.code, pl_error.code, "{mode:?}");
        assert_eq!(error.message, pl_error.message, "{mode:?}");
        assert_eq!(rows(&pl), Vec::<Vec<SpinValue>>::new(), "{mode:?}");
        assert_eq!(
            pl.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnsupportedControlFlow,
            },
            "{mode:?}"
        );
    }
}
