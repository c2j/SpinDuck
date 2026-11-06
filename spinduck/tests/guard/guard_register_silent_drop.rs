//! 行为锁定（AGENTS §4.3）：`register` 不得静默丢弃声明了的例程。
//!
//! ogsql-parser 对个别 PL 构造（如 `IN ()`）会把整条 `CREATE PROCEDURE` 降级成
//! `Empty`/其它形态（§1：只记录，不在其仓库内动手）。此前引擎照单返回
//! `Ok(0)`——语料接入时表现为"存过凭空消失"，且黄金比对只会看到 BIZ-0150
//! not registered。现归域为 ENGINE-0064：声明了例程却一条都没登记 = 引擎侧
//! 输入被解析器吞掉，必须显式失败。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;

fn engine() -> Engine {
    Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    )
}

/// 包规格（spec）合法地不产出例程名——只有包体才编译例程，不得误判为丢弃。
#[test]
fn should_allow_package_spec_that_defines_no_bodies() {
    let mut eng = engine();
    let n = eng
        .register(
            "CREATE OR REPLACE PACKAGE pkg_demo IS \
             PROCEDURE prc_a(p IN NUMBER); \
             END pkg_demo;",
        )
        .expect("包规格只声明，不产出例程名");
    assert_eq!(n, 0, "spec 不登记例程（体在 PACKAGE BODY 文件）");
}

#[test]
fn should_register_a_supported_procedure() {
    let mut eng = engine();
    let n = eng
        .register("CREATE OR REPLACE PROCEDURE prc_ok IS BEGIN NULL; END;")
        .expect("常规过程必须登记");
    assert_eq!(n, 1);
    assert!(eng.catalog.lookup("prc_ok").is_some());
}

/// 解析器把 `IN ()` 的整条 CREATE PROCEDURE 吞掉 → 引擎必须报错而非 Ok(0)。
#[test]
fn should_reject_routine_source_that_registers_nothing() {
    let mut eng = engine();
    let e = eng
        .register(
            "CREATE OR REPLACE PROCEDURE prc_empty_in(p_v IN NUMBER) IS \
             BEGIN IF p_v IN () THEN NULL; END IF; END;",
        )
        .expect_err("声明了例程却零登记必须显式失败");
    assert_eq!(
        e.code.to_string(),
        "ENG-0064",
        "静默丢弃属引擎侧缺口（§5 归域），实际: {e}"
    );
    assert!(
        e.message.contains("未登记任何例程"),
        "消息须点明静默丢弃，实际: {}",
        e.message
    );
}

/// 非例程文本（建表等）仍由 load 通道处理，不得被本护栏拦下。
#[test]
fn should_ignore_plain_ddl_without_routine_declaration() {
    let mut eng = engine();
    assert_eq!(eng.register("CREATE TABLE t(a INT)").unwrap(), 0);
}
