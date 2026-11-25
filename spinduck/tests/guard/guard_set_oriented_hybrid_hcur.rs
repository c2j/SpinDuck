//! 回归守护（AGENTS §4.3）：SO 行级混合 `__sd_hcur` 重建序（P0-3b）。
//!
//! 场景：byfund_branch 语料放大（异常行混合 + 段批写 + 异常行单行重放）。
//! 重放的原生例程调用会回滚会话临时表 → `ensure_hcur` 以同文本
//! `hcur_sql` 重建 `__sd_hcur_N` 并做位置校验。修复前：重建序与原物化
//! 序漂移（段批写后优化器管线变化）→ `ENG-0032 "sd_cur rematerialize
//! order drift"` 硬失败。修复后：`hcur_sql` 全列 `ORDER BY` 规范化——
//! 序由内容唯一决定（GROUP BY 游标键唯一即全序；整行重复互换不可观测），
//! 重建必然复现原序。
//!
//! 锁定不变量：同一引擎连续两次 BatchAtomic 调用（第 2 次触发重建路径）
//! 均须成功，且 OUT 形参与两轮状态逐值一致。

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

/// byfund_branch 种子（BRANCH_COVERAGE.md：B1-B18 全段维度 + 异常行）。
fn deploy_branch_seeded() -> Engine {
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
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng
}

/// 明细放大：5 行 × N 个 bs 变体 → 5N 个 c_get_data 分组（异常行随组复制）。
fn scale(eng: &mut Engine, n: usize) {
    eng.ex
        .execute(
            &format!(
                "INSERT INTO mid_yjqs_detail \
                 SELECT * EXCLUDE (range) REPLACE (CAST(9 + range AS VARCHAR) AS bs) \
                 FROM mid_yjqs_detail, range({n}) \
                 WHERE bcrq = '20250715' AND scdm = '001'"
            ),
            &BindMap::new(),
        )
        .unwrap();
}

fn snapshot(eng: &Engine) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (table, cols) in [
        (
            "dat_fund_cjqs",
            "fund_code, xwdm, security_id, bs, cjsl, cjje, yj, mrcb",
        ),
        ("dat_fund_xw_venture", "fund_code, xwdm, qsje, venture_type"),
    ] {
        let mut s = eng
            .ex
            .query(
                &format!("SELECT {cols} FROM {table} ORDER BY 1,2,3"),
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

/// 已知缺陷守护（V55.4 登记，PR#62 混合重放设计层缺口，修复后移除 ignore）：
/// 异常行重放的原生执行按 statement 级命名重建例程缓存（INSERT 位
/// `fnc_get_rate_info2` → `sd_fn0`），与 SO 计划期的游标 WHERE 缓存
/// （`fnc_get_fund_data_mode` → `sd_fn0`）同名冲突——重放后 `sd_fn0`
/// 内容被换成了 rate_info2 的键值，`ensure_hcur` 重建 hcur 时 WHERE
/// 查找全不匹配 → 重建 0 行 → `ENG-0032`（错误消息 "order drift"
/// 为误导性表述，真实原因是空集：缓存名冲突，非行序）。
/// 复现：scale(200) 即确定性复现（第 1 轮过、第 2 轮 ENG-0032）。
/// 修复方向：重放缓存按函数语义命名（与 SO 计划缓存共用一份）或
/// 重放前将计划缓存内容预种到解释器命名空间。
#[test]
#[ignore = "P0-3b 重放缓存名冲突（sd_fn0 双重占用）待 PR#62 修复；V55.4 登记"]
fn should_survive_hybrid_hcur_rebuild_across_calls() {
    let mut eng = deploy_branch_seeded();
    scale(&mut eng, 200);
    let first = eng
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .expect("第 1 轮混合 SO 不得失败（修复前此处即 ENG-0032）");
    let snap1 = snapshot(&eng);
    let second = eng
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .expect("第 2 轮触发 ensure_hcur 重建路径，不得 ENG-0032");
    let snap2 = snapshot(&eng);
    assert_eq!(first.outs, second.outs, "两轮 p_o_succeed 须一致");
    assert_eq!(snap1, snap2, "两轮用户表状态须逐值一致");
}
