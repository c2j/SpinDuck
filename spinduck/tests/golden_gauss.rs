//! 黄金对比（toy 聚合管线）：`prc_agg_fund` 物化 → CALL → 回写 → 读回全链路。
//!
//! 资产按 §12.1 项目模式组织（`golden/case_split_agg/`）；参照侧等价例程见
//! `cases/baselines/reference_routine.sql`（PG 风格函数形态 + `reference_call_sql`）。
//!
//! 未设置 `SPINDUCK_OG_HOST` 时打印 SKIP 并返回——跳过不等于通过。

#![cfg(feature = "gauss")]

mod common;

#[test]
fn golden_case_split_agg_matches_opengauss_baseline() {
    common::run_case("case_split_agg");
}
