//! 回归守护（AGENTS §4.3）：嵌套块同名游标声明的词法作用域。
//!
//! `declare_decls` 为变量绑定生成 `VarRestore`，但游标声明此前只把编译
//! 产物写进扁平 `pending_cursor_sql`、无恢复路径——内层块同名游标退出后
//! 外层 `OPEN` 会错用内层 SQL。本守护锁定：内层块内 `FETCH` 取内层结果，
//! 块结束后外层同名游标仍取外层结果。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;

fn col_str(eng: &Engine, sql: &str) -> Vec<String> {
    let mut s = eng.ex.query(sql, &BindMap::new()).unwrap();
    s.fetch_many(1000)
        .iter()
        .map(|r| r[0].as_str_loose())
        .collect()
}

/// 内层 DECLARE 块声明同名游标 `c`（SELECT 'INNER'），块内 OPEN/FETCH
/// 取 INNER；块结束后外层游标 `c`（SELECT 'OUTER'）必须仍取 OUTER——
/// 若游标声明无 restore，外层 OPEN 会消费内层遗留的编译产物。
#[test]
fn guard_should_restore_outer_cursor_sql_after_nested_same_name_decl() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute("CREATE TABLE t(s VARCHAR)", &BindMap::new())
        .unwrap();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_cur IS \
         CURSOR c IS SELECT 'OUTER' AS tag; \
         v_tag VARCHAR; \
         BEGIN \
           DECLARE \
             CURSOR c IS SELECT 'INNER' AS tag; \
           BEGIN \
             OPEN c; \
             FETCH c INTO v_tag; \
             CLOSE c; \
             INSERT INTO t(s) VALUES (v_tag); \
           END; \
           OPEN c; \
           FETCH c INTO v_tag; \
           CLOSE c; \
           INSERT INTO t(s) VALUES (v_tag); \
         END;",
    )
    .unwrap();
    eng.call("prc_cur", &[]).unwrap();
    assert_eq!(
        col_str(&eng, "SELECT s FROM t ORDER BY s"),
        vec!["INNER".to_string(), "OUTER".to_string()],
        "内层游标遮蔽仅块内有效，外层同名游标须恢复外层查询"
    );
}
