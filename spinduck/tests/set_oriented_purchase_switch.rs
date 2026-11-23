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
use spinduck_core::ir::set_oriented::{Arm, SetOrientedMode, Status};
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

/// 一次调用的可观察结果：`dat_trd_repurchase` 全行（确定性序）+
/// `p_o_succeed` OUT 值——四象限等价门的比对面。
fn observe(eng: &mut Engine, kind_id: Option<&str>) -> (String, String, Vec<Status>) {
    match kind_id {
        Some(k) => set_switch(eng, k),
        None => {
            eng.ex
                .execute(
                    "DELETE FROM swh_all_kind WHERE operation_kind='COMMISSION_SWITCH'",
                    &BindMap::new(),
                )
                .expect("删除 switch 行");
        }
    }
    let args = [SpinValue::Str("20250715".to_string()), SpinValue::Null];
    let r = eng
        .call("pkg_split_trade_step3_sh.prc_trd_deal_purchase", &args)
        .expect("例程调用");
    let succeed = r
        .outs
        .get("p_o_succeed")
        .map(|v| format!("{v:?}"))
        .unwrap_or_default();
    let mut rs = eng
        .ex
        .query(
            "SELECT * FROM dat_trd_repurchase ORDER BY security_id, purchase_no",
            &BindMap::new(),
        )
        .expect("产物表读取");
    let rows = rs
        .fetch_many(1000)
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let statuses = eng
        .set_oriented_reports()
        .iter()
        .map(|r| r.status.clone())
        .collect();
    (rows, succeed, statuses)
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
            // BatchAtomic + D7 双臂（§12.3/§12.6）：route 形状命中后
            // 两臂独立走查+生成，判别式在运行期选臂——OFF 象限选
            // ELSE、ON 象限选 THEN，均 Converted 且 arm 标签可断言
            // （测试 17 的同源口径：ADMIT 与 SET_ORIENTED 一致）。
            // 双臂引入前的基线（V53）：ON = Skipped(UnsupportedControlFlow)。
            SetOrientedMode::BatchAtomic => {
                assert!(
                    off.iter().all(|s| matches!(
                        s,
                        Status::Converted {
                            arm: Some(Arm::Else),
                            ..
                        }
                    )),
                    "{}",
                    describe(&off)
                );
                assert!(
                    on.iter().all(|s| matches!(
                        s,
                        Status::Converted {
                            arm: Some(Arm::Then),
                            ..
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

/// D7 四象限等价门（§12.6 测试 16 / §12.7 门 3）：BatchAtomic 双臂批写
/// 与 Off 解释器在同一 fixture 上逐值比对 `dat_trd_repurchase` 全列 +
/// `p_o_succeed`。象限：`'0'`→ELSE 臂、`'1'`→THEN 臂、缺席行→查找
/// miss 兜底值域。NULL 判别值象限在本 fixture 不可达——`kind_id`
/// 列 NOT NULL 约束 + miss 兜底为非 NULL 常量；NotTotal 路径由
/// 单测（合成判别式）覆盖。
#[test]
fn d7_purchase_batch_matches_interpreter_all_quadrants() {
    for (label, kind_id) in [("else", Some("0")), ("then", Some("1")), ("absent", None)] {
        // 基线：Off = 纯解释器（零集合化）。
        let mut off_eng = purchase_engine();
        off_eng.set_oriented = SetOrientedMode::Off;
        let (off_rows, off_succeed, _) = observe(&mut off_eng, kind_id);
        // 对照：BatchAtomic = D7 双臂。
        let mut ba_eng = purchase_engine();
        ba_eng.set_oriented = SetOrientedMode::BatchAtomic;
        let (ba_rows, ba_succeed, statuses) = observe(&mut ba_eng, kind_id);
        assert_eq!(
            ba_rows, off_rows,
            "{label} 象限 dat_trd_repurchase 与解释器不一致"
        );
        assert_eq!(
            ba_succeed, off_succeed,
            "{label} 象限 p_o_succeed 与解释器不一致"
        );
        let arm = statuses.iter().find_map(|s| match s {
            Status::Converted { arm, .. } => Some(*arm),
            _ => None,
        });
        let skipped = statuses.iter().find_map(|s| match s {
            Status::Skipped { reason } => Some(*reason),
            _ => None,
        });
        match label {
            "else" => assert_eq!(arm, Some(Some(Arm::Else)), "{label}: {statuses:?}"),
            "then" => assert_eq!(arm, Some(Some(Arm::Then)), "{label}: {statuses:?}"),
            "absent" => {
                // miss 兜底值进 ELSE 值域或判 NotTotal——两者都须与
                // 解释器逐值一致（上面已锁），此处只锁不静默 Converted。
                assert!(arm.is_some() || skipped.is_some(), "{label}: {statuses:?}");
            }
            _ => unreachable!(),
        }
    }
}
