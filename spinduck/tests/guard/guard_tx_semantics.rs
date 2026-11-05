//! 回归守护（AGENTS §4.3）：统一事务边界（决策 2025-11-05）。
//!
//! TxCommit/TxRollback 曾是空操作（Nop 同路径）；现最外层调用在显式事务内
//! 执行，事务语句即时提交/回滚并重开——异常处理器中的 ROLLBACK 具备撤销
//! 已执行 DML 的语义；未处理异常由 engine.call 统一 ROLLBACK。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;

fn setup() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute("CREATE TABLE tx_t(a INT)", &BindMap::new())
        .unwrap();
    eng
}

fn count(eng: &Engine) -> i64 {
    let mut s = eng
        .ex
        .query("SELECT count(*) FROM tx_t", &BindMap::new())
        .unwrap();
    match s.fetch_many(1)[0][0].clone() {
        SpinValue::Int(n) => n,
        other => other.as_str_loose().parse().unwrap(),
    }
}

/// 解释器/编译器递归深，测试线程给足栈（与 golden harness 同口径）。
fn big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn should_commit_on_tx_commit_statement() {
    big_stack(|| {
        let mut eng = setup();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_c IS BEGIN \
         INSERT INTO tx_t VALUES (1); COMMIT; INSERT INTO tx_t VALUES (2); \
         END;",
        )
        .unwrap();
        eng.call("prc_c", &[]).unwrap();
        assert_eq!(count(&eng), 2, "COMMIT 后续写仍在最终统一边界提交");
    });
}

#[test]
fn should_rollback_on_tx_rollback_statement() {
    big_stack(|| {
        let mut eng = setup();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_r IS BEGIN \
         INSERT INTO tx_t VALUES (1); ROLLBACK; \
         END;",
        )
        .unwrap();
        eng.call("prc_r", &[]).unwrap();
        assert_eq!(count(&eng), 0, "ROLLBACK 撤销已执行 DML");
    });
}

#[test]
fn should_rollback_on_unhandled_exception() {
    big_stack(|| {
        let mut eng = setup();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_e IS BEGIN \
         INSERT INTO tx_t VALUES (1); \
         RAISE_APPLICATION_ERROR(-20001, 'boom'); \
         END;",
        )
        .unwrap();
        assert!(eng.call("prc_e", &[]).is_err(), "未处理异常应外抛");
        assert_eq!(count(&eng), 0, "统一 ROLLBACK 撤销半程 DML");
    });
}
