//! 多语句动态 SQL 写入后的点查缓存回归守护。
#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;

#[test]
fn should_reject_stale_lookup_after_dynamic_delete() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE par_sys_purchase(security_id VARCHAR, purchase_days INTEGER, \
         inure_begin_date VARCHAR, inure_end_date VARCHAR)",
        "CREATE TABLE other(a INTEGER)",
        "INSERT INTO par_sys_purchase VALUES ('S1', 7, '20260101', '20260131')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_e2e(p_date VARCHAR2, p_id VARCHAR2) IS \
         v_days NUMBER; BEGIN \
         SELECT coalesce(t.purchase_days, 1) INTO v_days FROM par_sys_purchase t \
         WHERE p_date BETWEEN t.inure_begin_date AND t.inure_end_date \
         AND t.security_id = p_id; \
         EXECUTE IMMEDIATE 'INSERT INTO other VALUES (1); DELETE FROM par_sys_purchase;'; \
         SELECT coalesce(t.purchase_days, 1) INTO v_days FROM par_sys_purchase t \
         WHERE p_date BETWEEN t.inure_begin_date AND t.inure_end_date \
         AND t.security_id = p_id; END;",
    )
    .unwrap();
    let err = eng
        .call(
            "prc_e2e",
            &[
                SpinValue::Str("20260105".into()),
                SpinValue::Str("S1".into()),
            ],
        )
        .unwrap_err();
    assert_eq!(err.code.0, "BIZ-0100");
    assert!(err.message.contains("NO_DATA_FOUND"), "{}", err.message);
    let rows = eng
        .ex
        .query(
            "SELECT (SELECT count(*) FROM par_sys_purchase), (SELECT count(*) FROM other)",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(2);
    assert_eq!(
        rows,
        vec![vec![SpinValue::Int(1), SpinValue::Int(0)]],
        "未捕获错误应回滚整个调用"
    );
}
