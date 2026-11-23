//! G5 重测（设计 v3.1 §12.4/§12.7）：purchase 语料 `prc_trd_deal_purchase`
//! 在 switch='0'/'1' 两象限下的当前 main 行为取证——`set_oriented_reports`
//! 即 `SET_ORIENTED` 行的同一数据源。PR-5 门 1「逐字一致」基线由本测试
//! 的实测产出，结果登记 docs/validation.md。
//!
//! 数据前置（§12.7）：仓内 seed 固定 `kind_id='0'`；ON 象限由测试内
//! `UPDATE swh_all_kind` 翻转产生，不改语料资产。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
use spinduck_core::project::ProjectConfig;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use spinduck_core::verify::deploy_project;
use std::path::{Path, PathBuf};

fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .join("golden")
}

fn purchase_engine() -> Engine {
    let root = golden_root().join("case_corpus_purchase");
    let project = ProjectConfig::load(&root).expect("项目加载");
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .expect("规则加载");
    let ex = DuckExecutor::open_in_memory().expect("open duckdb");
    let mut eng = Engine::new(Box::new(ex), rules);
    deploy_project(&mut eng, &project).expect("部署");
    eng
}

fn call_purchase(eng: &mut Engine) -> Vec<Status> {
    let args = [SpinValue::Str("20250715".to_string()), SpinValue::Null];
    eng.call("pkg_split_trade_step3_sh.prc_trd_deal_purchase", &args)
        .expect("例程调用");
    eng.set_oriented_reports()
        .iter()
        .map(|r| r.status.clone())
        .collect()
}

fn set_switch(eng: &mut Engine, kind_id: &str) {
    eng.ex
        .execute(
            &format!(
                "UPDATE swh_all_kind SET kind_id='{kind_id}' \
                 WHERE operation_kind='COMMISSION_SWITCH'"
            ),
            &BindMap::new(),
        )
        .expect("翻转 switch");
}

fn describe(statuses: &[Status]) -> String {
    statuses
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn g5_retest_purchase_switch_off_then_on() {
    for mode in [SetOrientedMode::Strict, SetOrientedMode::BatchAtomic] {
        let mut eng = purchase_engine();
        eng.set_oriented = mode;
        // OFF 象限：仓内 seed kind_id='0'（THEN 不可达 → 守卫应证不可达）。
        let off = call_purchase(&mut eng);
        // ON 象限：翻转 kind_id='1'（THEN 可达 → route 守卫拒）。
        set_switch(&mut eng, "1");
        let on = call_purchase(&mut eng);
        // 复位 OFF 再取一遍，确认翻转可逆（fixture 自证，非断言对象）。
        set_switch(&mut eng, "0");
        let off2 = call_purchase(&mut eng);
        eprintln!(
            "G5 mode={mode:?}\n  off  = {}\n  on   = {}\n  off2 = {}",
            describe(&off),
            describe(&on),
            describe(&off2)
        );
        assert_eq!(off, off2, "{mode:?} OFF 象限翻转后未复原");
        match mode {
            // Strict 在更早的循环内 COMMIT 门槛就拒掉，到不了 route 守卫。
            SetOrientedMode::Strict => {
                for s in off.iter().chain(on.iter()) {
                    assert!(
                        matches!(
                            s,
                            Status::Skipped {
                                reason: spinduck_core::ir::set_oriented::SkipReason::TransactionBoundaryInsideLoop
                            }
                        ),
                        "{s:?}"
                    );
                }
            }
            // BatchAtomic：OFF 证 THEN 不可达 → Converted；ON 守卫拒 →
            // Skipped(UnsupportedControlFlow) 走解释回退（33.9s 静默退回的来源）。
            SetOrientedMode::BatchAtomic => {
                assert!(
                    off.iter().all(|s| matches!(s, Status::Converted { .. })),
                    "{}",
                    describe(&off)
                );
                assert!(
                    on.iter().any(|s| matches!(
                        s,
                        Status::Skipped {
                            reason:
                                spinduck_core::ir::set_oriented::SkipReason::UnsupportedControlFlow
                        }
                    )),
                    "{}",
                    describe(&on)
                );
            }
            SetOrientedMode::Off => unreachable!("Off 不产生报告"),
        }
    }
}
