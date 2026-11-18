//! 区域入口运行状态机（设计 §4.5-2）：前缀执行完毕后的不抛错守卫与续跑产物。

use super::schema::{self, Table};
use super::shape::Name;
use super::{BindSrc, BufferPolicy, IrProg, IrStmt, OptimizeCtx, SkipReason};
use crate::catalog::CompiledRoutine;
use crate::error::EngineError;
use crate::exec::SqlExecutor;

/// 结构准入与 SQL 生成的产物；入口守卫与后缀组装消费它。
#[derive(Clone)]
pub(crate) struct ConversionPlan {
    pub sql: String,
    /// 批写 SQL 的按名绑定（游标查询绑定 + INSERT 绑定合并）。
    pub(super) binds: Vec<(String, BindSrc)>,
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

/// 集合化后缀程序：批量 SqlExec + 条件尾提交 + 原始后缀。
/// `TxCommitIfWritten` 由同一运行内的 SqlExec 影响行数驱动（零行不提交）。
pub(crate) fn suffix_prog(routine: &CompiledRoutine, plan: &ConversionPlan) -> IrProg {
    let mut code = vec![IrStmt::SqlExec {
        sql: plan.sql.clone(),
        binds: plan.binds.clone(),
        fn_sites: Vec::new(),
        buffer_policy: BufferPolicy::Immediate,
    }];
    if plan.tail_commit {
        code.push(IrStmt::TxCommitIfWritten);
    }
    code.extend_from_slice(&routine.prog.code[plan.close_pc + 1..]);
    IrProg {
        code,
        var_types: routine.prog.var_types.clone(),
        local_ddl: routine.prog.local_ddl.clone(),
    }
}
