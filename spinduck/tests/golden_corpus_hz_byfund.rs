//! 黄金对比（语料级·热点过程）：**pkg_split_trade_step3_sh.prc_trd_hz_byfund**
//! （按基金汇总成交，考题调用链第 3 步，生产性能热点）。
//!
//! 资产按 §12.1 项目模式组织（`golden/case_corpus_hz_byfund/`）：依赖闭包
//! （表/视图/函数/桩包/核心包）逐个文件**逐字复制自考题基线**，仅目录重组；
//! 种子为自实现。
//!
//! 被测点：SQL 上下文例程调用下沉（UPDATE SET / SELECT WHERE / INSERT VALUES
//! 中的 PL 函数）、静态游标 + %ROWTYPE、聚合分组、过程内 COMMIT、多个
//! EXCEPTION 分支与 p_o_succeed 语义、视图查询。
//!
//! 未设置 `SPINDUCK_OG_HOST` 时打印 SKIP 并返回。

#![cfg(feature = "gauss")]

mod common;

#[test]
fn golden_corpus_hz_byfund_matches_a_mode_baseline() {
    common::run_case("case_corpus_hz_byfund");
}
