//! 门禁 7 静态面：循环前 prefix 洁净度（`batch_commit` 吞并 COMMIT
//! 口径）、$R_{prefix} \cap W_{loop} = \emptyset$ 与 $W_{loop}$ 可
//! 完备枚举性。
//!
//! 运行态 `clean_at_prewarm`（`dirty_tx_observed` / `wbuf` /
//! `open_cursor.fn_sites`）与 `try_prewarm_only` 三步协议由引擎驱动
//! 层（`engine/loop_parallel.rs`）在 `OPEN` 之后执行，本模块只做
//! 不依赖运行态的静态证明。

use super::{ParallelSkipReason, PurePrefixPlan};
use crate::catalog::RoutineCatalog;
use crate::ir::set_oriented::region as so_region;
use crate::ir::{CursorSource, IrProg, IrStmt};
use std::collections::BTreeSet;

/// 门禁 7 静态检查入口。
///
/// `batch_commit`：`env.wbuf.is_some()` 口径——为 true 时 `TxCommit` /
/// `TxCommitIfWritten` 被吞并不真实提交，前缀任何写迹一律判脏。
pub(crate) fn check_static(
    prog: &IrProg,
    plan: &PurePrefixPlan,
    catalog: &RoutineCatalog,
    batch_commit: bool,
) -> Result<(), ParallelSkipReason> {
    check_prefix_clean_static(prog, &plan.region, catalog, batch_commit)?;
    check_read_write_disjoint(prog, plan, catalog)?;
    Ok(())
}

/// 静态前序检查（设计 §4.1 门禁 7 第一段）：
/// `batch_commit == true` 且入口前存在任何写迹（DML/DDL/临时表写/
/// 动态 SQL/可写 `CallRoutine`/`fn_sites` 物化）→ 一律
/// `DirtyPrefixWithRollbackRisk`——被吞并的 `COMMIT` 无法证明洁净。
/// `batch_commit == false` 时静态放行，运行态 `clean_at_prewarm`
/// 双证兜底。
fn check_prefix_clean_static(
    prog: &IrProg,
    region: &so_region::CursorRegion,
    catalog: &RoutineCatalog,
    batch_commit: bool,
) -> Result<(), ParallelSkipReason> {
    if !batch_commit {
        return Ok(());
    }
    let (writes, opaque) = so_region::prefix_write_set(prog, region.open_pc, catalog);
    let local_ddl = prog.local_ddl.iter().any(|d| d.index < region.open_pc);
    if opaque || !writes.is_empty() || local_ddl {
        return Err(ParallelSkipReason::DirtyPrefixWithRollbackRisk);
    }
    Ok(())
}

/// $R_{prefix} \cap W_{loop} = \emptyset$（设计 §4.1 门禁 7 末段）。
///
/// $R_{prefix}$ = 游标查询读集 ∪ `V_pure` 内全部可预热查询的
/// `shape::Plan.tables`（策略 B 的 dummy 表在本实现恒交权，不计入）。
/// $W_{loop}$ = 循环 CFG 内静态 DML/DDL 目标 ∪ 非内联 `CallRoutine`
/// 传递写集 ∪ `reset_session`/DDL 潜在作用面——复用
/// `set_oriented::region::write_set` 的完备枚举；枚举失败（动态 SQL /
/// 非 NoOp 调用）按相交拒判 `UncachedQueryInPrefix`。
fn check_read_write_disjoint(
    prog: &IrProg,
    plan: &PurePrefixPlan,
    catalog: &RoutineCatalog,
) -> Result<(), ParallelSkipReason> {
    let w_loop = so_region::write_set(prog, &plan.region, catalog)
        .map_err(|_| ParallelSkipReason::UncachedQueryInPrefix)?;

    let mut r_prefix = BTreeSet::new();
    // 游标源读集。
    if let Ok(reads) = so_region::read_set(prog, &plan.region) {
        r_prefix.extend(reads);
    }
    // 预热查询读集（shape::recognize 已在 region::classify 验证过，
    // 此处重取 tables 构成 R_prefix）。
    for q in &plan.queries {
        if let Some(p) = crate::lookup_cache::shape::recognize(&q.sql) {
            r_prefix.extend(p.tables.iter().cloned());
        }
    }
    if r_prefix.intersection(&w_loop).next().is_some() {
        return Err(ParallelSkipReason::UncachedQueryInPrefix);
    }
    Ok(())
}

/// 供引擎驱动层复用：游标源为 `Static`（`region::build` 已静态保证；
/// fn_site 物化在 `OPEN` 同点完成、内容对 Worker 只读，V8-E5 准入
/// 口径见 `region::reject_cursor_fn_sites`）。
pub(crate) fn cursor_is_prewarmable(prog: &IrProg, plan: &PurePrefixPlan) -> bool {
    let Some(IrStmt::OpenCursor { source, .. }) = prog.code.get(plan.region.open_pc) else {
        return false;
    };
    matches!(source, CursorSource::Static { .. })
}
