//! E2 确定性调用物化快照与 `NeedKeys` 待执行请求（V8 详设
//! `loop-parallel-v8/e2`）。
//!
//! Worker 求值 `FuncCall`/`SqlFn` 时不执行例程/SQL，而是按规范键
//! 探测 `CallSnapshot`：
//! - 命中 `Scalar`/`Routine` → 直接取物化结果（Routine 含 OUT/INOUT
//!   写回向量，Worker 按形参序还原到调用方槽位）；
//! - 命中 `Failed` → 该行在指令边界交权，主线程原生复现错误语义；
//! - 未命中 → `WorkerOutcome::NeedKeys` 携带 `PendingCall` 交权，
//!   主线程按行序补执行并回填快照后重派该行（任意键依赖按需展开，
//!   无需静态 key_dag）。
//!
//! 快照只读共享给全部 Worker（`Mutex` 仅允许主线程在 chunk 间隙
//! 写入——`dispatch_chunk` join 后消费结果，天然无并发写）。

use crate::exec::BindMap;
use crate::value::SpinValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 一次物化调用的主线程产物。
#[derive(Debug, Clone)]
pub(crate) enum CallOutcome {
    /// `SqlFn` / 目录未命中内建 `FuncCall`：标量结果（空结果集 → Null，
    /// 与 `eval_sqlfn`/`eval_builtin` 同口径）。
    Scalar(SpinValue),
    /// 目录 `Function` 例程：返回值 + OUT/INOUT 写回（按形参序，
    /// 仅 `mode != In` 位置有值；其余位 `None`）。
    Routine {
        ret: SpinValue,
        outs: Vec<Option<SpinValue>>,
    },
    /// 主线程物化执行失败：Worker 见此标记一律交权，由原生路径
    /// 精确复现 trap/`__sqlerrm`/错误码。
    Failed,
}

/// 调用物化快照：规范键 → 结果。Worker 只读探测。
pub(crate) struct CallSnapshot {
    map: Mutex<HashMap<String, CallOutcome>>,
    /// E8 可观测：探测命中/未命中计数（报告 `snapshot_hits/misses`）。
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CallSnapshot {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// 探测物化结果（克隆返回，Worker 持有独立副本）。
    pub fn probe(&self, key: &str) -> Option<CallOutcome> {
        let out = self.map.lock().ok().and_then(|m| m.get(key).cloned());
        if out.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        out
    }

    /// 主线程物化完成后回填（chunk 消费间隙调用，无并发写）。
    pub fn insert(&self, key: String, outcome: CallOutcome) {
        if let Ok(mut m) = self.map.lock() {
            m.insert(key, outcome);
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// 快照体积估算（LP 报告 `peak_snapshot_bytes`）：键长 + 每条
    /// 结果的 `SpinValue` 粒度。堆布局估算而非精确字节，仅供
    /// 容量观测，不做正确性判据。
    pub fn approx_bytes(&self) -> usize {
        const V: usize = std::mem::size_of::<SpinValue>();
        self.map.lock().map_or(0, |m| {
            m.iter()
                .map(|(key, outcome)| {
                    let (count, extra) = match outcome {
                        CallOutcome::Scalar(v) => (1, v_heap_extra(v)),
                        CallOutcome::Routine { ret, outs } => (
                            1 + outs.len(),
                            v_heap_extra(ret)
                                + outs.iter().flatten().map(v_heap_extra).sum::<usize>(),
                        ),
                        CallOutcome::Failed => (0, 0),
                    };
                    key.len() + 32 + count * V + extra
                })
                .sum()
        })
    }
}

/// `SpinValue` 堆内变长部分的粗估（字符串/字节内容长度；
/// 数值型按 0 计——`approx_bytes` 本就是下界估算）。
fn v_heap_extra(v: &SpinValue) -> usize {
    match v {
        SpinValue::Str(s) => s.len(),
        SpinValue::Blob(b) => b.len(),
        SpinValue::Record(fields) => fields
            .iter()
            .map(|(name, inner)| name.len() + v_heap_extra(inner))
            .sum(),
        _ => 0,
    }
}

/// Worker → 主线程的待物化请求（`NeedKeys` 载荷）。
#[derive(Debug, Clone)]
pub(crate) enum PendingCall {
    /// 目录 `Function` 例程：实参已在 Worker 侧求值为 `SpinValue`
    /// （缺省形参由主线程物化时按 `param.default`/Null 补齐，
    /// 与 `eval_funccall` 同口径）。
    Routine { name: String, args: Vec<SpinValue> },
    /// 标量 SQL（`SqlFn` 模板或内建 `FuncCall` prepared）：
    /// SQL 文本 + 已解析绑定。
    Scalar { sql: String, binds: BindMap },
}

impl PendingCall {
    /// 快照规范键：例程按 `(小写名, 实参值序列)`；标量按
    /// `(sql, 绑定映射)`。`Debug` 输出对 `BTreeMap` 保序、对
    /// `SpinValue` 逐值区分——同输入必同键。
    pub fn key(&self) -> String {
        match self {
            Self::Routine { name, args } => routine_key(name, args),
            Self::Scalar { sql, binds } => scalar_key(sql, binds),
        }
    }
}

/// 例程物化键（Worker 探测与主线程回填必须同源）。
pub(crate) fn routine_key(name: &str, args: &[SpinValue]) -> String {
    format!("R|{}|{:?}", name.to_ascii_lowercase(), args)
}

/// 标量物化键（`SqlFn` 模板 / 内建 `FuncCall` prepared 同口径）。
pub(crate) fn scalar_key(sql: &str, binds: &BindMap) -> String {
    format!("S|{}|{:?}", sql, binds)
}
