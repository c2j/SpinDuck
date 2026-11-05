#![cfg(feature = "native")]

//! 回归守护（AGENTS §4.3）：PL 条件里的 `IN ()` 空列表。
//!
//! `fold_bin` 曾对空部件链 `it.next().expect(...)` panic；解析器侧对 PL 条件中的
//! `IN ()` 直接把整条 `CREATE PROCEDURE` 降级为 `Empty`（§1：只记录，不在其仓库
//! 内动手）。引擎的口径：SQL 上下文里空表按 Oracle 语义短路（`x IN ()` = FALSE，
//! `x NOT IN ()` = TRUE，见 ir 单测），PL 条件里则命中静默丢弃护栏
//! ENG-0064（tests/guard/guard_register_silent_drop.rs）。任何路径都不得 panic。

use spinduck_core::engine::Engine;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;

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
fn should_not_panic_on_empty_in_list() {
    big_stack(|| {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        let src = "CREATE OR REPLACE PROCEDURE prc_empty_in(p_v IN NUMBER) IS \
                   BEGIN IF p_v IN () THEN NULL; END IF; END;";
        // 行为锁定（当前口径）：ogsql-parser 不支持 PL 条件里的 `IN ()`，整条
        // CREATE 被降级 → 引擎按静默丢弃护栏显式报 ENG-0064（不 panic、不返回
        // Ok(0)）。解析器上游支持后期望值改为登记成功，须单独评审（§4.3）。
        let e = eng.register(src).expect_err("空 IN 表必须显式失败");
        assert_eq!(e.code.to_string(), "ENG-0064");
        assert!(e.message.contains("未登记任何例程"), "实际: {}", e.message);
    });
}
