//! 集合化回退路径的逐键 lookup 结果缓存（issue #65）。
//!
//! 守卫证据已证的静态 `SELECT INTO`（游标记录字段等值键 + 静态残余
//! 过滤）在 Skipped 回退中逐行下发原 SQL：N 个游标行 = N 次内核
//! lookup。本模块在回退 `RunSpan` 外包一层 [`SqlExecutor`]：首个探测
//! 行触发一次 `GROUP BY` 聚合预计算（`COUNT(*)` + `CASE WHEN
//! COUNT(*)=1 THEN MAX(投影列)`，与守卫聚合扫描同一判定口径），
//! 此后按"站点 + 全部行变绑定值"命中缓存，合成与原路径一致的
//! `NO_DATA_FOUND`/`TOO_MANY_ROWS`/单行结果；trace `SqlExec` 仍由
//! 解释器逐行照发，绑定文本逐字不变。
//!
//! 证据与禁用面（与守卫 `key_checks` 同口径）：键列与游标字段类型
//! 严格一致才可证；LIKE/函数/子查询/动态 SQL/Top-1 排序截断、
//! `lookup_cache` 已识别的形态一律不接管。缓存只服务本次回退调用，
//! 不跨调用/跨区域复用；写语句按表名失效、回滚全清；绑定含 NULL
//! 或非常量键的探测 bypass 原路径；distinct key 超上限禁用缓存。

mod cache;
mod site;

use crate::exec::SqlExecutor;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::IrProg;

pub(crate) use cache::FallbackExecutor;

/// 单个可缓存 `SELECT INTO` 站点的预计算规格（`site` 分析产物，`cache` 消费）。
#[derive(Debug)]
pub(super) struct SiteSpec {
    /// IR `SqlQuery` 原文：执行器按文本精确匹配站点。
    pub sql: String,
    /// 目标表名（小写）：写失效匹配键。
    pub table: String,
    /// 行变键占位符（与 GROUP BY 列同序）。
    pub key_phs: Vec<String>,
    /// 聚合预计算 SQL：`GROUP BY` 键列 + `COUNT(*)` + `CASE WHEN
    /// COUNT(*)=1 THEN MAX(投影列)`，残余过滤原样保留。
    pub build_sql: String,
    /// build_sql 引用的不变量占位符（探测时从绑定表取子集下发）。
    pub inv_phs: Vec<String>,
    /// 结果列名（SELECT 输出名），合成 `RowStream` 的 schema。
    pub schema: Vec<String>,
}

/// 回退区域的候选缓存执行器：扫描区域内静态 `SELECT INTO`，对守卫
/// 文法可证的站点生成聚合 outcome 索引；无可证站点返回 `None`，
/// 调用方走原解释器路径（行为逐字不变）。
pub(crate) fn prepare<'a>(
    prog: &IrProg,
    region: &CursorRegion,
    ex: &'a dyn SqlExecutor,
) -> Option<FallbackExecutor<'a>> {
    let sites = site::collect(prog, region, ex);
    cache::wrap(ex, sites)
}
