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

    // Assert：等价性优先；issue #65 硬门禁——内核原形态 lookup N→1
    // （off==n、batch==0 原形态 + 恰 1 次 asset_kind 聚合）、端到端
    // 不慢于基线；≥30% 区域耗时下降由 SPINDUCK_ISSUE65_GATE=1 显式
    // 开启（CI 计时抖动隔离，issue 验收口径为 release 10 万行）。
    let reports = fx.eng.set_oriented_reports();
    assert_eq!(reports.len(), 1);
    assert!(
        matches!(reports[0].status, Status::Skipped { .. }),
        "status={:?} 应为 Skipped",
        reports[0].status
    );
    let off_stream_rows = result_rows_limit(&mut off.eng, n + 10);
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

    // 硬门禁 1：内核 lookup N→1。Off 逐行 n 次；BatchAtomic 原形态
    // 0 次，预计算聚合扫描（GROUP BY/COUNT 形态）恰 1 次。
    assert_eq!(off_lookups, n, "Off 内核 lookup 应为逐行 n 次");
    assert_eq!(
        batch_lookups, 0,
        "BatchAtomic 原形态内核 lookup 应为 0（预计算接管）"
    );
    let asset_kind_queries = fx
        .queries
        .borrow()
        .iter()
        .filter(|s| s.to_ascii_uppercase().contains("ASSET_KIND"))
        .count();
    assert_eq!(
        asset_kind_queries, 1,
        "预计算聚合扫描应恰 1 次（内核 lookup N→1）"
    );
    // 硬门禁 2：端到端不慢于基线（无条件断言）。
    assert!(
        batch_ms <= off_ms,
        "回退端到端不慢于 Off 基线：batch={batch_ms}ms off={off_ms}ms"
    );
    // 硬门禁 3：≥30% 区域耗时下降——仅 SPINDUCK_ISSUE65_GATE=1 时断言
    // （issue 验收口径：release + SPINDUCK_ISSUE65_ROWS=100000）。
    if std::env::var("SPINDUCK_ISSUE65_GATE").as_deref() == Ok("1") {
        let reduction = off_ms.saturating_sub(batch_ms);
        assert!(
            reduction * 10 >= off_ms * 3,
            "区域耗时应降低 ≥30%：off={off_ms}ms batch={batch_ms}ms"
        );
    }

    // 记录基线成本（#65 PR 对比口径）。
    println!("issue65 bench n={n}");
    println!("  off:   {off_ms} ms, kernel lookups={off_lookups}");
    println!("  batch: {batch_ms} ms, kernel lookups={batch_lookups}");
}
