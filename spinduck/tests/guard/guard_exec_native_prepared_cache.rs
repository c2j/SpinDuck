//! 回归守护（AGENTS §4.3，V24）：prepared 语句缓存与 DDL/temp 表交互。
//!
//! 缓存的 `CREATE OR REPLACE TEMP TABLE` plan 二次执行会留下 0 列残表
//! （sqlfn 物化路径 `materialize_fn_sites` 必踩）——修复口径是 DDL 本体
//! 绕过语句缓存；同文本语句在表 DROP/重建（列类型变化）后必须按新
//! schema 重新绑定（DuckDB execute 期 rebind 兜底，V24 评审决断 1）。

#![cfg(feature = "native")]

use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;

#[test]
fn guard_should_recreate_temp_table_across_prepared_cache() {
    let ex = DuckExecutor::open_in_memory().unwrap();
    for round in 0..2 {
        ex.execute(
            "CREATE OR REPLACE TEMP TABLE sd_fn0 (sd_v VARCHAR, sd_a0 VARCHAR)",
            &BindMap::new(),
        )
        .unwrap_or_else(|e| panic!("round {round} create: {}", e.message));
        let ins = format!("INSERT INTO sd_fn0 VALUES ('v{round}','a{round}')");
        ex.execute(&ins, &BindMap::new())
            .unwrap_or_else(|e| panic!("round {round} insert: {}", e.message));
        let mut s = ex
            .query("SELECT count(*) FROM sd_fn0", &BindMap::new())
            .unwrap_or_else(|e| panic!("round {round} query: {}", e.message));
        assert_eq!(s.fetch_many(1)[0][0].as_str_loose(), "1");
    }
}

#[test]
fn guard_should_rebind_prepared_shape_after_table_recreate() {
    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute("CREATE TABLE t(a BIGINT)", &BindMap::new())
        .unwrap();
    ex.execute("INSERT INTO t VALUES (42)", &BindMap::new())
        .unwrap();
    let mut s = ex.query("SELECT a FROM t", &BindMap::new()).unwrap();
    assert_eq!(s.fetch_many(1)[0][0], SpinValue::Int(42));
    ex.execute("DROP TABLE t", &BindMap::new()).unwrap();
    ex.execute("CREATE TABLE t(a VARCHAR)", &BindMap::new())
        .unwrap();
    ex.execute("INSERT INTO t VALUES ('xyz')", &BindMap::new())
        .unwrap();
    let mut s = ex
        .query("SELECT a FROM t", &BindMap::new())
        .unwrap_or_else(|e| panic!("表重建后同文本语句必须可重新绑定: {}", e.message));
    assert_eq!(s.fetch_many(1)[0][0], SpinValue::Str("xyz".into()));
}
