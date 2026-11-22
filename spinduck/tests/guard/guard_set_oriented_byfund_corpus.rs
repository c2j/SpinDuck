//! 回归守护（AGENTS §4.3）：byfund 语料区域 0（c_get_data）的
//! 集合化端到端等价性。
//!
//! 锁定两条不变量：
//! 1. 提交进仓库的 `datasets_byfund` 种子上，区域 0 必须
//!    `BatchAtomic Converted`，且 OUT 形参 + 全部用户表行集与 Off
//!    逐行全等（业务成功断言，非"两模式同错"）。
//! 2. 规模化数据（克隆明细行 ×500 → 约 1000 个分组）上仍必须
//!    `Converted`——防止规模增长后静默回退成 Skipped。

#![cfg(feature = "native")]
use spinduck_core::exec::{ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
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

/// purchase 语料 + byfund 种子部署；返回已配置集合化模式的引擎。
fn deploy_seeded(mode: SetOrientedMode) -> Engine {
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
    // datasets_byfund 种子：清空共享业务表后灌入（与 purchase 种子表名相同）
    for t in [
        "mid_yjqs_detail",
        "par_fund_info",
        "par_fund_partner",
        "par_fund_seat",
        "par_oper_venture",
        "dic_all_kind",
    ] {
        let _ = eng.ex.execute(&format!("DELETE FROM {t}"), &BindMap::new());
    }
    let seed = std::fs::read_to_string(root.join("cases/datasets_byfund/seed.sql")).unwrap();
    for stmt in seed.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if let Err(e) = eng.ex.execute(stmt, &BindMap::new()) {
            eprintln!("SEED_SKIP {} -> {}", &stmt[..stmt.len().min(60)], e.message);
        }
    }
    eng.set_oriented = mode;
    eng
}

/// 全部用户表（排除 sd_* 物化临时表）行集快照：行内容排序后逐表全等，
/// 不依赖列序/ORDER BY 语义。
fn snapshot(eng: &Engine) -> BTreeMap<String, Vec<String>> {
    let names = eng
        .ex
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'main' ORDER BY 1",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10_000);
    let mut out = BTreeMap::new();
    for row in names {
        let SpinValue::Str(name) = &row[0] else {
            continue;
        };
        if name.starts_with("sd_") {
            continue;
        }
        let mut rows: Vec<String> = eng
            .ex
            .query(&format!("SELECT * FROM \"{name}\""), &BindMap::new())
            .unwrap()
            .fetch_many(2_000_000)
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        rows.sort();
        out.insert(name.clone(), rows);
    }
    out
}

/// 真实 purchase 语料端到端：byfund 区域 0（c_get_data）必须
/// BatchAtomic Converted，且 OUT 形参 + 全部用户表行集与 Off 逐行全等。
#[test]
fn should_convert_byfund_region0_and_match_off_row_for_row() {
    let mut off = deploy_seeded(SetOrientedMode::Off);
    let expected = off
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();
    let expected_state = snapshot(&off);

    let mut eng = deploy_seeded(SetOrientedMode::BatchAtomic);
    let actual = eng
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();

    let reports = eng.set_oriented_reports();
    assert_eq!(reports.len(), 1, "仅区域 0 应产生报告: {reports:?}");
    assert!(
        matches!(reports[0].status, Status::Converted { .. }),
        "区域 0 必须 Converted: {reports:?}"
    );
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
    assert_eq!(
        snapshot(&eng),
        expected_state,
        "全部用户表须与 Off 逐行全等"
    );
}

/// 规模化守护：把 2 行明细克隆为约 1000 个 GROUP BY 分组
/// （`bs` 在分组键内且 `to_number(bs)` 参与计算，取值须保持数值形态）。
/// 只跑 BatchAtomic：规模化下的 Off 逐行解释器耗时不属于 CI 预算；
/// 小规模种子已由上一测试锁定 Off/Batch 逐行全等。
#[test]
fn should_stay_converted_on_byfund_region0_at_scale() {
    let mut eng = deploy_seeded(SetOrientedMode::BatchAtomic);
    // 每行克隆 500 份、bs 取 '10'..'509'：2 行 × 500 = 1000 个分组。
    eng.ex
        .execute(
            "INSERT INTO mid_yjqs_detail              SELECT * EXCLUDE (range) REPLACE (CAST(9 + range AS VARCHAR) AS bs)              FROM mid_yjqs_detail, range(500)              WHERE bcrq = '20250715' AND scdm = '001'",
            &BindMap::new(),
        )
        .unwrap();
    let groups: i64 = eng
        .ex
        .query(
            "SELECT count(*) FROM (               SELECT fund_code, xwdm, bs, security_id, stock_kind                FROM mid_yjqs_detail WHERE bcrq = '20250715' AND scdm = '001'                GROUP BY 1,2,3,4,5)",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(1)[0][0]
        .as_str_loose()
        .parse()
        .unwrap();
    assert!(groups >= 1000, "克隆应产生 >=1000 分组，实际 {groups}");

    eng.call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();
    let reports = eng.set_oriented_reports();
    assert_eq!(reports.len(), 1, "仅区域 0 应产生报告: {reports:?}");
    assert!(
        matches!(reports[0].status, Status::Converted { .. }),
        "规模化数据下区域 0 不得回退为 Skipped: {reports:?}"
    );
}
