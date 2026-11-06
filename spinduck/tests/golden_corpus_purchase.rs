//! 黄金对比（语料级·函数）：**FNC_GET_PURCHASE_JS_DAYS**（申购 JS 结算天数），
//! 取自 `pkg_split_trade_step3_sh.prc_trd_deal_purchase` 调用链上的语料函数。
//!
//! 资产按 §12.1 项目模式组织（`golden/case_corpus_purchase/`）：
//! `sql/ddl`（表 + `%TYPE` 锚定视图）`sql/routines`（核心包与依赖函数）
//! `sql/stubs`（日志桩包）`cases/datasets`（种子）`cases/baselines`（基线）。
//!
//! 被测点（本层，参照实例逐场景实跑比对）：SELECT INTO + EXCEPTION OTHERS 归零分支、
//! `to_date(...) + varchar2` 隐式转换、PL 内建 substr、日期算术与 INTERVALTONUM。
//!
//! 不在本层执行的是**业务过程本体**（`prc_trd_deal_purchase`）：按 2026-11-06 口径，
//! 语料级用例的黄金层只比最简函数，过程的多维断言（`out`/`rows`/`error`）在同一份
//! 资产的进程内内核逐值锁定，见 `tests/cases_native.rs` 的
//! `native_asserts_corpus_purchase` 与 `cases/purchase.case.toml` 的 `[[case.assert]]`。
//! 原因与实测记录见 docs/validation.md V21.9–V21.12（amd64 参照实例在过程 CALL 中猝死）。
//!
//! 缺 A 模式容器时**直接失败**（`common::run_case` panic 并打印起容器命令），不打印
//! SKIP——跳过不等于通过（AGENTS §4.1）。

#![cfg(feature = "gauss")]

mod common;

#[test]
fn golden_corpus_purchase_matches_a_mode_baseline() {
    common::run_case("case_corpus_purchase");
}
