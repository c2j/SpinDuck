//! 编译期门禁守护（设计文档 V4 §4.4 Gate 2A DDL 层）：GaussDB/Oracle 方言
//! 或 DuckDB 1.5.5 不支持的顶层 DDL 必须在 `eng.register` 编译期以 AST
//! 判定显式拒绝（`ENG-0071`），不得随非例程语句被静默放过。
//!
//! 判定只看 `Statement` AST 变体，不做文本/关键字匹配。

use spinduck_core::engine::Engine;
use spinduck_core::exec::FakeExecutor;
use spinduck_core::rules::RuleRegistry;

fn engine() -> Engine {
    Engine::new(Box::new(FakeExecutor::default()), RuleRegistry::default())
}

fn assert_ddl_rejected(sql: &str) {
    let err = engine()
        .register(sql)
        .expect_err("不支持的 DDL 必须在编译期被拒绝");
    assert_eq!(err.code.to_string(), "ENG-0071", "{sql}: {err:?}");
}

#[test]
fn should_reject_trigger_with_eng_0071() {
    // DuckDB 1.5.5 无 CREATE TRIGGER（2.0 才引入）
    assert_ddl_rejected(
        "CREATE TRIGGER trg_t BEFORE INSERT ON tab FOR EACH ROW EXECUTE FUNCTION f()",
    );
}

#[test]
fn should_reject_materialized_view_with_eng_0071() {
    assert_ddl_rejected("CREATE MATERIALIZED VIEW mv_x AS SELECT 1 AS a");
}

#[test]
fn should_reject_grant_revoke_with_eng_0071() {
    assert_ddl_rejected("GRANT SELECT ON tab TO role_x");
    assert_ddl_rejected("REVOKE SELECT ON tab FROM role_x");
}

#[test]
fn should_reject_synonym_and_listen_with_eng_0071() {
    assert_ddl_rejected("CREATE SYNONYM syn_t FOR tab");
    assert_ddl_rejected("LISTEN chan_x");
}

#[test]
fn should_reject_oracle_ddl_family_with_eng_0071() {
    // 语法均取 ogsql-parser v0.11.0 已验证形态（parser/tests.rs）
    for sql in [
        "CREATE DATABASE LINK remote_db CONNECT TO user1 IDENTIFIED BY 'pass' USING 'h:1/d'",
        "CREATE GLOBAL INDEX idx_g ON t1(c1)",
        "CREATE CAST (a AS b) WITH INOUT",
    ] {
        assert_ddl_rejected(sql);
    }
}

#[test]
fn should_keep_supported_ddl_statements() {
    // 存量行为锁定：DuckDB 支持的常规 DDL 不受影响（register 对非例程
    // 语句返回 Ok(0)，由 load/deploy 通道执行）
    let mut eng = engine();
    for sql in [
        "CREATE TABLE t1(a INT, b VARCHAR)",
        "CREATE INDEX idx_t1_a ON t1(a)",
        "CREATE VIEW v1 AS SELECT a FROM t1",
        "CREATE SEQUENCE seq_x",
        "ALTER TABLE t1 ADD COLUMN c INT",
        "DROP TABLE IF EXISTS t_old",
        "TRUNCATE TABLE t1",
        "COMMENT ON TABLE t1 IS 'doc'",
    ] {
        eng.register(sql)
            .unwrap_or_else(|e| panic!("{sql} 必须照常放行: {e:?}"));
    }
}

#[test]
fn should_allow_merge_into() {
    // DuckDB ≥1.4 支持 MERGE INTO（内核 1.5.5 覆盖）：顶层与例程体内
    // 嵌入形态都放行，由 Gate 2B prepare 做语义校验，不做 Gate 2A 拒绝。
    let mut eng = engine();
    eng.register("MERGE INTO t1 USING s1 ON t1.a = s1.a WHEN MATCHED THEN UPDATE SET t1.b = s1.b")
        .expect("顶层 MERGE 应放行（register 对非例程语句返回 Ok）");
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_m IS BEGIN MERGE INTO t1 USING s1 ON t1.a = s1.a \
         WHEN MATCHED THEN UPDATE SET t1.b = s1.b; END;",
    )
    .expect("例程体内 MERGE 应编译通过");
}
