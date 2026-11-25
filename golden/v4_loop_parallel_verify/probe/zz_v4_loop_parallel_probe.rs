//! V4 loop-parallel 真实语料验证探针（AGENTS §4.3 差分口径）。
//!
//! 对 purchase（`prc_trd_deal_purchase`）与 byfund（`prc_trd_hz_byfund`）
//! 两个存储过程做 loop-parallel `Off` vs `Safe` 差分：
//! 1. 正确性：同一份种子上 OUT 形参 + 全部用户表行集逐值全等；
//! 2. 性能：规模化数据（游标行数 > min_rows）多轮墙钟对比；
//! 3. 可观测：打印 `loop_parallel_reports()` 判定（Parallelized/Skipped）。
//!
//! 由 `golden/v4_loop_parallel_verify/run_verify.sh` 注入 `spinduck/tests/`
//! 运行；本文件不进 crate 编译面。

#![cfg(feature = "native")]

use spinduck_core::exec::{ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::ir::loop_parallel::{LoopParallelConfig, LoopParallelMode};
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::project::ProjectConfig;
use spinduck_core::verify::deploy_project;
use spinduck_core::{BindMap, DuckExecutor, Engine, EngineError, RuleRegistry, SpinValue};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

struct DE(DuckExecutor);
impl SqlExecutor for DE {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.query(sql, binds)
    }
    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.0.execute(sql, binds)
    }
}

fn corpus_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("golden/case_corpus_purchase")
}

fn args() -> [SpinValue; 2] {
    [SpinValue::Str("20250715".into()), SpinValue::Null]
}

/// 部署 purchase 语料工程（DDL + 例程），不灌业务种子。
fn deploy_empty() -> Engine {
    let root = corpus_root();
    let project = ProjectConfig::load(&root).unwrap();
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap();
    let mut eng = Engine::new(Box::new(DE(DuckExecutor::open_in_memory().unwrap())), rules);
    deploy_project(&mut eng, &project).unwrap();
    eng
}

fn exec_seed(eng: &mut Engine, rel: &str) {
    let sql = std::fs::read_to_string(corpus_root().join(rel)).unwrap();
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let _ = eng.ex.execute(stmt, &BindMap::new());
    }
}

/// purchase 种子（cases/datasets/seed.sql）。
fn seed_purchase(eng: &mut Engine) {
    exec_seed(eng, "cases/datasets/seed.sql");
}

/// byfund 种子：清空共享业务表后灌 cases/datasets_byfund/seed.sql
/// （与 guard_set_oriented_byfund_corpus 同口径）。
fn seed_byfund(eng: &mut Engine) {
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
    exec_seed(eng, "cases/datasets_byfund/seed.sql");
}

/// 分支覆盖种子（byfund 版）：清空 branch seed 涉及的全部表后灌
/// cases/datasets_branch/seed_branch_byfund.sql（BRANCH_COVERAGE.md）。
fn seed_branch_byfund(eng: &mut Engine) {
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
    exec_seed(eng, "cases/datasets_branch/seed_branch_byfund.sql");
}

/// 分支覆盖种子（purchase 版）：与 byfund 版同口径清表后灌
/// seed_branch_purchase.sql（文件自含全部维度 + purchase 行，
/// 不含 byfund 明细——避免 bs='1' 行混入 purchase 数据集）。
fn seed_branch_purchase(eng: &mut Engine) {
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
    exec_seed(eng, "cases/datasets_branch/seed_branch_purchase.sql");
}

fn set_mode(eng: &mut Engine, mode: LoopParallelMode) {
    eng.loop_parallel = LoopParallelConfig {
        mode,
        threads: 8,
        chunk_size: 256,
        min_rows: 256,
    };
}

fn set_mode_min_rows(eng: &mut Engine, mode: LoopParallelMode, min_rows: usize) {
    eng.loop_parallel = LoopParallelConfig {
        mode,
        threads: 8,
        chunk_size: 256,
        min_rows,
    };
}

/// 全部用户表（排除 sd_* 物化临时表）行集快照：行内容排序后逐表全等。
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

fn report_lines(eng: &Engine) -> String {
    eng.loop_parallel_reports()
        .iter()
        .map(|r| format!("{:?}", r.status))
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// 差分主流程：Off 基线 vs Safe 实测。返回 (outs 相等?, 状态行)。
fn diff_call(routine: &str, seed: fn(&mut Engine), scale: Option<&str>) -> (bool, String) {
    diff_call_min(routine, seed, scale, 256, None)
}

/// 同 `diff_call`，可指定 min_rows 与额外 SQL（两侧同施加，如切换维度开关）。
fn diff_call_min(
    routine: &str,
    seed: fn(&mut Engine),
    scale: Option<&str>,
    min_rows: usize,
    extra: Option<&str>,
) -> (bool, String) {
    let mut off = deploy_empty();
    seed(&mut off);
    if let Some(sql) = scale {
        off.ex.execute(sql, &BindMap::new()).unwrap();
    }
    if let Some(sql) = extra {
        off.ex.execute(sql, &BindMap::new()).unwrap();
    }
    set_mode_min_rows(&mut off, LoopParallelMode::Off, min_rows);
    let expected = off.call(routine, &args()).unwrap();
    let expected_state = snapshot(&off);

    let mut eng = deploy_empty();
    seed(&mut eng);
    if let Some(sql) = scale {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    if let Some(sql) = extra {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    set_mode_min_rows(&mut eng, LoopParallelMode::Safe, min_rows);
    let actual = eng.call(routine, &args()).unwrap();

    let outs_eq = actual.outs == expected.outs;
    let state_eq = snapshot(&eng) == expected_state;
    let status = report_lines(&eng);
    println!(
        "V4|{routine}|equiv|outs={} state={} status={status}",
        if outs_eq { "EQ" } else { "DIFF" },
        if state_eq { "EQ" } else { "DIFF" },
    );
    assert!(
        outs_eq,
        "{routine} OUT 形参与 Off 分歧: {actual:?} vs {expected:?}"
    );
    assert!(state_eq, "{routine} 用户表行集与 Off 分歧");
    (outs_eq && state_eq, status)
}

/// 规模化性能：同一引擎上多轮 `call` 计时（过程开头自清目标表，
/// 重复调用幂等）。Off/Safe 交替各 R 轮，取最小值对比。
fn perf_call(routine: &str, seed: fn(&mut Engine), scale: &str, rounds: usize) {
    let mut off_times: Vec<u128> = Vec::new();
    let mut safe_times: Vec<u128> = Vec::new();
    let mut last_status = String::new();
    for mode in [LoopParallelMode::Off, LoopParallelMode::Safe] {
        let mut eng = deploy_empty();
        seed(&mut eng);
        eng.ex.execute(scale, &BindMap::new()).unwrap();
        set_mode(&mut eng, mode);
        // 预热一轮（编译/缓存预热不计入）
        let _ = eng.call(routine, &args()).unwrap();
        for _ in 0..rounds {
            let t = Instant::now();
            let _ = eng.call(routine, &args()).unwrap();
            if mode == LoopParallelMode::Off {
                off_times.push(t.elapsed().as_millis());
            } else {
                safe_times.push(t.elapsed().as_millis());
            }
        }
        if mode == LoopParallelMode::Safe {
            last_status = report_lines(&eng);
        }
    }
    let off = off_times.iter().min().unwrap();
    let safe = safe_times.iter().min().unwrap();
    println!(
        "V4|{routine}|perf|off_min={off}ms safe_min={safe}ms \
         off_all={off_times:?} safe_all={safe_times:?} status={last_status}",
    );
}

// purchase 游标行放大（V8 终验口径：约 10 万游标行 / 并行度 8）：
// 3 行明细 × 16000 → 约 96k 明细行（实测游标行 ≈ 2× 明细数）。
// cjbh 加 range 后缀保证行级可区分；bs 保持 'B' 数值形态。
const PURCHASE_SCALE: &str = "INSERT INTO mid_yjqs_detail \
    SELECT * EXCLUDE (range) REPLACE (cjbh || CAST(range AS VARCHAR) AS cjbh) \
    FROM mid_yjqs_detail, range(16000) \
    WHERE stock_kind LIKE '05%' AND scdm = '001'";

// byfund c_get_data 分组放大：2 行 × 500 个 bs 变体 → 约 1000 组
// （与 guard_set_oriented_byfund_corpus 同口径）。
const BYFUND_SCALE: &str = "INSERT INTO mid_yjqs_detail \
    SELECT * EXCLUDE (range) REPLACE (CAST(9 + range AS VARCHAR) AS bs) \
    FROM mid_yjqs_detail, range(500) \
    WHERE bcrq = '20250715' AND scdm = '001'";

/// byfund 明细放大（基础/分支种子通用）：5 行 × N 个 bs 变体 → 5N 个 c_get_data 分组。
/// bs 变体保持数值形态（`to_number(bs)` 参与计算）。
fn byfund_scale(n: usize) -> String {
    format!(
        "INSERT INTO mid_yjqs_detail \
         SELECT * EXCLUDE (range) REPLACE (CAST(9 + range AS VARCHAR) AS bs) \
         FROM mid_yjqs_detail, range({n}) \
         WHERE bcrq = '20250715' AND scdm = '001'"
    )
}

/// purchase 分支种子放大：9 行（stock_kind '05%'，含 1 行 '0509'
/// 游标排除样本）× N → 约 2N×9 游标行。N=5000 → 约 9 万游标行。
fn purchase_branch_scale(n: usize) -> String {
    format!(
        "INSERT INTO mid_yjqs_detail \
         SELECT * EXCLUDE (range) REPLACE (cjbh || CAST(range AS VARCHAR) AS cjbh) \
         FROM mid_yjqs_detail, range({n}) \
         WHERE stock_kind LIKE '05%' AND scdm = '001'"
    )
}

// ---------------------------------------------------------------------------
// 三方性能矩阵（SO vs LP vs 双开）：同一场景数据下 Off / set-oriented
// BatchAtomic / loop-parallel Safe / SO+LP 组合的最小墙钟对比。
// set_oriented 在 run_selected 中先于 loop_parallel 裁决——区域可
// 集合化时 LP 不再介入；SO+LP 列用于验证该优先级与回退路径。
// ---------------------------------------------------------------------------

/// 单场景单配置计时：预热 1 轮后取 `rounds` 轮最小值；返回
/// (min_ms, set_oriented 状态摘要, loop_parallel 状态摘要, outs)。
fn perf_one(
    routine: &str,
    args: &[SpinValue],
    seed: fn(&mut Engine),
    scale: Option<&str>,
    extra: Option<&str>,
    so_mode: SetOrientedMode,
    lp_mode: LoopParallelMode,
    rounds: usize,
) -> (u128, String, String, spinduck_core::CallResult) {
    let mut eng = deploy_empty();
    seed(&mut eng);
    if let Some(sql) = scale {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    if let Some(sql) = extra {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.set_oriented = so_mode;
    eng.loop_parallel = LoopParallelConfig {
        mode: lp_mode,
        threads: 8,
        chunk_size: 256,
        min_rows: 256,
    };
    let _ = eng.call(routine, args).unwrap();
    let mut best = u128::MAX;
    let mut result = None;
    for _ in 0..rounds {
        let t = Instant::now();
        result = Some(eng.call(routine, args).unwrap());
        best = best.min(t.elapsed().as_millis());
    }
    let so = eng
        .set_oriented_reports()
        .iter()
        .map(|r| format!("{:?}", r.status))
        .collect::<Vec<_>>()
        .join(";");
    let lp = report_lines(&eng);
    (best, so, lp, result.unwrap())
}

/// 场景矩阵：4 配置 × N 轮，逐配置打印 `V4PERF|` 行并断言
/// OUT 形参跨配置全等（状态等价由各 equiv 用例另行锁定）。
fn perf_matrix(
    routine: &str,
    label: &str,
    seed: fn(&mut Engine),
    scale: Option<&str>,
    extra: Option<&str>,
    rounds: usize,
) {
    let args = args();
    let configs = [
        (SetOrientedMode::Off, LoopParallelMode::Off, "off"),
        (SetOrientedMode::BatchAtomic, LoopParallelMode::Off, "so_ba"),
        (SetOrientedMode::Off, LoopParallelMode::Safe, "lp_safe"),
        (
            SetOrientedMode::BatchAtomic,
            LoopParallelMode::Safe,
            "so_lp",
        ),
    ];
    let mut baseline: Option<spinduck_core::CallResult> = None;
    for (so, lp, tag) in configs {
        let (ms, so_st, lp_st, res) =
            perf_one(routine, &args, seed, scale, extra, so, lp, rounds);
        match &baseline {
            None => baseline = Some(res.clone()),
            Some(b) => assert_eq!(res.outs, b.outs, "{label}/{tag} OUT 与 Off 分歧"),
        }
        println!(
            "V4PERF|{label}|{tag}|min_ms={ms} so=[{so_st}] lp=[{lp_st}]",
        );
    }
}

/// purchase 小规模等价（3 行种子，低于 min_rows → 必须 Skipped/串行等价）。
#[test]
fn v4_purchase_small_scale_equiv() {
    diff_call(
        "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
        seed_purchase,
        None,
    );
}

/// purchase 规模化等价（约 10 万游标行，触发 min_rows 门槛）。
#[test]
fn v4_purchase_scale_equiv() {
    diff_call(
        "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
        seed_purchase,
        Some(PURCHASE_SCALE),
    );
}

/// purchase 规模化性能（约 10 万游标行，Off vs Safe 各 3 轮）。
#[test]
fn v4_purchase_perf() {
    perf_call(
        "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
        seed_purchase,
        PURCHASE_SCALE,
        3,
    );
}

/// byfund 小规模等价。
#[test]
fn v4_byfund_small_scale_equiv() {
    diff_call(
        "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
        seed_byfund,
        None,
    );
}

/// byfund 规模化等价（约 1000 个 c_get_data 分组）。
#[test]
fn v4_byfund_scale_equiv() {
    diff_call(
        "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
        seed_byfund,
        Some(BYFUND_SCALE),
    );
}

/// byfund 规模化性能（Off vs Safe 各 5 轮）。
#[test]
fn v4_byfund_perf() {
    perf_call(
        "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
        seed_byfund,
        BYFUND_SCALE,
        5,
    );
}

// ---------------------------------------------------------------------------
// 分支覆盖验收（BRANCH_COVERAGE.md）：min_rows=1 强制小规模数据也走
// 并行路径，使全部数据可触发分支同时经受 Off/Safe 差分。
// ---------------------------------------------------------------------------

/// purchase 分支覆盖（swh_all_kind='0' → P5 ELSE 查询路径）。
#[test]
fn v4_purchase_branch_equiv() {
    diff_call_min(
        "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
        seed_branch_purchase,
        None,
        1,
        None,
    );
}

/// purchase 分支覆盖（swh_all_kind='1' → P4 fnc_com_get_seat_para + CASE 臂路径）。
#[test]
fn v4_purchase_branch_kind1_equiv() {
    diff_call_min(
        "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
        seed_branch_purchase,
        None,
        1,
        Some("UPDATE swh_all_kind SET kind_id='1' WHERE operation_kind='COMMISSION_SWITCH'"),
    );
}

/// byfund 分支覆盖（段1 B1-B5/B10 + 段2 B7-B9 + 段3 + 段4 QFII B11-B18）。
#[test]
fn v4_byfund_branch_equiv() {
    diff_call_min(
        "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
        seed_branch_byfund,
        None,
        1,
        None,
    );
}

/// byfund 分支覆盖·NO_DATA_FOUND 变体：删除 0130000002 的
/// par_sys_bond 行 → B5 全部异常行变 NO_DATA_FOUND（同 security_id
/// → 同消息，p_o_succeed 末次写赢确定性成立）。
#[test]
fn v4_byfund_branch_nodata_equiv() {
    diff_call_min(
        "pkg_split_trade_step3_sh.prc_trd_hz_byfund",
        seed_branch_byfund,
        None,
        1,
        Some("DELETE FROM par_sys_bond WHERE security_id='0130000002'"),
    );
}

/// 分支种子串行稳定性守护：两个独立 Off 引擎跑同一分支数据，
/// `p_o_succeed`（末次异常写赢的 OUT 参）必须逐值一致——
/// 若此处即 DIFF，说明游标 GROUP BY 行序本身不稳定，
/// 属种子设计问题而非并行缺陷。
#[test]
fn v4_byfund_branch_serial_stability() {
    let mut a = deploy_empty();
    seed_branch_byfund(&mut a);
    set_mode_min_rows(&mut a, LoopParallelMode::Off, 1);
    let ra = a
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();

    let mut b = deploy_empty();
    seed_branch_byfund(&mut b);
    set_mode_min_rows(&mut b, LoopParallelMode::Off, 1);
    let rb = b
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();

    println!(
        "V4|byfund|serial_stability|outs={} state={}",
        if ra.outs == rb.outs { "EQ" } else { "DIFF" },
        if snapshot(&a) == snapshot(&b) {
            "EQ"
        } else {
            "DIFF"
        },
    );
    assert_eq!(ra.outs, rb.outs, "串行两次 p_o_succeed 分歧（行序不稳定）");
}

// ---------------------------------------------------------------------------
// SO vs LP 性能矩阵验收（100k 量级 / 并行度 8）。
// 场景按分支族分开验证：purchase happy-path / purchase 分支 kind='0' /
// purchase 分支 kind='1'；byfund 基础种子 / byfund 分支种子（含异常行）。
// ---------------------------------------------------------------------------

const PURCHASE_ROUTINE: &str = "pkg_split_trade_step3_sh.prc_trd_deal_purchase";
const BYFUND_ROUTINE: &str = "pkg_split_trade_step3_sh.prc_trd_hz_byfund";

/// purchase 场景1：基础种子 happy path（P5 ELSE 查询路径），约 9.6 万游标行。
#[test]
fn v4_perf_purchase_base() {
    perf_matrix(
        PURCHASE_ROUTINE,
        "purchase_base",
        seed_purchase,
        Some(PURCHASE_SCALE),
        None,
        3,
    );
}

/// purchase 场景2：分支种子放大（P0 排除 + P1/P2 MISS error_row +
/// P5 ELSE），kind='0'，约 9 万游标行。
#[test]
fn v4_perf_purchase_branch_k0() {
    perf_matrix(
        PURCHASE_ROUTINE,
        "purchase_branch_k0",
        seed_branch_purchase,
        Some(&purchase_branch_scale(5000)),
        None,
        3,
    );
}

/// purchase 场景3：同场景2 但 swh_all_kind 切 '1'（P4 seat_para+CASE 臂）。
#[test]
fn v4_perf_purchase_branch_k1() {
    perf_matrix(
        PURCHASE_ROUTINE,
        "purchase_branch_k1",
        seed_branch_purchase,
        Some(&purchase_branch_scale(5000)),
        Some("UPDATE swh_all_kind SET kind_id='1' WHERE operation_kind='COMMISSION_SWITCH'"),
        3,
    );
}

/// byfund 场景1：基础种子放大（全命中 happy path），2 行 × 2500 → 约 5000 分组。
#[test]
fn v4_perf_byfund_base() {
    perf_matrix(
        BYFUND_ROUTINE,
        "byfund_base",
        seed_byfund,
        Some(&byfund_scale(2500)),
        None,
        2,
    );
}

/// byfund 场景2：分支种子放大（B1-B5 混合臂 + B10 + 段2/3/4），
/// 5 行 × 1000 → 约 5000 分组。
#[test]
fn v4_perf_byfund_branch() {
    perf_matrix(
        BYFUND_ROUTINE,
        "byfund_branch",
        seed_branch_byfund,
        Some(&byfund_scale(1000)),
        None,
        2,
    );
}
