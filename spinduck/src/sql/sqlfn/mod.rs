//! SQL 上下文例程调用下沉（§8.3 路径②）：语句中出现**已注册 PL 例程**的调用时，
//! 编译期构造 `SELECT DISTINCT <args> …` 参数元组查询，并把每个调用点替换为
//! 针对参数缓存的相关子查询；运行期逐元组调用例程填充缓存表。
//!
//! 语义前提（文档化限制）：
//! - 例程在语句执行期间**确定性**（结果只依赖入参）——语料中为查表类函数；
//! - 同一语句中重复的相同调用（同 name+实参文本）合并为一次物化；
//! - 参数元组上界 [`MAX_TUPLES`]；
//! - 实参内的嵌套调用逐层下沉（内层先物化，外层元组查询引用内层缓存子查询）；
//! - 上下文互相引用的调用（A 的 WHERE 调 B、B 的 WHERE 调 A）显式拒绝（BIZ-0160）。
//!
//! 规模拆分（AGENTS §3，单文件 ≤500 行）：`plan`（调用点扫描与计划构造）、
//! `region`（语句上下文区间与参数元组查询）、`lower`（片段区间切分与就地下沉）。

mod lower;
mod plan;
mod region;

#[cfg(test)]
mod tests;

pub use plan::plan_sites;

use crate::ir::BindSrc;

/// 单次下沉允许的参数元组上界（超出即显式拒绝，避免无界临时表）。
pub const MAX_TUPLES: usize = 100_000;

/// 一个去重后的调用点及其下沉计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnSite {
    /// 缓存表名（运行期 `CREATE OR REPLACE TEMP TABLE`）
    pub table: String,
    /// 被调例程名（小写）
    pub name: String,
    /// 调用实参文本（原始形态，供诊断）
    pub args: Vec<String>,
    /// 参数元组查询（`SELECT DISTINCT … [FROM …] [WHERE …]`，已改写）
    pub distinct_sql: String,
    /// 元组查询所需绑定
    pub binds: Vec<(String, BindSrc)>,
}

/// 语句中的一个例程调用点（含实参区间与嵌套父链）。
struct Occurrence {
    start: usize,
    end: usize,
    name: String,
    args: Vec<String>,
    args_key: String,
    /// 各实参在语句文本中的绝对区间（按调用括号内的顶层逗号切分）
    arg_spans: Vec<(usize, usize)>,
    /// 包裹本调用的**例程**调用索引（`collect_occurrences` 返回列表内）；顶层为 `None`
    parent: Option<usize>,
    /// 归属的下沉计划索引（去重后回填）
    plan: usize,
}

struct CallPlan {
    name: String,
    args: Vec<String>,
    /// 首个调用点的实参绝对区间（元组查询按区间就地下沉嵌套调用）
    arg_spans: Vec<(usize, usize)>,
    /// 首个调用点在语句文本中的区间（定位所属查询块）
    occ_span: (usize, usize),
    /// 实参下沉后的文本（内层调用已替换为其缓存子查询）
    lowered_args: Vec<String>,
    distinct_sql: String,
    binds: Vec<(String, BindSrc)>,
    sub_orig: String,
    depends_on: Vec<usize>,
}

/// 语句是否具备"表行"上下文：`SELECT`/`UPDATE` 与 `INSERT … SELECT` 有；
/// `INSERT … VALUES` 是单行求值，参数元组即实参本身。
struct Region {
    has_context: bool,
}
