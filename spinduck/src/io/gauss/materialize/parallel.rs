//! 并行物化调度（`materialize` 的子模块）：完成序回调 + 输入序返回。
//!
//! 拆出本文件是为守住 AGENTS §3 的单文件规模上限；调度只有这一份实现，
//! `materialize_parallel` 只是无回调的包装。

use super::{GaussConn, MaterializeOutcome};
use crate::error::EngineError;
use std::path::Path;

/// 线程聚合的逐表结果：(输入序, 表名, 物化结果/错误)。
type TableResults = Vec<(usize, String, Result<MaterializeOutcome, EngineError>)>;

/// 并行物化的批次输入（表清单 + 并发度 + 临时目录）。
///
/// 独立成结构体是为守住 §3 的「函数参数 ≤5」：调度、连接与回调各自是参数，
/// 批次本身不该再摊成三个位置参数。
pub struct MaterializeBatch<'a> {
    /// 并发度；内部按 `max(1).min(tables.len())` 收敛。
    pub jobs: usize,
    /// 待物化表名；返回值保持同一顺序。
    pub tables: Vec<String>,
    /// 临时 CSV 落盘目录。
    pub workdir: &'a Path,
}

/// 并行物化的**唯一**调度实现：完成序回调 + 输入序返回。
///
/// `on_done(k, total, item)` 在调用线程按完成序被调用（`k` 从 1 起），供 CLI
/// 逐表渲染进度；返回值仍按输入表序，失败以 `Err` 体现。回调不返回 I/O 结果：
/// 调用方在回调里记录自身错误，待本函数返回后处理，避免把「渲染失败」与
/// 「物化失败」混成同一状态。
pub fn materialize_parallel_with_progress<T, F, P>(
    batch: MaterializeBatch<'_>,
    open_source: F,
    duck_executors: Vec<T>,
    mut on_done: P,
) -> Vec<(String, Result<MaterializeOutcome, EngineError>)>
where
    T: crate::exec::SqlExecutor + Send + 'static,
    F: Fn() -> Result<GaussConn, EngineError> + Sync,
    P: FnMut(usize, usize, &(String, Result<MaterializeOutcome, EngineError>)),
{
    let MaterializeBatch {
        jobs,
        tables,
        workdir,
    } = batch;
    // 前置条件硬断言（#21-4）：release 下也生效。长度不一致时线程池按索引取
    // 执行器会越界 panic，且 panic 经 thread::scope 抛出主线程，与「单表失败不
    // 中断其余表」相反——公共 API 的契约不能只靠调用方自觉。
    assert_eq!(
        tables.len(),
        duck_executors.len(),
        "materialize_parallel: 表与执行器须一一对应"
    );
    let total = tables.len();
    let jobs = jobs.max(1).min(total);
    // 游标每次调用局部新建：同进程第二次调用必须重新从 0 起。进程级 static
    // 会停在上次终值，让第二次调用跳过全部表并静默成功（F1 回归点）。
    let next = std::sync::atomic::AtomicUsize::new(0);
    // 执行器按索引所有权移交（take）：线程只触碰自己的 T，约束收敛到 Send 即可
    let pool: std::sync::Mutex<Vec<Option<T>>> =
        std::sync::Mutex::new(duck_executors.into_iter().map(Some).collect());
    let (tx, rx) =
        std::sync::mpsc::channel::<(usize, String, Result<MaterializeOutcome, EngineError>)>();
    let mut collected: TableResults = Vec::with_capacity(total);
    std::thread::scope(|s| {
        // 以共享引用捕获连接工厂：`&F` 在 `F: Sync` 下可跨线程，公共 API 的
        // 约束不因调度搬进线程作用域而被迫扩成 `F: Send`。
        let open_source = &open_source;
        for _ in 0..jobs {
            let tx = tx.clone();
            let next = &next;
            let pool = &pool;
            let tables = &tables;
            s.spawn(move || loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= total {
                    break;
                }
                let Some(duck) = pool.lock().unwrap_or_else(|e| e.into_inner())[i].take() else {
                    break;
                };
                let res = open_source()
                    .and_then(|mut og| og.materialize(&duck, &tables[i], None, workdir));
                // 接收端在本作用域内始终存活；send 失败只可能发生在接收端已丢弃后，
                // 该形态由 scope 的 panic 传播暴露，调用方另有计数兜底，不静默成功。
                let _ = tx.send((i, tables[i].clone(), res));
            });
        }
        drop(tx);
        for (k, (i, table, res)) in rx.iter().enumerate() {
            let item = (table, res);
            on_done(k + 1, total, &item);
            collected.push((i, item.0, item.1));
        }
    });
    collected.sort_by_key(|(i, _, _)| *i);
    collected.into_iter().map(|(_, t, r)| (t, r)).collect()
}

/// 无进度回调的并行物化（既有调用面）：调度仍走
/// [`materialize_parallel_with_progress`]，不保留第二套线程池。
pub fn materialize_parallel<T, F>(
    jobs: usize,
    tables: Vec<String>,
    workdir: &Path,
    open_source: F,
    duck_executors: Vec<T>,
) -> Vec<(String, Result<MaterializeOutcome, EngineError>)>
where
    T: crate::exec::SqlExecutor + Send + 'static,
    F: Fn() -> Result<GaussConn, EngineError> + Sync,
{
    materialize_parallel_with_progress(
        MaterializeBatch {
            jobs,
            tables,
            workdir,
        },
        open_source,
        duck_executors,
        |_, _, _| {},
    )
}
