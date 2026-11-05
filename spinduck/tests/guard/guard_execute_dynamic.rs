//! 回归守护（AGENTS §4.3）：EXECUTE IMMEDIATE 统一收口（决策 2025-11-05）。
//!
//! 运行期拼接文本不再编译期拒绝（ENG-0062 撤销）；与 OPEN FOR 共用解析管线，
//! 解析失败运行期显式报 BIZ-061，不执行未解析文本。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;

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
fn should_execute_runtime_built_sql() {
    big_stack(|| {
        eprintln!("[T] a");
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eprintln!("[T] b");
        eng.ex
            .execute("CREATE TABLE dyn_t(a INT)", &BindMap::new())
            .unwrap();
        eprintln!("[T] c");
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_dyn IS DECLARE v_sql VARCHAR2(100); BEGIN \
             v_sql := 'insert into dyn_t values (' || 7 || ')'; \
             EXECUTE IMMEDIATE v_sql; \
             END;",
        )
        .unwrap();
        eprintln!("[T] d");
        eng.call("prc_dyn", &[]).unwrap();
        eprintln!("[T] e");
        let mut s = eng
            .ex
            .query("SELECT count(*) FROM dyn_t", &BindMap::new())
            .unwrap();
        assert_eq!(s.fetch_many(1)[0][0], SpinValue::Int(1));
    });
}

#[test]
fn should_report_unparseable_dynamic_sql_explicitly() {
    big_stack(|| {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_bad IS DECLARE v_sql VARCHAR2(100); BEGIN \
         v_sql := 'THIS IS NOT SQL'; \
         EXECUTE IMMEDIATE v_sql; \
         END;",
        )
        .unwrap();
        // ogsql-parser 对乱码文本宽容接受（无解析错误返回）→ 由内核显式报错；
        // 宽容解析已登记上游 issue（ogsql-parser#337）：待其提供严格模式后收敛为 BIZ-061
        let e = eng.call("prc_bad", &[]).unwrap_err();
        assert!(
            !e.message.is_empty(),
            "乱码动态 SQL 必须显式失败，不得静默成功: {e:?}"
        );
    });
}
