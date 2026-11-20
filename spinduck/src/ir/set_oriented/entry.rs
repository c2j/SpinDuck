//! 区域入口运行状态机（设计 §4.5-2）：前缀执行完毕后的不抛错守卫与续跑产物。

use super::schema::{self, Table};
use super::shape::Name;
use super::{BindSrc, OptimizeCtx, SkipReason};
use crate::error::EngineError;
use crate::exec::SqlExecutor;

/// 批写前的例程物化前导（游标 WHERE fn_site）：与解释器 OpenCursor
/// 时 `materialize_fn_sites` 同口径——distinct 元组查询 → 逐元组调用
/// 例程 → 临时缓存表 `(sd_v, sd_a0…)`，批写 SQL 内相关子查询命中。
#[derive(Clone)]
pub(crate) struct FnPrelude {
    /// 缓存临时表名（与批写 SQL 中 `sd_fnN` 引用一致）。
    pub table: String,
    /// 例程名（物化期 `call_routine_value` 解析）。
    pub name: String,
    /// 去重元组查询（游标 site 为编译期原文，含源表与过滤）。
    pub distinct_sql: String,
    /// 元组查询绑定（IN 形参等，运行期按名从帧解析）。
    pub binds: Vec<(String, BindSrc)>,
    /// 例程形参个数（缓存表 sd_a0..sd_a{arity-1} 列数）。
    pub arity: usize,
    /// INSERT 位前导：distinct 查询内嵌 `sd_cur`（依赖游标 site 缓存表
    /// 先行物化），其例程求值的会话回滚可能撤销已建游标表——物化器对
    /// 本类前导在依赖方求值后重写基础表。
    pub needs_sd_cur: bool,
}

/// 结构准入与 SQL 生成的产物；入口守卫与后缀组装消费它。
#[derive(Clone)]
pub(crate) struct ConversionPlan {
    pub sql: String,
    /// 批写 SQL 的按名绑定（游标查询绑定 + INSERT 绑定合并）。
    pub(crate) binds: Vec<(String, BindSrc)>,
    /// 批写前须物化的例程缓存表（游标 WHERE fn_site，按声明序执行）。
    pub(crate) fn_preludes: Vec<FnPrelude>,
    /// 入口守卫 SQL 引用缓存表（verbatim cur_scan / 写守卫子查询）时为
    /// true：物化前导先于 entry_ok 执行。回退路径下解释器会重新物化
    /// （例程纯度已证，仅多一次去重调用——与 Off 逐行调用次数本就不同）。
    pub(crate) preludes_first: bool,
    pub tail_commit: bool,
    pub open_pc: usize,
    pub close_pc: usize,
    pub(super) source: Name,
    pub(super) target: Name,
    pub(super) source_table: Table,
    pub(super) target_table: Table,
    /// 兜底查找涉案表（入口漂移复核同口径）。
    pub(super) lookup_tables: Vec<(Name, Table)>,
    /// 有序 Top-1 入口并列一致性守卫 SQL（期望 COUNT(*) = 0）及其须随行的
    /// 形参绑定（LATERAL 静态过滤含 BETWEEN 等；入口从运行帧解析后求值）。
    pub(super) top1_guards: Vec<(String, Vec<(String, BindSrc)>)>,
    /// IF 路由入口守卫 SQL（期望 TRUE = THEN 分支不可达，§4.3）。
    pub(super) route_guards: Vec<String>,
    /// 内联表达式入口无抛错守卫 SQL（期望 COUNT = 0，§4.5-2）：以 TRY_
    /// 全定义预筛全部源行，任一行结果为 NULL（坏输入或 NULL 输入）即退回。
    pub(super) expr_guards: Vec<BoundGuard>,
}

pub(super) type BoundGuard = (String, Vec<(String, BindSrc)>);

pub(super) fn bind_guards(
    guards: Vec<String>,
    binds: &[(String, BindSrc)],
) -> Result<Vec<BoundGuard>, SkipReason> {
    guards
        .into_iter()
        .map(|sql| {
            let tokens =
                super::shape::Tokens::new(&sql).ok_or(SkipReason::UnprovenQueryTotalness)?;
            let used = binds
                .iter()
                .filter(|(ph, _)| tokens.contains(ph))
                .cloned()
                .collect();
            Ok((sql, used))
        })
        .collect()
}

/// 入口守卫（前缀 IR 执行完毕、踏入 open_pc 之前）：全部查询经证明不抛错
/// （存在性先行、只读目录面）。任一失败 → 调用方以 open_pc 续跑原始区域，
/// 绝不重放前缀；事务此刻仍完全干净。
pub(crate) fn entry_ok(
    ex: &dyn SqlExecutor,
    plan: &ConversionPlan,
    ctx: &OptimizeCtx<'_>,
    frame: &crate::runtime::Frame,
) -> Result<Result<(), SkipReason>, EngineError> {
    if !schema::sys_dummy_ok(ex)? {
        return Ok(Err(SkipReason::UnprovenSysDummyInvariant));
    }
    if drifted(ex, &plan.source, &plan.source_table)?
        || drifted(ex, &plan.target, &plan.target_table)?
    {
        return Ok(Err(SkipReason::UnprovenTargetConstraints));
    }
    for (name, table) in &plan.lookup_tables {
        if drifted(ex, name, table)? {
            return Ok(Err(SkipReason::UnprovenTargetConstraints));
        }
    }
    if !snapshot_matches(ex, plan, ctx)? {
        return Ok(Err(SkipReason::UnprovenTargetConstraints));
    }
    for (guard, gbinds) in &plan.top1_guards {
        let bm = crate::runtime::interp::resolve_binds(gbinds, frame)?;
        let mut stream = ex.query(guard, &bm)?;
        let clean = matches!(
            stream.fetch_many(1).first().and_then(|r| r.first()),
            Some(crate::value::SpinValue::Int(0))
        );
        if !clean {
            return Ok(Err(SkipReason::AmbiguousTop1Tie));
        }
    }
    for guard in &plan.route_guards {
        let mut stream = ex.query(guard, &crate::exec::BindMap::new())?;
        let unreachable = matches!(
            stream.fetch_many(1).first().and_then(|r| r.first()),
            Some(crate::value::SpinValue::Bool(true))
        );
        if !unreachable {
            return Ok(Err(SkipReason::UnsupportedControlFlow));
        }
    }
    for (guard, binds) in &plan.expr_guards {
        let bm = crate::runtime::interp::resolve_binds(binds, frame)?;
        let mut stream = ex.query(guard, &bm)?;
        let clean = matches!(
            stream.fetch_many(1).first().and_then(|r| r.first()),
            Some(crate::value::SpinValue::Int(0))
        );
        if !clean {
            return Ok(Err(SkipReason::UnprovenQueryTotalness));
        }
    }
    Ok(Ok(()))
}

fn snapshot_matches(
    ex: &dyn SqlExecutor,
    plan: &ConversionPlan,
    ctx: &OptimizeCtx<'_>,
) -> Result<bool, EngineError> {
    if let Some(snapshot) = ctx.snapshot {
        if !schema::matches_snapshot(ex, snapshot, &plan.source, ctx.rules)?
            || !schema::matches_snapshot(ex, snapshot, &plan.target, ctx.rules)?
        {
            return Ok(false);
        }
        for (name, _) in &plan.lookup_tables {
            if !schema::matches_snapshot(ex, snapshot, name, ctx.rules)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// 运行期结构漂移：表缺失、可见性（唯一索引）或列结构与生成期证据不一致。
fn drifted(ex: &dyn SqlExecutor, name: &Name, expected: &Table) -> Result<bool, EngineError> {
    // 离线 SQL 使用未限定名；load 已证明它唯一绑定当前库/模式的普通表。
    match schema::load(ex, name)? {
        Some(t) => Ok(t.columns != expected.columns
            || (t.qualified != expected.qualified && expected.qualified != name.sql())),
        None => Ok(true),
    }
}
