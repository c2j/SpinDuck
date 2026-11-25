//! Issue #65 复现案例：BatchAtomic Skipped 回退路径的语义与成本基线。
//!
//! 四个断言面（对应 issue 验收标准 + 2026-11-25 实测分层）：
//! 1. **A 面**（等值+日期区间）：`lookup_cache`（PR#62 遗产）已把内核
//!    lookup 降到 1 次——本案例锁定该现状，PR **不得回退**；
//! 2. **B 面**（等值+数值区间）：`LookupExecutor` 形态识别不接管，
//!    内核 lookup 仍逐行 N 次——**#65 的目标面**，PR 后应为 1；
//!    语义与 Off 逐字一致（行/carryover/p_msg/INSERT/COMMIT 序列），
//!    trace `SqlExec` 仍逐行 6 条（不变式）；
//! 3. **LIKE 负对照**：非 total 形态，#65 明确禁缓存——必须保持 N 次；
//! 4. **Case 2 Top-1 并列**：守卫拒绝整段，回退与 Off 一致，不加
//!    Tie-breaker。
//!
//! PR 合入时修改点：B 面 `lookup_calls` 断言 6→0（原形态调用；预计算
//! 自身另证 1 次），其余断言不变。

#![cfg(feature = "native")]

#[path = "common/issue65.rs"]
mod issue65;

use issue65::*;
use spinduck_core::exec::{TraceEvent, TraceSink};
use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
use std::cell::RefCell;
use std::rc::Rc;

/// SqlExec 文本收集 sink：断言 trace 里逐行 lookup 事件数（#65 的
/// 不变式——预计算后内核调用降为 1，但 trace 仍逐行原语义）。
struct SqlExecRecorder {
    sqls: Rc<RefCell<Vec<String>>>,
}

impl SqlExecRecorder {
    fn new() -> (Self, Rc<RefCell<Vec<String>>>) {
        let sqls = Rc::new(RefCell::new(Vec::new()));
        (Self { sqls: sqls.clone() }, sqls)
    }
}

impl TraceSink for SqlExecRecorder {
    fn on_event(&mut self, ev: TraceEvent) {
        if let TraceEvent::SqlExec { sql, .. } = ev {
            self.sqls.borrow_mut().push(sql);
        }
    }
}

fn trace_lookup_events(sqls: &Rc<RefCell<Vec<String>>>) -> usize {
    sqls.borrow().iter().filter(|s| is_lookup_sql(s)).count()
}

fn assert_skipped(exactly: usize, eng: &mut spinduck_core::engine::Engine) {
    let reports = eng.set_oriented_reports();
    assert_eq!(reports.len(), exactly, "判定报告份数");
    assert!(
        matches!(reports[0].status, Status::Skipped { .. }),
        "status={:?} 应为 Skipped（守卫拒绝整段）",
        reports[0].status
    );
}

#[test]
fn should_issue65_face_a_date_range_already_cached_by_lookup_executor() {
    // Arrange：A 面夹具（等值 + 日期区间，issue Case 1 原形）。
    let mut off = setup_case1();
    let off_mark = executes_mark(&off.executes);
    off.eng.call("issue65_case1", &[]).expect("off call");
    let off_lookups = lookup_calls(&off.queries);
    let off_exec_seq = executes_since(off_mark, &off.executes);

    let mut fx = setup_case1();
    let (mut recorder, trace_sqls) = SqlExecRecorder::new();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    let fx_mark = executes_mark(&fx.executes);

    // Act
    fx.eng
        .call_with_sink("issue65_case1", &[], &mut recorder)
        .expect("batch-atomic call");

    // Assert：语义与 Off 逐字一致。
    assert_skipped(1, &mut fx.eng);
    assert_eq!(result_rows(&mut fx.eng), expected_case1_rows());
    assert_eq!(msg_out(&mut fx.eng), expected_case1_msg());
    assert_eq!(
        executes_since(fx_mark, &fx.executes),
        off_exec_seq,
        "Skipped 回退的 INSERT/COMMIT 序列必须与 Off 逐字一致"
    );
    assert_eq!(
        commit_count(&fx.executes),
        7,
        "逐行 COMMIT×6 + 调用尾提交×1"
    );
    // 成本现状：A 面 lookup_cache 已接管，Off/BatchAtomic 均为 1 次
    // 内核 lookup（PR 不得回退此现状）。
    assert_eq!(off_lookups, 1, "A 面 Off 基线：lookup_cache 接管后 1 次");
    assert_eq!(
        lookup_calls(&fx.queries),
        1,
        "A 面 BatchAtomic：lookup_cache 已接管（#65 PR 不得回退）"
    );
    // trace 不变式：逐行 SqlExec 事件 6 条。
    assert_eq!(
        trace_lookup_events(&trace_sqls),
        6,
        "A 面 trace：逐行 SqlExec×6"
    );
}

#[test]
fn should_issue65_face_b_numeric_range_keep_semantics_and_baseline_cost() {
    // Arrange：B 面夹具（等值 + 数值区间，缓存识别器未接管形态）。
    let mut off = setup_case1_num();
    let off_mark = executes_mark(&off.executes);
    off.eng.call("issue65_case1", &[]).expect("off call");
    let off_lookups = lookup_calls(&off.queries);
    let off_exec_seq = executes_since(off_mark, &off.executes);

    let mut fx = setup_case1_num();
    let (mut recorder, trace_sqls) = SqlExecRecorder::new();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    let fx_mark = executes_mark(&fx.executes);

    // Act
    fx.eng
        .call_with_sink("issue65_case1", &[], &mut recorder)
        .expect("batch-atomic call");

    // Assert：守卫拒绝，语义与 Off 逐字一致。
    assert_skipped(1, &mut fx.eng);
    assert_eq!(result_rows(&mut fx.eng), expected_case1_rows());
    assert_eq!(msg_out(&mut fx.eng), expected_case1_msg());
    assert_eq!(
        commit_count(&fx.executes),
        7,
        "逐行 COMMIT×6 + 调用尾提交×1"
    );
    assert_eq!(
        executes_since(fx_mark, &fx.executes),
        off_exec_seq,
        "Skipped 回退的 INSERT/COMMIT 序列必须与 Off 逐字一致"
    );
    // 成本（#65 落地口径）：Off 仍逐行 6 次；BatchAtomic 回退经区域级
    // outcome 缓存——原形态内核 lookup 0 次，预计算聚合扫描恰 1 次
    // （GROUP BY/COUNT 形态，`lookup_calls` 口径外）。
    assert_eq!(off_lookups, 6, "B 面 Off 基线：逐行内核 lookup×6");
    assert_eq!(
        lookup_calls(&fx.queries),
        0,
        "B 面回退：原形态内核 lookup×0（#65 预计算接管）"
    );
    let asset_kind_queries = fx
        .queries
        .borrow()
        .iter()
        .filter(|s| s.to_ascii_uppercase().contains("ASSET_KIND"))
        .count();
    assert_eq!(
        asset_kind_queries, 1,
        "B 面预计算：asset_kind 聚合扫描恰 1 次（内核 lookup N→1）"
    );
    // trace 不变式：逐行 SqlExec 事件 6 条（PR 后必须仍为 6）。
    assert_eq!(
        trace_lookup_events(&trace_sqls),
        6,
        "B 面 trace：逐行 SqlExec×6"
    );
}

#[test]
fn should_issue65_like_form_stay_uncached_per_issue_constraint() {
    // Arrange：LIKE 负对照（非 total 形态，#65 明确禁用缓存）。
    let mut off = setup_case1_like();
    off.eng.call("issue65_case1", &[]).expect("off call");
    let expected = result_rows(&mut off.eng);
    assert_eq!(
        expected,
        expected_case1_rows(),
        "Off 参考自检（LIKE 无通配符=等值）"
    );

    let mut fx = setup_case1_like();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;

    // Act
    fx.eng.call("issue65_case1", &[]).expect("batch call");

    // Assert：守卫拒绝；回退与 Off 一致；内核 lookup 保持逐行 6 次
    // （禁缓存约束——PR 不得把该形态也缓存）。
    assert_skipped(1, &mut fx.eng);
    assert_eq!(result_rows(&mut fx.eng), expected);
    assert_eq!(
        lookup_calls(&fx.queries),
        6,
        "负对照：非 total 形态必须保持原路径（#65 禁缓存约束）"
    );
}

#[test]
fn should_issue65_top1_tie_fallback_identical_to_off() {
    // Arrange：Case 2 夹具（E 存在两条最大 priority、不同 kind）。
    let mut off = setup_case2();
    off.eng.call("issue65_case2", &[]).expect("off call case2");
    let expected = tie_rows_out(&mut off.eng);
    assert_eq!(expected, expected_case2_rows(), "Off 参考自检");

    let mut fx = setup_case2();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;

    // Act
    fx.eng.call("issue65_case2", &[]).expect("batch call case2");

    // Assert：守卫继续拒绝整段（Top-1 并列不可缓存、不加 Tie-breaker），
    // 回退输出与 Off 一致。
    assert_skipped(1, &mut fx.eng);
    assert_eq!(tie_rows_out(&mut fx.eng), expected, "回退与 Off 逐字一致");
}
