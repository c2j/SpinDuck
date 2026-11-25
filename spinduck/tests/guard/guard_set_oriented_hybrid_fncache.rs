//! 回归守护（AGENTS §4.3）：SO 行级混合重放的例程缓存命名冲突（V55.4 残留面）。
//!
//! 场景：byfund_branch 语料。游标 WHERE 例程 `fnc_get_fund_data_mode` 的
//! SO 计划缓存名为 statement 级 `sd_fn0`；异常行重放走解释器，INSERT 位
//! 例程 `fnc_get_rate_info2` 的 fn_site 同样生成 `sd_fn0`——重放
//! `CREATE OR REPLACE` 覆写游标缓存。重放结束后 `miss_outs`（NO_DATA_FOUND
//! 仿真标量出口）内嵌 verbatim cur_scan，其 `sd_fn0` 子查询读到的是
//! rate_info2 的键形态 → 全行不匹配 → miss_out 返回空 → 帧变量
//! （`p_o_succeed` 等）与 Off 分歧。
//!
//! 判别种子：追加一行 `security_id='0130000003'`（par_sys_bond 无此债券
//! → v_temp_stock_kind NO_DATA_FOUND miss）的普通分组行——miss_out
//! 路径的唯一触发源。修复前：重放覆写 `sd_fn0` → miss_out 空 →
//! `p_o_succeed` 偏离 Off；修复后：计划游标缓存改名 `sd_pfnN`，
//! 与解释器 `sd_fn{i}` 命名空间隔离 → 与 Off 逐值一致。

#![cfg(feature = "native")]
use spinduck_core::exec::{ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::project::ProjectConfig;
use spinduck_core::verify::deploy_project;
use spinduck_core::{BindMap, DuckExecutor, Engine, EngineError, RuleRegistry, SpinValue};
use std::collections::BTreeMap;
use std::path::Path;

struct DE(DuckExecutor);
impl SqlExecutor for DE {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.query(sql, binds)
    }
    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.0.execute(sql, binds)
    }
}

fn args() -> [SpinValue; 2] {
    [SpinValue::Str("20250715".into()), SpinValue::Null]
}

fn deploy(mode: SetOrientedMode) -> Engine {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("golden/case_corpus_purchase");
    let project = ProjectConfig::load(&root).unwrap();
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap();
    let mut eng = Engine::new(Box::new(DE(DuckExecutor::open_in_memory().unwrap())), rules);
    deploy_project(&mut eng, &project).unwrap();
    for t in [
        "mid_yjqs_detail",
        "par_fund_info",
        "par_fund_partner",
        "par_fund_seat",
        "par_fund_check_para",
        "par_fund_seat_para",
        "par_sys_purchase",
        "par_sys_bond",
        "par_sys_securities",
        "dic_stock_kind_sub",
        "dic_all_kind",
        "swh_all_kind",
        "par_oper_venture",
        "jyfy_temp",
        "qfqscjhbrbb_temp",
        "dat_fund_cjqs",
        "dat_fund_xw_venture",
        "dat_trd_repurchase",
        "dat_xw_venture",
        "db_log",
    ] {
        let _ = eng.ex.execute(&format!("DELETE FROM {t}"), &BindMap::new());
    }
    let seed =
        std::fs::read_to_string(root.join("cases/datasets_branch/seed_branch_byfund.sql")).unwrap();
    for stmt in seed.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        eng.ex.execute(stmt, &BindMap::new()).unwrap();
    }
    eng.set_oriented = mode;
    eng.loop_parallel = spinduck_core::ir::loop_parallel::LoopParallelConfig {
        mode: spinduck_core::ir::loop_parallel::LoopParallelMode::Off,
        threads: 8,
        chunk_size: 256,
        min_rows: 256,
    };
    eng
}

fn snapshot(eng: &Engine) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (table, cols) in [
        (
            "dat_fund_cjqs",
            "fund_code, xwdm, security_id, bs, cjsl, cjje, accrual, accural_tax, mrcb",
        ),
        ("dat_fund_xw_venture", "fund_code, xwdm, qsje, venture_type"),
    ] {
        let mut s = eng
            .ex
            .query(
                &format!("SELECT {cols} FROM {table} ORDER BY 1,2,3,4"),
                &BindMap::new(),
            )
            .unwrap();
        let rows = s.fetch_many(usize::MAX);
        out.insert(
            table.to_string(),
            rows.iter()
                .map(|r| {
                    r.iter()
                        .map(|v| format!("{v:?}"))
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect(),
        );
    }
    out
}

/// 混合路径下 miss_out 消费的游标例程缓存不得被重放覆写：
/// BatchAtomic（SO+LP 混合）与 Off 的 OUT/表状态须逐值一致，
/// 且 byfund 段确实走了 Converted（否则等价断言不触达本缺陷面）。
#[test]
fn should_match_off_when_replay_overwrites_cursor_fn_cache() {
    let mut off = deploy(SetOrientedMode::Off);
    let mut ba = deploy(SetOrientedMode::BatchAtomic);
    let off_res = off
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .expect("Off 基线不得失败");
    let ba_res = ba
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .expect("BatchAtomic 不得失败");
    assert!(
        ba.set_oriented_reports().iter().any(|r| matches!(
            r.status,
            spinduck_core::ir::set_oriented::Status::Converted { .. }
        )),
        "byfund 段须走 SO Converted 才能触达混合重放路径"
    );
    assert_eq!(
        off_res.outs, ba_res.outs,
        "OUT 形参须与 Off 逐值一致（miss_out 帧变量 p_o_succeed）"
    );
    assert_eq!(
        snapshot(&off),
        snapshot(&ba),
        "dat_fund_cjqs（含 accrual/accrual_tax）与 dat_fund_xw_venture 须逐值一致"
    );
}
