//! Gate 1B 列级检查单元测试（自 `columns/mod.rs` 拆分，AGENTS §3）。

use super::*;
use ogsql_parser::token::tokenizer::Tokenizer;

fn snap_of(ddl: &str) -> SchemaSnapshot {
    let tokens = Tokenizer::new(ddl).tokenize().unwrap();
    let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
    SchemaSnapshot::from_statements(&stmts).unwrap()
}

fn stmt_of(sql: &str) -> Statement {
    let tokens = Tokenizer::new(sql).tokenize().unwrap();
    let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
    assert_eq!(stmts.len(), 1, "{sql}");
    stmts.into_iter().next().unwrap()
}

fn missing(sql: &str, snap: &SchemaSnapshot) -> Vec<(Option<String>, String, &'static str)> {
    check_columns(&stmt_of(sql), snap)
        .into_iter()
        .map(|m| (m.qualifier, m.column, m.problem))
        .collect()
}

#[test]
fn should_flag_unqualified_missing_column() {
    let snap = snap_of("CREATE TABLE t1(a INT, b VARCHAR2(10))");
    assert_eq!(
        missing("SELECT typo_col FROM t1", &snap),
        [(None, "typo_col".to_string(), "missing")]
    );
    assert!(missing("SELECT a, b FROM t1", &snap).is_empty());
}

#[test]
fn should_flag_qualified_missing_column() {
    let snap = snap_of("CREATE TABLE t1(a INT)");
    assert_eq!(
        missing("SELECT t1.nope FROM t1", &snap),
        [(Some("t1".to_string()), "nope".to_string(), "missing")]
    );
    // 别名 x 下 a 存在 → 不误报
    assert!(missing("SELECT x.a FROM t1 x", &snap).is_empty());
}

#[test]
fn should_flag_ambiguous_unqualified_column() {
    let snap = snap_of("CREATE TABLE t1(id INT); CREATE TABLE t2(id INT)");
    assert_eq!(
        missing("SELECT id FROM t1, t2", &snap),
        [(None, "id".to_string(), "ambiguous")]
    );
    // 限定引用消歧
    assert!(missing("SELECT t1.id FROM t1, t2", &snap).is_empty());
}

#[test]
fn should_resolve_cte_and_subquery_columns() {
    let snap = snap_of("CREATE TABLE t1(a INT, b INT)");
    // CTE 显式列清单
    assert_eq!(
        missing(
            "WITH c(x, y) AS (SELECT a, b FROM t1) SELECT z FROM c",
            &snap
        ),
        [(None, "z".to_string(), "missing")]
    );
    assert!(missing(
        "WITH c(x, y) AS (SELECT a, b FROM t1) SELECT x FROM c",
        &snap
    )
    .is_empty());
    // 子查询别名投影列
    assert_eq!(
        missing("SELECT s.nope FROM (SELECT a AS k FROM t1) s", &snap),
        [(Some("s".to_string()), "nope".to_string(), "missing")]
    );
    assert!(missing("SELECT s.k FROM (SELECT a AS k FROM t1) s", &snap).is_empty());
    // 含 * 投影 → Unknown，不判缺失
    assert!(missing("SELECT s.anything FROM (SELECT * FROM t1) s", &snap).is_empty());
}

#[test]
fn should_skip_unknown_view_and_deferred_ctas() {
    let snap = snap_of("CREATE VIEW v_unk AS SELECT 1; CREATE TEMP TABLE tmp AS SELECT a FROM t1");
    // 视图 Unknown：列引用不判
    assert!(missing("SELECT v.anything FROM v_unk v", &snap).is_empty());
    // CTAS 延迟表：列引用不判（归 Gate 2B）
    assert!(missing("SELECT tmp.whatever FROM tmp", &snap).is_empty());
}

#[test]
fn should_skip_columns_of_local_temp_table() {
    // 例程体内声明的局部表：对象已知、列集合延迟到 Gate 2B；
    // 声明之前的引用不享受该豁免。
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let got = check_columns(
        &stmt_of(
            "CREATE PROCEDURE p IS BEGIN \
             CREATE TEMP TABLE tmp_pl AS SELECT a FROM t1; \
             SELECT whatever FROM tmp_pl; END;",
        ),
        &snap,
    );
    assert!(got.is_empty(), "{got:?}");
    // 声明之前引用 tmp_pl 的列 → 对象缺失归对象级切片，
    // 列级同样不得把 tmp_pl 当已知（Cols::Unknown 之外不误伤）
    let got = check_columns(&stmt_of("SELECT whatever FROM tmp_pl"), &snap);
    // tmp_pl 不在快照 → 对象级会报 missing；列级对未知对象本就 Unknown，
    // 断言仅锁定"不产生 missing/ambiguous 列诊断"。
    assert!(
        got.iter()
            .all(|m| m.problem != "missing" && m.problem != "ambiguous"),
        "{got:?}"
    );
}

#[test]
fn should_isolate_local_table_columns_between_package_routines() {
    // 包体例程作用域隔离：p1 声明的局部 tmp 不泄漏到 p2——p2 的
    // `SELECT l FROM tmp` 必须按全局 tmp(g) 判定，l 缺失要报；
    // 泄漏时 tmp 被当 Cols::Unknown 而漏报。
    let snap = snap_of("CREATE TABLE tmp(g INT)");
    let got = check_columns(
        &stmt_of(
            "CREATE PACKAGE BODY pkg AS \
             PROCEDURE p1 IS BEGIN CREATE TEMP TABLE tmp(l INT); END; \
             PROCEDURE p2 IS BEGIN SELECT l FROM tmp; END; \
             END pkg",
        ),
        &snap,
    );
    assert_eq!(
        got.iter()
            .map(|m| (m.qualifier.clone(), m.column.clone(), m.problem))
            .collect::<Vec<_>>(),
        [(None, "l".to_string(), "missing")],
        "{got:?}"
    );
}

#[test]
fn should_not_flag_pl_variable_as_column() {
    // 例程参数/变量在内嵌 SQL 中不算列引用
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let sql = "CREATE OR REPLACE PROCEDURE p(v_x INT) IS BEGIN \
               INSERT INTO t1 VALUES (v_x); \
               FOR r IN SELECT v_x AS k FROM t1 LOOP NULL; END LOOP; \
               END;";
    assert!(missing(sql, &snap).is_empty(), "{:?}", missing(sql, &snap));
}

#[test]
fn should_check_columns_inside_embedded_sql() {
    let snap = snap_of("CREATE TABLE t1(a INT)");
    let sql = "CREATE OR REPLACE PROCEDURE p IS BEGIN \
               INSERT INTO t1 SELECT bad_col FROM t1; END;";
    assert_eq!(
        missing(sql, &snap),
        [(None, "bad_col".to_string(), "missing")]
    );
}

#[test]
fn should_flag_insert_column_list_missing() {
    let snap = snap_of("CREATE TABLE t1(a INT, b INT)");
    assert_eq!(
        missing("INSERT INTO t1(nope) SELECT a FROM t1", &snap),
        [(Some("t1".to_string()), "nope".to_string(), "missing")]
    );
    assert!(missing("INSERT INTO t1(a, b) SELECT a, b FROM t1", &snap).is_empty());
}

#[test]
fn should_check_update_and_delete_column_scopes() {
    let snap = snap_of("CREATE TABLE t1(a INT, b INT); CREATE TABLE t2(c INT)");
    // UPDATE 右值/WHERE/SET 左列
    assert_eq!(
        missing("UPDATE t1 SET a = bad_rhs WHERE b = 1", &snap),
        [(None, "bad_rhs".to_string(), "missing")]
    );
    assert_eq!(
        missing("UPDATE t1 SET nope = 1", &snap),
        [(None, "nope".to_string(), "missing")]
    );
    assert_eq!(
        missing("DELETE FROM t1 WHERE nope = 1", &snap),
        [(None, "nope".to_string(), "missing")]
    );
    assert!(missing("UPDATE t1 SET a = 1 WHERE b = 2", &snap).is_empty());
    assert!(missing("DELETE FROM t1 WHERE a = 1", &snap).is_empty());
}

#[test]
fn should_check_update_from_scope() {
    let snap = snap_of("CREATE TABLE t1(a INT); CREATE TABLE t2(b INT)");
    assert!(missing("UPDATE t1 SET a = t2.b FROM t2", &snap).is_empty());
    assert_eq!(
        missing("UPDATE t1 SET a = t2.nope FROM t2", &snap),
        [(Some("t2".to_string()), "nope".to_string(), "missing")]
    );
}

#[test]
fn should_treat_engine_provided_and_dual_as_known() {
    let snap = snap_of("CREATE TABLE t1(a INT)");
    // sys_dummy：存在性成立、列不可静态确定 → 不判
    assert!(missing("SELECT * FROM sys_dummy", &snap).is_empty());
    assert!(missing("SELECT x FROM sys_dummy", &snap).is_empty());
    // dual：Oracle 口径仅 DUMMY 列
    assert!(missing("SELECT dummy FROM dual", &snap).is_empty());
    assert_eq!(
        missing("SELECT nope FROM dual", &snap),
        [(None, "nope".to_string(), "missing")]
    );
}

#[test]
fn should_check_percent_type_and_rowtype_anchors() {
    let snap = snap_of("CREATE TABLE t1(a INT, b INT)");
    // %TYPE 列存在 → 放行；列缺失/表缺失 → 上报
    let ok = "CREATE PROCEDURE p IS v_a t1.a%TYPE; BEGIN NULL; END;";
    assert!(missing(ok, &snap).is_empty(), "{:?}", missing(ok, &snap));
    let bad_col = "CREATE PROCEDURE p IS v_a t1.nope%TYPE; BEGIN NULL; END;";
    assert_eq!(
        missing(bad_col, &snap),
        [(Some("t1".to_string()), "nope".to_string(), "missing")]
    );
    let bad_tbl = "CREATE PROCEDURE p IS v_a ghost.x%TYPE; BEGIN NULL; END;";
    assert_eq!(
        missing(bad_tbl, &snap),
        [(Some("ghost".to_string()), "x".to_string(), "missing_table")]
    );
    let bad_row = "CREATE PROCEDURE p IS r t1%ROWTYPE; v_g ghost%ROWTYPE; BEGIN NULL; END;";
    assert_eq!(
        missing(bad_row, &snap),
        [(
            Some("ghost".to_string()),
            "%rowtype".to_string(),
            "missing_table"
        )]
    );
}
