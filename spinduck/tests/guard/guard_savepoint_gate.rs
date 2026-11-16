//! 编译期门禁守护（设计文档 V4 §4.4 Gate 2A）：高斯合法但 SpinDuck IR 未实现
//! 且 DuckDB 1.5.5 不支持的事务保存点语句必须在 `eng.register` 编译期显式
//! 拒绝（`ENG-0071`），严禁静默降级为 `Nop` 或错编为整事务 `ROLLBACK`。
//!
//! 判定必须走 AST 节点（`PlStatement::Savepoint` / `ReleaseSavepoint` /
//! `Rollback{to_savepoint: Some}`），字符串字面量与注释中的同名文本不误杀。

use spinduck_core::engine::Engine;
use spinduck_core::exec::FakeExecutor;
use spinduck_core::rules::RuleRegistry;

fn engine() -> Engine {
    Engine::new(Box::new(FakeExecutor::default()), RuleRegistry::default())
}

fn register_err(sql: &str) -> spinduck_core::error::EngineError {
    engine()
        .register(sql)
        .expect_err("保存点语句必须在编译期被拒绝")
}

#[test]
fn should_reject_savepoint_statement_with_eng_0071() {
    let err = register_err("CREATE OR REPLACE PROCEDURE prc_sp IS BEGIN SAVEPOINT sp1; END;");
    assert_eq!(err.code.to_string(), "ENG-0071", "{err:?}");
}

#[test]
fn should_reject_rollback_to_savepoint_with_eng_0071() {
    // ROLLBACK TO <name> 曾被错编为整事务 TxRollback（语义破坏，最高优先级）。
    // 注意：ogsql-parser v0.11.0 的 ROLLBACK 文法只吃 `TO <标识符>`，不吃
    // `TO SAVEPOINT <名>` 关键字形（会降级为游离 Sql 语句报 ENG-0022），
    // 该解析缺口按 AGENTS §1 只记录不在解析器仓库内修复。
    let err = register_err(
        "CREATE OR REPLACE PROCEDURE prc_rb IS BEGIN \
         INSERT INTO t VALUES (1); ROLLBACK TO sp1; END;",
    );
    assert_eq!(err.code.to_string(), "ENG-0071", "{err:?}");
}

#[test]
fn should_reject_release_savepoint_with_eng_0071() {
    let err =
        register_err("CREATE OR REPLACE PROCEDURE prc_rel IS BEGIN RELEASE SAVEPOINT sp1; END;");
    assert_eq!(err.code.to_string(), "ENG-0071", "{err:?}");
}

#[test]
fn should_keep_plain_commit_and_rollback_unchanged() {
    // 存量行为锁定：顶层事务语句仍走 TxCommit/TxRollback，不受本门禁影响
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_tx IS BEGIN \
         COMMIT; ROLLBACK; END;",
    )
    .expect("普通 COMMIT/ROLLBACK 必须照常编译");
}

#[test]
fn should_reject_set_transaction_with_eng_0071() {
    // SET TRANSACTION ISOLATION/READ ONLY：解释器未实现，曾被静默编为 Nop
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET TRANSACTION READ ONLY",
    ] {
        let err = register_err(&format!(
            "CREATE OR REPLACE PROCEDURE prc_st IS BEGIN {sql}; END;"
        ));
        assert_eq!(err.code.to_string(), "ENG-0071", "{sql}: {err:?}");
    }
}

#[test]
fn should_reject_autonomous_transaction_pragma_with_eng_0071() {
    // PRAGMA AUTONOMOUS_TRANSACTION：声明段 Pragma 节点曾被 `_ => {}` 静默丢弃
    let err = register_err(
        "CREATE OR REPLACE PROCEDURE prc_at IS \
         PRAGMA AUTONOMOUS_TRANSACTION; BEGIN NULL; END;",
    );
    assert_eq!(err.code.to_string(), "ENG-0071", "{err:?}");
}

#[test]
fn should_reject_exception_init_pragma_with_eng_0071() {
    // PRAGMA EXCEPTION_INIT：异常名→SQLSTATE 绑定解释器未实现，静默丢弃会让
    // 后续 `WHEN <name>` 永远不命中——编译期显式拒绝（ENG-0071）
    let err = register_err(
        "CREATE OR REPLACE PROCEDURE prc_ei IS \
         PRAGMA EXCEPTION_INIT; BEGIN NULL; END;",
    );
    assert_eq!(err.code.to_string(), "ENG-0071", "{err:?}");
}

#[test]
fn should_reject_collection_type_decl_with_eng_0071() {
    // TYPE .. IS TABLE OF / VARRAY：集合类型解释器未实现，曾被声明段静默丢弃
    for decl in [
        "TYPE t_num IS TABLE OF NUMBER;",
        "TYPE t_vc IS VARRAY(10) OF VARCHAR2;",
    ] {
        let err = register_err(&format!(
            "CREATE OR REPLACE PROCEDURE prc_ty IS {decl} BEGIN NULL; END;"
        ));
        assert_eq!(err.code.to_string(), "ENG-0071", "{decl}: {err:?}");
    }
}

#[test]
fn should_not_reject_savepoint_text_inside_string_literal() {
    // AST-only 判定：字符串字面量中的同名文本不是语句，不得误杀
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_lit(p_o OUT VARCHAR2) IS BEGIN \
         p_o := 'SAVEPOINT sp1'; END;",
    )
    .expect("字符串字面量中的 SAVEPOINT 不得触发门禁");
}

#[test]
fn should_reject_returning_into_and_bulk_collect_with_eng_0071() {
    // Oracle 批量取数/RETURNING INTO：无 DuckDB 语义，编译期显式拒绝
    for stmt in [
        "UPDATE t1 SET a = 1 WHERE id = 1 RETURNING id INTO v_id",
        "DELETE FROM t1 WHERE id = 1 RETURNING id INTO v_id",
        "INSERT INTO t1 (a) VALUES (1) RETURNING id INTO v_id",
        "SELECT a BULK COLLECT INTO v_ids FROM t1",
    ] {
        let sql = format!(
            "CREATE OR REPLACE PROCEDURE prc_r IS \
             v_id NUMBER; v_ids NUMBER; BEGIN {stmt}; END;"
        );
        let err = engine()
            .register(&sql)
            .expect_err("RETURNING INTO / BULK COLLECT 必须编译期拒绝");
        assert_eq!(err.code.to_string(), "ENG-0071", "{stmt}: {err:?}");
    }
}
