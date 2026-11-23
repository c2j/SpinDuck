//! D7 性能对比取证（临时基准，非门禁测试）：purchase/byfund 语料
//! Off（逐行解释）vs BatchAtomic（集合化双臂）调用耗时对比。
//! 数据经 `mid_yjqs_detail` 自连接放大到 ~N 行，放大倍数由
//! `SCALE_POW` 控制（每次 INSERT SELECT * 翻倍）。
//! 默认跳过：`SD_BENCH=1 cargo test --test d7_perf_bench` 显式启用。

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
use std::time::Instant;

const SCALE_POW: u32 = 15; // purchase 3 行 → ~98k；byfund 2 行 → ~65k

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

fn scale_detail(eng: &mut Engine) {
    for _ in 0..SCALE_POW {
        eng.ex
            .execute(
                "INSERT INTO mid_yjqs_detail SELECT * FROM mid_yjqs_detail",
                &BindMap::new(),
            )
            .expect("scale");
    }
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
        .expect("switch");
}

fn detail_rows(eng: &Engine) -> i64 {
    let rows = eng
        .ex
        .query("SELECT COUNT(*) FROM mid_yjqs_detail", &BindMap::new())
        .expect("count")
        .fetch_many(1);
    match rows.first().and_then(|r| r.first()) {
        Some(SpinValue::Int(i)) => *i,
        _ => -1,
    }
}

/// 简明状态标签：Converted 标注选中臂，Skipped 标注拒因（不打印 SQL 本体）。
fn status_tag(status: &Status) -> String {
    match status {
        Status::Converted { arm, .. } => match arm {
            Some(a) => format!("Converted(arm={})", a.as_str()),
            None => "Converted".to_string(),
        },
        Status::Skipped { reason } => format!("Skipped({})", reason.as_str()),
    }
}

fn status_tags(eng: &Engine) -> Vec<String> {
    eng.set_oriented_reports()
        .iter()
        .map(|r| status_tag(&r.status))
        .collect()
}

fn bench_purchase(mode: SetOrientedMode, switch: Option<&str>) -> (f64, Vec<String>, i64) {
    let mut eng = purchase_engine();
    eng.set_oriented = mode;
    scale_detail(&mut eng);
    let rows = detail_rows(&eng);
    if let Some(k) = switch {
        set_switch(&mut eng, k);
    }
    let t = Instant::now();
    let args = [SpinValue::Str("20250715".to_string()), SpinValue::Null];
    let call = eng.call("pkg_split_trade_step3_sh.prc_trd_deal_purchase", &args);
    let el = t.elapsed().as_secs_f64();
    let mut tags = status_tags(&eng);
    if let Err(e) = call {
        tags.push(format!("call_err={e}"));
    }
    (el, tags, rows)
}

fn byfund_engine() -> Engine {
    let eng = purchase_engine();
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
    let seed = std::fs::read_to_string(
        golden_root().join("case_corpus_purchase/cases/datasets_byfund/seed.sql"),
    )
    .expect("byfund seed");
    for stmt in seed.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        eng.ex.execute(stmt, &BindMap::new()).expect("seed");
    }
    eng
}

fn bench_byfund(mode: SetOrientedMode) -> (f64, Vec<String>, i64) {
    let mut eng = byfund_engine();
    eng.set_oriented = mode;
    scale_detail(&mut eng);
    let rows = detail_rows(&eng);
    let t = Instant::now();
    let args = [SpinValue::Str("20250715".to_string()), SpinValue::Null];
    let call = eng.call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args);
    let el = t.elapsed().as_secs_f64();
    let mut tags = status_tags(&eng);
    if let Err(e) = call {
        tags.push(format!("call_err={e}"));
    }
    (el, tags, rows)
}

/// 取证基准默认跳过（100k 规模单次 ~4.5min）：`SD_BENCH=1` 显式启用。
fn bench_enabled() -> bool {
    std::env::var_os("SD_BENCH").is_some()
}

#[test]
fn bench_byfund_off_vs_batch() {
    if !bench_enabled() {
        return;
    }
    let mut best_off = f64::MAX;
    let mut best_bat = f64::MAX;
    let mut st_off = Vec::new();
    let mut st_bat = Vec::new();
    for _ in 0..1 {
        let (el, st, rows) = bench_byfund(SetOrientedMode::Off);
        eprintln!("BENCH byfund Off    rows={rows} {el:.3}s {st:?}");
        if el < best_off {
            best_off = el;
        }
        st_off = st;
        let (el, st, rows) = bench_byfund(SetOrientedMode::BatchAtomic);
        eprintln!("BENCH byfund Batch  rows={rows} {el:.3}s {st:?}");
        if el < best_bat {
            best_bat = el;
        }
        st_bat = st;
    }
    eprintln!(
        "BENCH-SUMMARY byfund off={best_off:.3}s batch={best_bat:.3}s speedup={:.1}x off_status={:?} batch_status={:?}",
        best_off / best_bat,
        st_off,
        st_bat
    );
}

#[test]
fn bench_purchase_off_vs_batch() {
    if !bench_enabled() {
        return;
    }
    for switch in [Some("0"), Some("1")] {
        let label = if switch == Some("1") { "ON " } else { "OFF" };
        // 预热+计时各两轮取最优（JIT/页缓存噪声）
        let mut best_off = f64::MAX;
        let mut best_bat = f64::MAX;
        let mut st_off = Vec::new();
        let mut st_bat = Vec::new();
        let mut nrows = 0;
        for _ in 0..1 {
            let (el, st, rows) = bench_purchase(SetOrientedMode::Off, switch);
            nrows = rows;
            eprintln!("BENCH purchase {label} Off      rows={rows} {el:.3}s {st:?}");
            if el < best_off {
                best_off = el;
            }
            st_off = st;
            let (el, st, rows) = bench_purchase(SetOrientedMode::BatchAtomic, switch);
            eprintln!("BENCH purchase {label} Batch   rows={rows} {el:.3}s {st:?}");
            if el < best_bat {
                best_bat = el;
            }
            st_bat = st;
        }
        eprintln!(
            "BENCH-SUMMARY purchase {label} rows={nrows} off={best_off:.3}s batch={best_bat:.3}s speedup={:.1}x off_status={:?} batch_status={:?}",
            best_off / best_bat,
            st_off,
            st_bat
        );
    }
}
