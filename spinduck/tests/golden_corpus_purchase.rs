//! 黄金对比（语料级·重量级）：核心过程 **pkg_split_trade_step3_sh.prc_trd_deal_purchase**
//! （回购/申购业务拆分，生产调用链第 4 步）。
//!
//! 资产按 §12.1 项目模式组织（`golden/case_corpus_purchase/`）：
//! `sql/ddl`（表 + `%TYPE` 锚定视图）`sql/routines`（核心包与依赖函数）
//! `sql/stubs`（日志桩包）`cases/datasets`（种子）`cases/baselines`（基线）。
//!
//! 被测点：%ROWTYPE 整行 FETCH、记录字段直接下发 SQL、`<<error_row>>`/GOTO、
//! 嵌套 EXCEPTION OTHERS、过程内 COMMIT、日期算术（SqlFn 下沉）、INTERVALTONUM、
//! PL 内建 substr、异常路径跳过。
//!
//! 未设置 `SPINDUCK_OG_HOST` 时打印 SKIP 并返回。

#![cfg(feature = "gauss")]

mod common;

#[test]
fn golden_corpus_purchase_matches_a_mode_baseline() {
    common::run_case("case_corpus_purchase");
}
