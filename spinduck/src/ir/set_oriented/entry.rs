//! 区域入口运行状态机（设计 §4.5-2）：前缀执行完毕后的不抛错守卫与续跑产物。

use super::schema::{self, Table};
use super::shape::Name;
use super::{BindSrc, OptimizeCtx, SkipReason, VarName};
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

/// 视图物化前导（普通查找落在视图上）：区域入口把视图物化为
/// `sd_vwN` 临时表，批写/守卫中的查找派生表改引用临时表。物化与
/// 原解释器逐行求值视图等价的前提是视图 SQL 确定（无易变函数、
/// 目录例程均 PureExpr）；物化失败由调用方回退原区域——行级 trap
/// 兜底语义在解释路径下逐行复现，物化失败不构成调用失败。
#[derive(Clone)]
pub(crate) struct ViewPrelude {
    /// 物化 SQL：`CREATE OR REPLACE TEMP TABLE sd_vwN AS SELECT * FROM <视图>`
    /// （临时表名 sd_vwN 与批写/守卫 SQL 中的派生表引用一致）。
    pub sql: String,
}

/// 批写后标量出口（miss 仿真处理器赋值回填）：批写成功后、后缀续跑
/// 前执行；返回行时把各列值按 `targets` 顺序写回帧，无行保持批写前值。
/// 多个出口按查找程序序应用（后写覆盖先写）。
#[derive(Clone)]
pub(crate) struct MissOut {
    pub targets: Vec<VarName>,
    pub sql: String,
    pub binds: Vec<(String, BindSrc)>,
}

/// 结构准入与 SQL 生成的产物；入口守卫与后缀组装消费它。
#[derive(Clone)]
pub(crate) struct ConversionPlan {
    pub sql: String,
    /// 批写 SQL 的按名绑定（游标查询绑定 + INSERT 绑定合并）。
    pub(crate) binds: Vec<(String, BindSrc)>,
    /// 批写前须物化的例程缓存表（游标 WHERE fn_site，按声明序执行）。
    pub(crate) fn_preludes: Vec<FnPrelude>,
    /// 视图物化前导（普通查找落在视图上）：先于入口守卫执行
    /// （守卫可内嵌 sd_vwN 派生表），失败回退原区域。
    pub(crate) view_preludes: Vec<ViewPrelude>,
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
    /// 混合调度异常行守卫（so-exception-row-hybrid）：每条返回违规行
    /// `__sd_rn`（0 基）集合；非空时引擎走"正常行批段+异常行单行重放"。
    /// 仅 `PlanFlags.hybrid` 计划填充，此时对应 dup 守卫已从
    /// `top1_guards` 移除（不再整段退回）。
    pub(crate) exc_guards: Vec<BoundGuard>,
    /// 混合调度规格（游标/FETCH pc/唯一出口 pc）；`exc_guards` 非空
    /// 的充要条件——引擎据此走分段批写+单行重放。
    pub(crate) hybrid: Option<super::hybrid::HybridSpec>,
    /// 混合段批写 SQL 模板（含 `__SD_LO__`/`__SD_HI__` 占位符）；
    /// `sql` 字段为占位符替换 NULL 后的全行版本（exc 空集路径直发，
    /// 与无混合计划行为等价）。
    pub(crate) seg_sql: Option<String>,
    /// IF 路由入口守卫 SQL（期望 TRUE = THEN 分支不可达，§4.3）。
    pub(super) route_guards: Vec<String>,
    /// 内联表达式入口无抛错守卫 SQL（期望 COUNT = 0，§4.5-2）：以 TRY_
    /// 全定义预筛全部源行，任一行结果为 NULL（坏输入或 NULL 输入）即退回。
    pub(super) expr_guards: Vec<BoundGuard>,
    /// miss 仿真标量出口（NO_DATA_FOUND 处理器赋值回填）：批写成功后
    /// 按程序序执行，返回行时写回帧变量。
    pub(crate) miss_outs: Vec<MissOut>,
    /// THEN 臂备选计划（分臂备选）：本计划为 ELSE 臂（route_guards 证
    /// THEN 不可达）；主计划入口守卫失败时尝试备选，其 route_guards
    /// 为"条件 IS TRUE"判定（THEN 可达才放行）。视图物化前导不兼容
    /// 双计划（sd_vwN 序号冲突），含前导的备选在计划期已丢弃。
    pub(crate) route_alt: Option<Box<ConversionPlan>>,
}

pub(crate) type BoundGuard = (String, Vec<(String, BindSrc)>);

/// 两份 fn 物化前导逐项全等（表名/例程/去重 SQL/绑定/元数/依赖标记）。
/// 分臂备选计划共用同一份入口期物化结果的前提——INSERT 位 sd_bfN 的
/// distinct-over-sd_cur 按臂投影渲染，同名表内容可不同，不等即不可挂载。
pub(crate) fn same_fn_preludes(a: &[FnPrelude], b: &[FnPrelude]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.table == y.table
                && x.name == y.name
                && x.distinct_sql == y.distinct_sql
                && x.binds == y.binds
                && x.arity == y.arity
                && x.needs_sd_cur == y.needs_sd_cur
        })
}

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

/// 混合调度异常行集（so-exception-row-hybrid §3）：逐条 `exc_guards`
/// 求值并集，返回 0 基行号集合（对齐 `CursorState::seek_before` 与批写
/// `__sd_rn` 区间）。守卫 SQL/绑定与 `top1_guards` 同口径（计划期已
/// 补齐 $v_ 形参）。
///
/// 返回值三态：`Some(空集)` = 无异常行走普通 SO；`Some(集)` = 混合
/// 调度；`None` = 守卫产出畸形行号（非 Int/负值/空行）——证据不可信，
/// 调用方必须整区域串行兜底，**绝不**把畸形值静默当普通行跳过
/// （P0-3 审核修复：此前非 Int 被 `if let` 静默丢弃）。
pub(crate) fn exc_rows(
    ex: &dyn SqlExecutor,
    guards: &[BoundGuard],
    frame: &crate::runtime::Frame,
) -> Result<Option<std::collections::BTreeSet<u64>>, EngineError> {
    let mut rows = std::collections::BTreeSet::new();
    for (sql, binds) in guards {
        let bm = crate::runtime::interp::resolve_binds(binds, frame)?;
        let mut stream = ex.query(sql, &bm)?;
        for row in stream.fetch_many(usize::MAX) {
            match row.first() {
                Some(crate::value::SpinValue::Int(rn)) if *rn >= 0 => {
                    rows.insert(*rn as u64);
                }
                _ => return Ok(None),
            }
        }
    }
    Ok(Some(rows))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FakeExecutor;
    use crate::value::SpinValue;

    fn guards() -> Vec<BoundGuard> {
        vec![("SELECT __sd_rn FROM g".to_string(), Vec::new())]
    }

    /// 正常行号集：多条守卫并集、去重、0 基原样收集。
    #[test]
    fn should_collect_exception_row_numbers() {
        let ex = FakeExecutor::with_rows(
            vec!["rn".into()],
            vec![
                vec![SpinValue::Int(0)],
                vec![SpinValue::Int(2)],
                vec![SpinValue::Int(2)],
            ],
        );
        let frame = crate::runtime::Frame::new();
        let got = exc_rows(&ex, &guards(), &frame).unwrap();
        assert_eq!(got, Some(std::collections::BTreeSet::from([0, 2])));
    }

    /// 畸形行号（非 Int / 负值）→ `None`：证据不可信，调用方必须
    /// 整区域串行兜底，绝不静默当普通行。
    #[test]
    fn should_reject_malformed_exception_row_numbers() {
        for bad in [
            vec![SpinValue::Str("x".into())],
            vec![SpinValue::Int(-1)],
            vec![SpinValue::Null],
        ] {
            let ex = FakeExecutor::with_rows(vec!["rn".into()], vec![bad]);
            let frame = crate::runtime::Frame::new();
            assert_eq!(exc_rows(&ex, &guards(), &frame).unwrap(), None);
        }
    }

    /// 空守卫结果集 → `Some(空集)`：无异常行，走普通 SO 批写。
    #[test]
    fn should_return_empty_set_when_no_exception_rows() {
        let ex = FakeExecutor::with_rows(vec!["rn".into()], Vec::new());
        let frame = crate::runtime::Frame::new();
        let got = exc_rows(&ex, &guards(), &frame).unwrap();
        assert_eq!(got, Some(std::collections::BTreeSet::new()));
    }
}
