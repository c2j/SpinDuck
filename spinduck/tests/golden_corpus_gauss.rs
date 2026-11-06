//! 黄金对比（语料级·函数）：`FNC_GET_STOCK_KIND` 与 `FNC_COM_GET_MIN_TAX`。
//!
//! 用例资产按 §12.1 项目模式组织（`golden/case_corpus_*/spinduck.toml`），
//! 场景/参数/基线全部来自 `cases/*.case.toml` 与 `cases/baselines/expected.json`；
//! 源码逐字取自考题基线，两侧在同一份种子数据上执行并逐场景比对。
//!
//! 缺 A 模式容器时**直接失败**（`common::run_case` panic 并打印起容器命令），不打印
//! SKIP——跳过不等于通过（AGENTS §4.1）。

#![cfg(feature = "gauss")]

mod common;

#[test]
fn golden_corpus_stock_kind_matches_a_mode_baseline() {
    common::run_case("case_corpus_stock_kind");
}

#[test]
fn golden_corpus_min_tax_matches_a_mode_baseline() {
    common::run_case("case_corpus_min_tax");
}
