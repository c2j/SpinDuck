//! 区域入口运行状态机（设计 §4.5-2）：前缀执行完毕后的不抛错守卫与续跑产物。

use super::schema::{self, Table};
use super::shape::Name;
use super::{BindSrc, GuardKind, GuardScope, OptimizeCtx, SkipReason, VarName};
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
    /// IF 路由入口守卫 SQL（期望 TRUE = THEN 分支不可达，§4.3）。
    pub(super) route_guards: Vec<String>,
    /// 内联表达式入口无抛错守卫 SQL（期望 COUNT = 0，§4.5-2）：以 TRY_
    /// 全定义预筛全部源行，任一行结果为 NULL（坏输入或 NULL 输入）即退回。
    pub(super) expr_guards: Vec<BoundGuard>,
    /// miss 仿真标量出口（NO_DATA_FOUND 处理器赋值回填）：批写成功后
    /// 按程序序执行，返回行时写回帧变量。
    pub(crate) miss_outs: Vec<MissOut>,
    /// 双臂路由（设计 v3.1 §12.3(a)）：`Some` 时本计划是 ELSE 臂（值域
    /// 补集），`route_guards` 必须为空——THEN 不可达改由运行期判别式
    /// 取值选臂证明。`None` = 今日单臂形态，行为逐字不变。
    pub(crate) discriminator: Option<Discriminator>,
}

/// 双臂判别式（§12.3(a/d)）：`sql` 返回整型标签——`0..arms.len()-1`
/// = 对应臂计划、`arms.len()` = ELSE 臂（主计划本体，值域补集；
/// 判别值 NULL 经 SQL `=` 三值语义同落 ELSE，与 PL IF 及 PR#62
/// `route::pick` 口径一致）。标签越界/非整数/查询不产恰一行 →
/// `RouteDiscriminatorNotTotal` 整段回退（防御位）。臂等值比较在
/// SQL 侧完成（`=` 语义与原 IF 条件同源），不经 SpinValue 跨类型比对。
#[derive(Clone)]
pub(crate) struct Discriminator {
    pub sql: String,
    /// 非 ELSE 臂计划（值域划分；ELSE 臂即外层 ConversionPlan 本体）。
    pub arms: Vec<RouteArm>,
}

/// 判别式标签命中时选用的备选臂计划（独立走查+生成，§12.3(b/c)）。
#[derive(Clone)]
pub(crate) struct RouteArm {
    /// 该臂自己的 sql/binds/guards/preludes/tail_commit；`discriminator`
    /// 恒 `None`（臂内不再嵌套路由分叉）。
    pub plan: Box<ConversionPlan>,
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

/// plan 守卫的有序槽位视图：entry_ok 与 admit 共用同一求值序
/// （sys_dummy → drift(源/目标/查找) → snapshot → top1 → route → expr）。
pub(super) enum GuardSlot<'a> {
    SysDummy,
    Drift(&'a Name, &'a Table),
    Snapshot(&'a ConversionPlan),
    Top1(&'a BoundGuard),
    Route(&'a str),
    Expr(&'a BoundGuard),
}

impl<'a> GuardSlot<'a> {
    /// 守卫类别（admit 报告元数据）。
    pub(super) fn kind(&self) -> GuardKind {
        match self {
            Self::SysDummy => GuardKind::SysDummy,
            Self::Drift(..) => GuardKind::Drift,
            Self::Snapshot(_) => GuardKind::Snapshot,
            Self::Top1(_) => GuardKind::Top1Tie,
            Self::Route(_) => GuardKind::Route,
            Self::Expr(_) => GuardKind::ExprTotalness,
        }
    }

    /// 求值面：帧绑定守卫（top1/expr）= Frame，其余 = Static。
    pub(super) fn scope(&self) -> GuardScope {
        match self {
            Self::Top1(_) | Self::Expr(_) => GuardScope::Frame,
            _ => GuardScope::Static,
        }
    }

    /// 守卫关联对象名（drift=表名；sys_dummy="sys_dummy"；其余 None）。
    pub(super) fn object(&self) -> Option<String> {
        match self {
            Self::SysDummy => Some("sys_dummy".to_string()),
            Self::Drift(name, _) => Some(name.0.clone()),
            _ => None,
        }
    }

    /// 守卫 SQL 原文（route/top1/expr；目录面守卫无独立 SQL）。
    pub(super) fn sql(&self) -> Option<&'a str> {
        match self {
            Self::Top1(bound) | Self::Expr(bound) => Some(bound.0.as_str()),
            Self::Route(sql) => Some(sql),
            _ => None,
        }
    }

    /// 须随行的按名绑定占位符（top1/expr；route 为空绑定求值）。
    pub(super) fn bind_names(&self) -> Vec<String> {
        match self {
            Self::Top1(bound) | Self::Expr(bound) => {
                bound.1.iter().map(|(name, _)| name.clone()).collect()
            }
            _ => Vec::new(),
        }
    }
}

pub(super) fn guard_slots(plan: &ConversionPlan) -> Vec<GuardSlot<'_>> {
    let mut slots = Vec::with_capacity(
        3 + plan.lookup_tables.len()
            + plan.top1_guards.len()
            + plan.route_guards.len()
            + plan.expr_guards.len(),
    );
    slots.push(GuardSlot::SysDummy);
    slots.push(GuardSlot::Drift(&plan.source, &plan.source_table));
    slots.push(GuardSlot::Drift(&plan.target, &plan.target_table));
    slots.extend(
        plan.lookup_tables
            .iter()
            .map(|(name, table)| GuardSlot::Drift(name, table)),
    );
    slots.push(GuardSlot::Snapshot(plan));
    slots.extend(plan.top1_guards.iter().map(GuardSlot::Top1));
    slots.extend(plan.route_guards.iter().map(|sql| GuardSlot::Route(sql)));
    slots.extend(plan.expr_guards.iter().map(GuardSlot::Expr));
    slots
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
    for slot in guard_slots(plan) {
        if let Err(reason) = eval_guard(&slot, ex, ctx, frame)? {
            return Ok(Err(reason));
        }
    }
    Ok(Ok(()))
}

/// 单槽位求值：EngineError 原样上抛（`?`），逻辑不通过映射为 SkipReason。
pub(super) fn eval_guard(
    slot: &GuardSlot<'_>,
    ex: &dyn SqlExecutor,
    ctx: &OptimizeCtx<'_>,
    frame: &crate::runtime::Frame,
) -> Result<Result<(), SkipReason>, EngineError> {
    Ok(match slot {
        GuardSlot::SysDummy => {
            if schema::sys_dummy_ok(ex)? {
                Ok(())
            } else {
                Err(SkipReason::UnprovenSysDummyInvariant)
            }
        }
        GuardSlot::Drift(name, expected) => {
            if drifted(ex, name, expected)? {
                Err(SkipReason::UnprovenTargetConstraints)
            } else {
                Ok(())
            }
        }
        GuardSlot::Snapshot(plan) => {
            if snapshot_matches(ex, plan, ctx)? {
                Ok(())
            } else {
                Err(SkipReason::UnprovenTargetConstraints)
            }
        }
        GuardSlot::Top1(bound) => {
            if count_guard(ex, &bound.0, &bound.1, frame)? {
                Ok(())
            } else {
                Err(SkipReason::AmbiguousTop1Tie)
            }
        }
        GuardSlot::Route(sql) => {
            if route_guard(ex, sql)? {
                Ok(())
            } else {
                Err(SkipReason::UnsupportedControlFlow)
            }
        }
        GuardSlot::Expr(bound) => {
            if count_guard(ex, &bound.0, &bound.1, frame)? {
                Ok(())
            } else {
                Err(SkipReason::UnprovenQueryTotalness)
            }
        }
    })
}

/// 计数型守卫（top1/expr）：解析帧绑定 → 下发 SQL → 首行 Int(0) = 干净。
fn count_guard(
    ex: &dyn SqlExecutor,
    sql: &str,
    binds: &[(String, BindSrc)],
    frame: &crate::runtime::Frame,
) -> Result<bool, EngineError> {
    let bm = crate::runtime::interp::resolve_binds(binds, frame)?;
    let mut stream = ex.query(sql, &bm)?;
    Ok(matches!(
        stream.fetch_many(1).first().and_then(|r| r.first()),
        Some(crate::value::SpinValue::Int(0))
    ))
}

/// 路由守卫：无绑定求值，首行 TRUE = THEN 分支不可达。
fn route_guard(ex: &dyn SqlExecutor, sql: &str) -> Result<bool, EngineError> {
    let mut stream = ex.query(sql, &crate::exec::BindMap::new())?;
    Ok(matches!(
        stream.fetch_many(1).first().and_then(|r| r.first()),
        Some(crate::value::SpinValue::Bool(true))
    ))
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
    use crate::catalog::RoutineCatalog;
    use crate::exec::{BindMap, ExecOutcome, RowStream};
    use crate::rules::RuleRegistry;
    use crate::DuckExecutor;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 记录下发 SQL 的执行器包装：守卫序列是 entry_ok 可观察契约之一。
    struct Seen {
        inner: DuckExecutor,
        seen: Rc<RefCell<Vec<String>>>,
    }

    impl SqlExecutor for Seen {
        fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
            self.seen.borrow_mut().push(sql.to_string());
            self.inner.query(sql, binds)
        }
        fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
            self.seen.borrow_mut().push(sql.to_string());
            self.inner.execute(sql, binds)
        }
    }

    fn int_table(name: &str) -> Table {
        Table {
            qualified: Name(name.into()).sql(),
            columns: vec![schema::Column {
                name: Name("a".into()),
                kind: "integer".into(),
                nullable: true,
                default: None,
            }],
        }
    }

    fn plan(top1: Vec<BoundGuard>, route: Vec<String>, expr: Vec<BoundGuard>) -> ConversionPlan {
        ConversionPlan {
            sql: String::new(),
            binds: Vec::new(),
            fn_preludes: Vec::new(),
            view_preludes: Vec::new(),
            preludes_first: false,
            tail_commit: false,
            open_pc: 0,
            close_pc: 0,
            source: Name("s".into()),
            target: Name("t".into()),
            source_table: int_table("s"),
            target_table: int_table("t"),
            lookup_tables: Vec::new(),
            top1_guards: top1,
            route_guards: route,
            expr_guards: expr,
            miss_outs: Vec::new(),
            discriminator: None,
        }
    }

    /// 内存库：sys_dummy 恰 1 行 + s/t 各一整数列（漂移复核全过）。
    fn engine() -> (Seen, RoutineCatalog, RuleRegistry) {
        let inner = DuckExecutor::open_in_memory().unwrap();
        for sql in [
            "CREATE TABLE sys_dummy (x INTEGER)",
            "INSERT INTO sys_dummy VALUES (1)",
            "CREATE TABLE s (a INTEGER)",
            "CREATE TABLE t (a INTEGER)",
        ] {
            inner.execute(sql, &BindMap::new()).unwrap();
        }
        let seen = Rc::new(RefCell::new(Vec::new()));
        (
            Seen {
                inner,
                seen: seen.clone(),
            },
            RoutineCatalog::new(),
            RuleRegistry::default(),
        )
    }

    #[test]
    fn should_report_top1_reason_before_route_when_both_guards_fail() {
        // top1（帧槽位）排在 route（静态槽位）之前求值：两者同败时首个
        // 失败原因必须是 AmbiguousTop1Tie，且 route 守卫 SQL 不得下发。
        let (ex, catalog, rules) = engine();
        let seen = ex.seen.clone();
        let plan = plan(
            vec![("SELECT 1".into(), Vec::new())],
            vec!["SELECT FALSE".into()],
            Vec::new(),
        );
        let ctx = OptimizeCtx {
            mode: super::super::SetOrientedMode::Off,
            batch_commit: false,
            rules: &rules,
            snapshot: None,
            catalog: &catalog,
        };
        let frame = crate::runtime::Frame::new();
        let verdict = entry_ok(&ex, &plan, &ctx, &frame).unwrap();
        assert_eq!(verdict, Err(SkipReason::AmbiguousTop1Tie));
        let sqls = seen.borrow();
        assert_eq!(sqls.len(), 5, "sys_dummy×2 + drift×2 + top1×1: {sqls:?}");
        assert_eq!(sqls[4], "SELECT 1");
        assert!(!sqls.iter().any(|s| s == "SELECT FALSE"));
    }

    #[test]
    fn should_default_plan_to_single_arm_with_route_guards() {
        // 测试 13（§12.6）：discriminator=None 的单臂计划——route_guards
        // 保留、臂字段缺席，ELSE-only 形态逐字不变。
        let p = plan(vec![], vec!["SELECT TRUE".to_string()], vec![]);
        assert!(p.discriminator.is_none());
        assert_eq!(p.route_guards, ["SELECT TRUE"]);
    }

    #[test]
    fn should_tag_route_guard_as_static_and_top1_as_frame() {
        // 槽位次序 = entry_ok 原始次序；route 静态无绑定，top1/expr 帧绑定。
        let plan = plan(
            vec![(
                "SELECT 0".into(),
                vec![("$v_x".into(), BindSrc::Var(VarName("x".into())))],
            )],
            vec!["SELECT TRUE".into()],
            vec![("SELECT 0".into(), Vec::new())],
        );
        let tags: Vec<(GuardKind, GuardScope)> = guard_slots(&plan)
            .iter()
            .map(|s| (s.kind(), s.scope()))
            .collect();
        assert_eq!(
            tags,
            vec![
                (GuardKind::SysDummy, GuardScope::Static),
                (GuardKind::Drift, GuardScope::Static),
                (GuardKind::Drift, GuardScope::Static),
                (GuardKind::Snapshot, GuardScope::Static),
                (GuardKind::Top1Tie, GuardScope::Frame),
                (GuardKind::Route, GuardScope::Static),
                (GuardKind::ExprTotalness, GuardScope::Frame),
            ]
        );
        let route = guard_slots(&plan)
            .into_iter()
            .find(|s| s.kind() == GuardKind::Route)
            .unwrap();
        assert_eq!(route.sql(), Some("SELECT TRUE"));
        assert!(route.bind_names().is_empty());
        let top1 = guard_slots(&plan)
            .into_iter()
            .find(|s| s.kind() == GuardKind::Top1Tie)
            .unwrap();
        assert_eq!(top1.bind_names(), vec!["$v_x".to_string()]);
    }

    #[test]
    fn should_skip_with_route_reason_when_only_route_guard_fails() {
        // top1/expr 全过、route 不可达证明失败 → UnsupportedControlFlow；
        // route 守卫以空绑定表求值（无帧依赖）。
        let (ex, catalog, rules) = engine();
        let seen = ex.seen.clone();
        let plan = plan(
            vec![("SELECT 0".into(), Vec::new())],
            vec!["SELECT FALSE".into()],
            vec![("SELECT 0".into(), Vec::new())],
        );
        let ctx = OptimizeCtx {
            mode: super::super::SetOrientedMode::Off,
            batch_commit: false,
            rules: &rules,
            snapshot: None,
            catalog: &catalog,
        };
        let frame = crate::runtime::Frame::new();
        let verdict = entry_ok(&ex, &plan, &ctx, &frame).unwrap();
        assert_eq!(verdict, Err(SkipReason::UnsupportedControlFlow));
        let sqls = seen.borrow();
        assert_eq!(sqls.last().map(String::as_str), Some("SELECT FALSE"));
        assert_eq!(sqls.len(), 6, "expr 守卫不得执行: {sqls:?}");
    }
}
