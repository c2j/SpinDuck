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
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.ex
            .execute("CREATE TABLE dyn_t(a INT)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_dyn IS DECLARE v_sql VARCHAR2(100); BEGIN \
             v_sql := 'insert into dyn_t values (' || 7 || ')'; \
             EXECUTE IMMEDIATE v_sql; \
             END;",
        )
        .unwrap();
        eng.call("prc_dyn", &[]).unwrap();
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
        // ogsql-parser 对乱码文本给出解析错误 → 运行期显式 BIZ-061，不执行未解析文本
        let e = eng.call("prc_bad", &[]).unwrap_err();
        assert_eq!(
            e.code.to_string(),
            "BIZ-0061",
            "乱码动态 SQL 必须报解析缺口，实际: {e:?}"
        );
        assert!(
            e.message.contains("动态 SQL 解析失败"),
            "消息须点明动态 SQL 解析失败，实际: {}",
            e.message
        );
    });
}

#[test]
fn should_fetch_dynamic_execute_into_targets() {
    // EXECUTE IMMEDIATE .. INTO：与静态 SELECT INTO 同语义（恢复静态期能力）
    big_stack(|| {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.ex
            .execute("CREATE TABLE src_t(a INT)", &BindMap::new())
            .unwrap();
        eng.ex
            .execute("INSERT INTO src_t VALUES (42)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_into IS DECLARE v_a NUMBER; BEGIN \
             EXECUTE IMMEDIATE 'select a from src_t' INTO v_a; \
             INSERT INTO src_t VALUES (v_a + 1); \
             END;",
        )
        .unwrap();
        eng.call("prc_into", &[]).unwrap();
        let mut s = eng
            .ex
            .query("SELECT count(*) FROM src_t WHERE a = 43", &BindMap::new())
            .unwrap();
        assert_eq!(s.fetch_many(1)[0][0], SpinValue::Int(1));
    });
}

/// 动态 INTO 的行数语义必须与静态 SELECT INTO 同口径（0 行 / >1 行）。
#[test]
fn should_raise_no_data_found_on_dynamic_into_zero_rows() {
    big_stack(|| {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.ex
            .execute("CREATE TABLE empty_t(a INT)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_ndf IS DECLARE v_a NUMBER; BEGIN \
             EXECUTE IMMEDIATE 'select a from empty_t' INTO v_a; \
             END;",
        )
        .unwrap();
        let e = eng
            .call("prc_ndf", &[])
            .expect_err("0 行必须报 NO_DATA_FOUND");
        assert_eq!(e.code.to_string(), "BIZ-0100", "实际: {e:?}");
    });
}

#[test]
fn should_raise_too_many_rows_on_dynamic_into_multiple_rows() {
    big_stack(|| {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.ex
            .execute("CREATE TABLE two_t(a INT)", &BindMap::new())
            .unwrap();
        eng.ex
            .execute("INSERT INTO two_t VALUES (1), (2)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_tmr IS DECLARE v_a NUMBER; BEGIN \
             EXECUTE IMMEDIATE 'select a from two_t' INTO v_a; \
             END;",
        )
        .unwrap();
        let e = eng
            .call("prc_tmr", &[])
            .expect_err(">1 行必须报 TOO_MANY_ROWS");
        assert_eq!(e.code.to_string(), "BIZ-0101", "实际: {e:?}");
    });
}
