//! SQL 上下文例程调用下沉（§8.3 路径②）：语句中出现**已注册 PL 例程**的调用时，
//! 编译期构造 `SELECT DISTINCT <args> …` 参数元组查询，并把每个调用点替换为
//! 针对参数缓存的相关子查询；运行期逐元组调用例程填充缓存表。
//!
//! 语义前提（文档化限制）：
//! - 例程在语句执行期间**确定性**（结果只依赖入参）——语料中为查表类函数；
//! - 同一语句中重复的相同调用（同 name+实参文本）合并为一次物化；
//! - 参数元组上界 [`MAX_TUPLES`]；
//! - 上下文互相引用的调用（A 的 WHERE 调 B、B 的 WHERE 调 A）显式拒绝。
//!
//! 规模拆分（AGENTS §3，单文件 ≤500 行）：`plan`（调用点扫描与计划构造）、
//! `region`（语句上下文区间与参数元组查询）。

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

struct Occurrence {
    start: usize,
    end: usize,
    name: String,
    args: Vec<String>,
    args_key: String,
}

struct CallPlan {
    name: String,
    args: Vec<String>,
    distinct_sql: String,
    binds: Vec<(String, BindSrc)>,
    sub_orig: String,
    occurrences: Vec<(usize, usize)>,
    depends_on: Vec<usize>,
}

/// 语句的上下文区间：FROM/WHERE（SELECT）、表/WHERE（UPDATE）；
/// INSERT 无上下文（参数元组即单行）。
struct Region {
    has_context: bool,
    ctx_start: usize,
    ctx_end: usize,
}

impl Region {
    fn contains(&self, pos: usize) -> bool {
        self.has_context && pos >= self.ctx_start && pos < self.ctx_end
    }
}
