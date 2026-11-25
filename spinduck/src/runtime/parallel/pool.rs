//! loop-parallel 派发池：chunk 内 `std::thread::scope` 有界并行。
//!
//! 每个 Worker 只持有 `WorkerJob` 的只读共享引用（`IrProg`/`PurePrefixPlan`/
//! `Frame` 快照/`Arc<Vec<Row>>`/`FrozenLookupSnapshot`），零 `SqlExecutor`、
//! 零 `TraceSink`、零游标状态。结果经 channel 回收后**按 `row_idx`
//! 排序**返回——主线程的合并顺序由驱动层保证，这里只保证集合完整。

use super::calls::CallSnapshot;
use super::worker::{run_row, WorkerJob, WorkerRowResult};
use crate::catalog::RoutineCatalog;
use crate::exec::Row;
use crate::ir::loop_parallel::PurePrefixPlan;
use crate::ir::{AssignTarget, IrProg};
use crate::lookup_cache::FrozenLookupSnapshot;
use crate::runtime::frame::Frame;
use crate::runtime::interp::TrapFrame;
use crate::types::SpinType;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

/// 一批行的派发参数（全部为只读共享引用，`Sync` 由编译期断言保证）。
pub(crate) struct ChunkJob<'a> {
    pub prog: &'a IrProg,
    pub plan: &'a PurePrefixPlan,
    pub base_frame: &'a Frame,
    pub rows: &'a Arc<Vec<Row>>,
    pub cursor_schema: &'a [String],
    pub record_target: &'a AssignTarget,
    pub snapshot: &'a FrozenLookupSnapshot,
    /// E2：例程目录（`FuncCall` 物化分派）+ 调用物化快照。
    pub catalog: &'a RoutineCatalog,
    pub calls: &'a CallSnapshot,
    pub var_types: &'a BTreeMap<String, SpinType>,
    pub loose: bool,
    pub traps_seed: &'a [TrapFrame],
    /// 本 chunk 覆盖的游标物化行绝对下标（升序）。
    pub indices: &'a [usize],
    pub threads: usize,
}

/// 并行执行一个 chunk 的全部行，返回按 `row_idx` 升序的结果向量。
/// `threads <= 1` 时退化为顺序执行（与并行路径同一 `run_row` 代码）。
pub(crate) fn dispatch_chunk(job: &ChunkJob<'_>) -> Vec<WorkerRowResult> {
    let threads = job.threads.max(1).min(job.indices.len().max(1));
    if threads <= 1 {
        return job
            .indices
            .iter()
            .map(|&i| run_row(&worker_job(job), i))
            .collect();
    }
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..threads {
            let next = &next;
            let tx = tx.clone();
            s.spawn(move || loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                let Some(&row_idx) = job.indices.get(k) else {
                    break;
                };
                let res = run_row(&worker_job(job), row_idx);
                if tx.send(res).is_err() {
                    break;
                }
            });
        }
    });
    drop(tx);
    let mut out: Vec<WorkerRowResult> = rx.iter().collect();
    out.sort_by_key(|r| r.row_idx);
    out
}

fn worker_job<'a>(job: &'a ChunkJob<'a>) -> WorkerJob<'a> {
    WorkerJob {
        prog: job.prog,
        plan: job.plan,
        base_frame: job.base_frame,
        rows: job.rows,
        cursor_schema: job.cursor_schema,
        record_target: job.record_target,
        snapshot: job.snapshot,
        catalog: job.catalog,
        calls: job.calls,
        var_types: job.var_types,
        loose: job.loose,
        traps_seed: job.traps_seed,
    }
}
