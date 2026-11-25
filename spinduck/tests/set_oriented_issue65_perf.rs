//! Issue #65 性能基线：BatchAtomic Skipped 回退路径的成本度量。
//!
//! 按 issue 验收口径播种（n 行游标，10% 恰一行命中，90% 落空；
//! duplicate 正确性由 set_oriented_issue65_repro 覆盖，不混入性能口径）。
//! 本文件当前只锁定"等价性 + 记录分段成本"；#65 PR 落地后在此追加
//! 硬门槛断言（原 lookup 内核调用 N→1、端到端不慢于基线、10 万行
//! 回退区域耗时降低 ≥30%）。
//!
//! 规模：默认 10_000 行；`SPINDUCK_ISSUE65_ROWS=100000` 跑 issue
//! 目标口径（本机验证用，不建议进 CI）。

#![cfg(feature = "native")]

#[path = "common/issue65.rs"]
mod issue65;

use issue65::*;
use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
use std::time::Instant;

fn bench_rows() -> usize {
    std::env::var("SPINDUCK_ISSUE65_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

#[test]
fn should_issue65_fallback_rows_equal_off_and_record_baseline_cost() {
    let n = bench_rows();

    // Arrange + Act：Off 参考计时。
    let mut off = setup_bench(n);
    let t0 = Instant::now();
    off.eng.call("issue65_bench", &[]).expect("off bench call");
    let off_ms = t0.elapsed().as_millis();
    let off_lookups = lookup_calls(&off.queries);

    // BatchAtomic（守卫拒绝 → 回退）计时。
    let mut fx = setup_bench(n);
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    let t1 = Instant::now();
    fx.eng.call("issue65_bench", &[]).expect("batch bench call");
    let batch_ms = t1.elapsed().as_millis();
    let batch_lookups = lookup_calls(&fx.queries);

    // Assert：等价性优先，计时仅记录（门槛断言随 #65 PR 追加）。
    let reports = fx.eng.set_oriented_reports();
    assert_eq!(reports.len(), 1);
    assert!(
        matches!(reports[0].status, Status::Skipped { .. }),
        "status={:?} 应为 Skipped",
        reports[0].status
    );
    let mut off_stream_rows = result_rows_limit(&mut off.eng, n + 10);
    let batch_stream_rows = result_rows_limit(&mut fx.eng, n + 10);
    assert_eq!(off_stream_rows.len(), n, "Off 行数");
    assert_eq!(
        batch_stream_rows, off_stream_rows,
        "回退输出与 Off 逐行一致"
    );
    // 首尾抽查：第 0 行命中（i%10==0），第 1 行落空（NULL）。
    assert_eq!(
        off_stream_rows[0][2],
        spinduck_core::value::SpinValue::Str("K_0".into())
    );
    assert_eq!(
        batch_stream_rows[1][2],
        spinduck_core::value::SpinValue::Null
    );

    // 记录基线成本（#65 PR 对比口径）。
    println!("issue65 bench n={n}");
    println!("  off:   {off_ms} ms, kernel lookups={off_lookups}");
    println!("  batch: {batch_ms} ms, kernel lookups={batch_lookups}");
}
