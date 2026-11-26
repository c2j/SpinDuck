//! 集合化回退路径的逐键 lookup 结果缓存（issue #65）。
//!
//! 守卫证据已证的静态 `SELECT INTO`（游标记录字段等值键 + 全定义
//! 残余谓词）在 Skipped 回退中逐行下发原 SQL：N 个游标行 = N 次内核
//! lookup。本模块在回退 `RunSpan` 外包一层 [`SqlExecutor`]：首个探测
//! 行触发一次 `GROUP BY` 聚合预计算（`COUNT(*)` + `CASE WHEN
//! COUNT(*)=1 THEN MAX(投影列)`，与守卫聚合扫描同一判定口径），
//! 此后按"站点 + 全部绑定值"命中缓存，合成与原路径一致的
//! `NO_DATA_FOUND`/`TOO_MANY_ROWS`/单行结果；trace `SqlExec` 仍由
//! 解释器逐行照发，绑定文本逐字不变。
//!
//! 证据与禁用面（与守卫 `key_checks` 同口径 + 审核修复）：键列与
//! 游标字段类型严格一致才可证，浮点键列（有损舍入）整站禁用；
//! 残余合取项必须过 `residual` 全定义谓词证明（算术/函数/子查询/
//! 类型不安全比较一律禁用）；LIKE/动态 SQL/Top-1、`lookup_cache`
//! 已识别形态不接管。缓存只服务本次回退调用；写语句按表名使站点
//! **规格作废重证**（schema 可能已变，旧 proof 不复用）、回滚全清；
//! 绑定含 NULL、探测不变量值与构建值不一致（嵌套例程）或 distinct
//! key 超上限时 bypass/禁用走原路径。

mod cache;
mod residual;
mod site;
mod verbatim;

use crate::exec::SqlExecutor;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::IrProg;
use std::cell::RefCell;

pub(crate) use cache::FallbackExecutor;

thread_local! {
    /// 当前执行例程名栈（R3 站点身份）：`run_fallback` 压栈本例程，
    /// `CallRoutine` 压栈被调例程。嵌套例程经 `run_with_catalog` 复用
    /// 调用方 `env.ex`，其查询会落到外层 `FallbackExecutor`——栈顶
    /// 例程名与执行器 owner 不一致即 bypass，站点身份成为匹配一等键。
    static ROUTINE_SCOPE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// 例程作用域 RAII 守卫：Drop 时弹栈（异常路径同样恢复）。
pub(crate) struct ScopeGuard;

/// 压入例程名并返回守卫；与 `current_scope` 配对使用。
pub(crate) fn push_scope(name: &str) -> ScopeGuard {
    ROUTINE_SCOPE.with(|s| s.borrow_mut().push(name.to_string()));
    ScopeGuard
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        ROUTINE_SCOPE.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// 当前例程名（栈顶）；无例程上下文返回 `None`。
pub(crate) fn current_scope() -> Option<String> {
    ROUTINE_SCOPE.with(|s| s.borrow().last().cloned())
}

/// 单个可缓存 `SELECT INTO` 站点的预计算规格（`site` 分析产物，`cache` 消费）。
#[derive(Debug, Clone)]
pub(super) struct SiteSpec {
    /// IR `SqlQuery` 原文：执行器按文本精确匹配站点。
    pub sql: String,
    /// 站点绑定表（重证输入）：`(占位符, BindSrc)` 原表。
    pub binds: Vec<(String, crate::ir::BindSrc)>,
    /// 失效监视表集合（小写）：维度表 + 游标源表——写/DDL 命中
    /// 任一即作废本规格并重证（B3：不得复用旧 schema/type proof）。
    pub watch: Vec<String>,
    /// 行变键占位符（与 GROUP BY 列同序）。
    pub key_phs: Vec<String>,
    /// 聚合预计算 SQL：`GROUP BY` 键列 + `COUNT(*)` + `CASE WHEN
    /// COUNT(*)=1 THEN MAX(投影列)`，残余过滤原样保留。
    pub build_sql: String,
    /// build_sql 引用的不变量占位符（探测时从绑定表取子集下发）。
    pub inv_phs: Vec<String>,
    /// 运行期类型复核项：`(占位符, 列 kind)`——构建前按绑定值
    /// `SpinValue` 变体复核（键绑定 + 残余 `$v_` 绑定），不相容禁用。
    pub typed_binds: Vec<(String, String)>,
    /// 结果列名（SELECT 输出名），合成 `RowStream` 的 schema。
    pub schema: Vec<String>,
    /// 站点身份（例程名 + 出现 PC 列表），诊断标记。
    pub scope: String,
}

/// 回退区域的候选缓存执行器：扫描区域内静态 `SELECT INTO`，对守卫
/// 文法可证的站点生成聚合 outcome 索引；无可证站点返回 `None`，
/// 调用方走原解释器路径（行为逐字不变）。
/// `prog`/`region` 随执行器保留：写失效后按当前 schema 重证站点。
pub(crate) fn prepare<'a>(
    prog: &'a IrProg,
    region: &'a CursorRegion,
    ex: &'a dyn SqlExecutor,
    routine: &str,
) -> Option<FallbackExecutor<'a>> {
    let sites = site::collect(prog, region, ex, routine);
    cache::wrap(ex, prog, region, sites, routine)
}
