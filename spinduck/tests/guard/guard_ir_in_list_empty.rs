//! 回归守护（AGENTS §4.3）：`IN ()` 空列表脱糖。
//!
//! `fold_bin` 曾对空部件链 `it.next().expect(...)` panic；现空表按 Oracle
//! 语义短路（`x IN ()` = FALSE，`x NOT IN ()` = TRUE），解析器拒绝的形态
//! 显式报错而非 panic。

use spinduck_core::engine::Engine;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;

#[test]
fn should_not_panic_on_empty_in_list() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let src = "CREATE OR REPLACE PROCEDURE prc_empty_in(p_v IN NUMBER) IS \
               BEGIN IF p_v IN () THEN NULL; END IF; END;";
    // 解析器接受 → 按语义编译（空表 = FALSE）；拒绝 → 不登记（0 条）。
    // 两条路径都不得 panic。
    let _ = eng.register(src).expect("register 不得 panic");
    if eng.catalog.lookup("prc_empty_in").is_some() {
        let r = eng
            .call("prc_empty_in", &[spinduck_core::value::SpinValue::Int(1)])
            .expect("空 IN 表不得在运行期 panic");
        // x IN () = FALSE：即便命中分支也不应有返回值写入
        assert!(r.outs.is_empty());
    }
}
