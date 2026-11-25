//! 游标循环并行（`loop-parallel` 设计）：`set_oriented` 退回后的二级降级
//! 协同通道。纯 CFG 前缀子图 $G_{\text{pure}}$ 并行预计算（Worker 零数据库
//! 连接）+ 主线程持久上下文 `InterpState` 按 `row_idx` 顺序 `FETCH` 与
//! 多出口续跑。本模块只承载配置面与可观测类型；静态门禁见 `region` /
//! `proof` / `prewarm`，运行期调度见 `runtime::parallel`。

#[cfg(test)]
mod callmat_tests;
#[cfg(test)]
mod callout_tests;
#[cfg(test)]
mod fnsite_tests;
#[cfg(test)]
mod gate_tests;
#[cfg(test)]
mod reentry_tests;
#[cfg(test)]
mod tests;

pub(crate) mod callmat;
pub(crate) mod cfg;
pub(crate) mod exprs;
pub(crate) mod prewarm;
pub(crate) mod proof;
pub(crate) mod region;

use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::IrProg;
use std::collections::BTreeSet;

/// 静态准入通过后的纯前缀执行计划（设计 §3.1/§4.1）。
/// PC 均为原始 `IrProg.code` 下标；Worker 仅按 `g_pure` 成员资格与
/// `Cfg::succs` 推进，遇后继 ∉ `g_pure` 即交权。
#[derive(Debug)]
pub(crate) struct PurePrefixPlan {
    pub region: CursorRegion,
    /// `cond_pc = fetch_pc + 1`（`JumpIf %NOTFOUND`）。
    pub cond_pc: usize,
    /// 循环体首条指令（`cond_pc + 1`），`G_pure` 唯一入口。
    pub body_start_pc: usize,
    /// `close_pc + 1`，正常出口续跑点。
    pub exit_pc: usize,
    /// $V_{pure}$：可在 Worker 沙箱求值的指令 PC 集合（DAG）。
    pub g_pure: BTreeSet<usize>,
    /// 门禁 6：每行进入 `G_pure` 到任一交权出口的最大指令步数。
    pub max_prefix_steps: u64,
    /// `V_pure` 内可静态预热的 `SqlQuery`（已通过 `shape::recognize`
    /// 且 range_key 循环不变）。
    pub queries: Vec<PrewarmQuery>,
}

/// 一条待预热查询的静态描述（设计 §5.1）。
#[derive(Debug, Clone)]
pub(crate) struct PrewarmQuery {
    pub pc: usize,
    pub sql: String,
}

/// 七道静态硬门禁总控（设计 §4.1）：`Off` 语义由调用方保证不进入；
/// 任一门禁失败返回对应 `ParallelSkipReason`，由引擎退回串行。
///
/// `catalog` 用于门禁 5 传递调用闭包与门禁 7 写集枚举；
/// `batch_commit` 为 `env.wbuf.is_some()` 口径（门禁 7 静态面）。
pub(crate) fn analyze(
    prog: &IrProg,
    catalog: &crate::catalog::RoutineCatalog,
    batch_commit: bool,
) -> Result<PurePrefixPlan, ParallelSkipReason> {
    // 门禁 1 + 6：SESE 骨架、G_pure 切分、反可重入、max_prefix_steps、
    // 最小工作量。
    let plan = region::build(prog, catalog)?;
    // 门禁 2：字段粒度跨轮状态独立性（含隐式 `__sqlerrm` 建模）。
    proof::check_loop_carried_independence(prog, &plan, catalog)?;
    // 门禁 5：传递性确定性 / 会话无关 / 事务锚定读 / Top-1 歧义。
    proof::check_determinism_and_tx_anchor(prog, &plan, catalog)?;
    // 门禁 7 静态面：读写集不交 + W_loop 可枚举性 + batch_commit 下
    // 前缀洁净度（运行态 `clean_at_prewarm` 与 `try_prewarm_only`
    // 三步协议由引擎驱动层执行）。
    prewarm::check_static(prog, &plan, catalog, batch_commit)?;
    // 门禁 3/4：fn_sites 交权边界已在 region::classify 内处理；
    // 策略 A 下 FoldedQuery 恒交权，Kernel 全定义由「纯求值不触
    // env.ex + probe Fallback 退回主线程」结构性保证。
    Ok(plan)
}

/// 并行开关（设计 §2）：默认 `Off`，100% 保持现有单线程解释路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopParallelMode {
    /// 默认关闭。
    #[default]
    Off,
    /// 安全并行：仅在通过全套静态门禁、洁净入口检查与安全预热后启用。
    Safe,
}

impl LoopParallelMode {
    /// 配置及命令行的规范模式名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Safe => "safe",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "safe" => Some(Self::Safe),
            _ => None,
        }
    }
}

/// 并行运行参数（设计 §2）。`threads = 0` 语义为自动探测
/// （`available_parallelism().min(8)`），由调度层解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopParallelConfig {
    pub mode: LoopParallelMode,
    /// Worker 线程数；0 = 自动探测。
    pub threads: usize,
    /// 每次派发给 Worker 池的游标行批大小。
    pub chunk_size: usize,
    /// 最小行数门槛：`OPEN` 后剩余行数 < `min_rows` 不启线程池。
    pub min_rows: usize,
}

// R4-P2-4：非零默认值无法 derive，显式实现。
impl Default for LoopParallelConfig {
    fn default() -> Self {
        Self {
            mode: LoopParallelMode::Off,
            threads: 0,
            chunk_size: 256,
            min_rows: 256,
        }
    }
}

/// 未获得并行证明的原因（设计 §4.2 完备枚举，审计面按名输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelSkipReason {
    NoLoopRegionFound,
    /// 含非 SESE、嵌套游标循环、不可纯化前缀、反重入证明失败、
    /// $V_{\text{pure}}$ 内 `CursorAttr` 等表达式级禁令。
    UnsupportedControlFlow,
    /// 游标为动态源（`OPEN FOR` / REF CURSOR）。
    DynamicCursorSource,
    /// 跨轮依赖（异常残留值、隐式 `__sqlerrm` 写、尾段写前缀读）。
    CycleCarriedDependence,
    /// 存在 `fn_sites` 物化或未允许的非纯语句。
    MaterializedFnSitePresent,
    /// 前缀查询无法静态预热进快照，或 $W_{\text{loop}}$ 不可完备枚举。
    UncachedQueryInPrefix,
    /// 非确定性算子、会话耦合或 Top-1 歧义并列。
    NonDeterministicOrAmbiguousTop1,
    /// 例程范围读取事务锚定值（预热复位将切换事务）。
    TransactionAnchoredRead,
    /// 前缀受 Trap 保护但无法证明无 Kernel 错。
    UnprovenPrefixTotalness,
    /// 入口存在未提交写入且循环存在 `reset_session`/回滚风险。
    DirtyPrefixWithRollbackRisk,
    /// 预热 SQL 执行失败，洁净复位后退回串行。
    PrewarmFailed,
    /// 纯前缀工作量过小或 `min_rows` 未达标。
    InsufficientParallelWork,
}

/// 一次例程调用的并行调度报告（与 `set_oriented::Report` 并行双轨，
/// 审计可还原完整漏斗，设计 §2.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopParallelReport {
    pub routine: String,
    pub status: ParallelStatus,
}

/// 一次循环并行调度的可观测结果（设计 §2；含 `prewarm_ms` 与
/// `rollback_invalidated` 供基准审计）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelStatus {
    Parallelized {
        workers: usize,
        rows_dispatched: usize,
        /// 因临界 `step_budget` 或运行时 `reset_session`/`TxRollback`
        /// 退回原生串行的行数。
        rows_fallback_serial: usize,
        rollback_invalidated: bool,
        prewarmed_tables: usize,
        prewarm_ms: u64,
        snapshot_hits: u64,
        snapshot_misses: u64,
        peak_snapshot_bytes: usize,
    },
    Skipped {
        reason: ParallelSkipReason,
    },
}
