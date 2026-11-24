//! 受限静态游标集合化；保留原始 IR，离线候选在调用入口同事务复核。

pub mod admit;
mod agg;
mod analyze;
mod candidate;
mod defaults;
pub use candidate::{compile_set_oriented_plans, CandidatePlan, CompiledPlans, SchemaProvider};
mod catalog_summary;
mod copy;
mod dead;
mod entry;
mod expr;
mod filter;
mod generate;
use generate::generate;
mod lookup;
pub mod metadata;
mod planbinds;
mod region;
mod resolve;
mod route;
mod schema;
mod shape;
mod vguard;
mod viewdep;

pub(crate) use entry::{entry_ok, ConversionPlan, Discriminator, FnPrelude};

use super::{AssignTarget, BinOp, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use crate::catalog::{CompiledRoutine, ParamMode};
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use analyze::{analyze_region, analyze_then_arm};
mod status;
pub(crate) use status::PlanRejection;
pub use status::{Arm, GuardKind, GuardScope, OnSkip, Report, SetOrientedMode, SkipReason, Status};
/// 下沉算子：PL 除法（整数 `//`）与拼接（双 NULL CASE）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SinkOp {
    Div,
    Concat,
}

/// §3.3 条件算术覆写：IF 门内 `var := <DECIMAL 乘/减表达式>`，
/// 集合化投影为 `CASE WHEN <cond> THEN <arith> ELSE <原值> END`。
#[derive(Clone)]
pub(super) struct ArithWrite {
    pub var: VarName,
    pub cond: crate::ir::IrExpr,
    pub expr: crate::ir::IrExpr,
}

/// INSERT 位 fn_site（缓存表改名 `sd_bfN`）：`distinct_sql` 为无 FROM
/// 实参数列表，批写时在投影环境下改写为 `SELECT DISTINCT <实参>
/// FROM sd_cur <joins>` 前导物化；`orig_table` 为编译产物中的 `sd_fnN`，
/// 缓存子查询实参渲染时据此换名。
#[derive(Clone)]
pub(super) struct InsertSite {
    pub orig_table: String,
    pub table: String,
    pub name: String,
    pub distinct_sql: String,
    pub binds: Vec<(String, BindSrc)>,
    pub arity: usize,
}

struct CopyLoop<'a> {
    pub(super) query: &'a str,
    pub(super) insert: &'a str,
    pub(super) binds: &'a [(String, BindSrc)],
    /// 游标查询自身的绑定（WHERE 引用形参等），须随批写 SQL 一并携带。
    pub(super) query_binds: &'a [(String, BindSrc)],
    /// 游标 WHERE 内 SQL 上下文例程调用（已证位于顶层 WHERE 之后）：
    /// 由 FnPrelude 在批写前按 distinct 元组物化缓存表。
    pub(super) fn_sites: &'a [crate::sql::FnSite],
    /// INSERT 参数内 SQL 上下文例程调用（PureExpr + 无 FROM 实参列表）：
    /// distinct-over-sd_cur 前导物化，缓存表名已改 `sd_bfN`。
    pub(super) insert_sites: Vec<InsertSite>,
    /// 变量类型表：过滤式无抛错证明裁决绑定值域（Varchar↔str / 数值↔num）。
    pub(super) var_types: &'a std::collections::BTreeMap<String, crate::types::SpinType>,
    pub(super) record: &'a VarName,
    /// 区域程序 IR：内联 Lookup Var 实参的循环不变性证明依据（§4.3 A7）。
    pub(super) prog: &'a crate::ir::IrProg,
    /// BatchAtomic × trap：授权以列级守卫过滤坏行（跳行语义等价）。
    pub(super) allow_filter: bool,
    /// §3.5-4 INSERT 跳起型处理器（非 NoOp inst_log）：入口守卫须证
    /// bad_rows==0，处理器不可达时副作用不观测；非零整段退回。
    pub(super) insert_log: bool,
    /// PL 表达式下沉：变量 → (算子, 左字段, 右字段)，仅字段/字段形态。
    pub(super) sinks: Vec<(VarName, SinkOp, String, String)>,
    /// 兜底型循环不变查找（§4.4 B1）：聚合标量子查询替代 SELECT INTO。
    pub(super) lookups: Vec<lookup::LookupQuery>,
    /// 多 OUT 查找函数内联（§3.2 LookupTuple）：单条 GROUP BY 派生表 +
    /// 每变量 `CASE WHEN sd_cnt = 1 THEN 列 ELSE 兜底` 投影。
    pub(super) tuples: Vec<lookup::TupleQuery>,
    /// 条件无 GROUP BY 聚合查表（§3.4）：trap 包裹 SUM + 字段覆写投影。
    pub(super) aggs: Vec<agg::AggQuery>,
    /// 聚合结果写回游标字段（可挂 IF 门），驱动 CASE 投影。
    pub(super) field_writes: Vec<agg::FieldWrite>,
    /// IF 路由（§4.3）：(查找变量, 常量 SQL)，入口守卫证明 THEN 不可达。
    pub(super) route: Option<(VarName, String)>,
    /// 本 CopyLoop 是双臂计划的 ELSE 臂（§12.3）：generate 据此把
    /// route_guards 换成判别式 SQL，route 不可达证明改由选臂承担。
    pub(super) dual_arm: bool,
    /// 条件算术覆写（§3.3）：IF 门内变量重赋值，投影外层 CASE。
    pub(super) arith_writes: Vec<ArithWrite>,
    /// PureExpr 常量折叠（§4.3 A7）：赋值变量 → 常量；仅用于静态剪枝
    /// 与出口活跃证明，不参与批写投影。
    pub(super) folded: Vec<(VarName, crate::value::SpinValue)>,
    /// 纯函数符号内联（§4.3 A7）：赋值变量 → 内联 IrExpr，参与批写投影渲染。
    pub(super) inlined: Vec<(VarName, IrExpr)>,
    /// 覆盖赋值前已求值的表达式仍须通过入口守卫，不能随死值消除错误。
    pub(super) inline_checks: Vec<(VarName, IrExpr)>,
    pub(super) varchar_inputs: Vec<IrExpr>,
    /// 仅记录赋值前的跳行查找；后续查找不能屏蔽先前的裸表达式错误。
    pub(super) inline_preconditions: std::collections::BTreeMap<String, Vec<VarName>>,
    /// 死存储消除（§5 清单 g）：全程序无读取变量的常量赋值，整体跳过。
    pub(super) dead: Vec<VarName>,
    /// 区域写集（§3.5 V3-5 视图底表 Reads∩Writes 复核）。
    pub(super) writes: std::collections::BTreeSet<String>,
    /// 本过程 IN 形参名（小写）：循环不变的调用方输入，可作批写投影。
    pub(super) in_params: std::collections::BTreeSet<String>,
}

/// 一次调用选择所需的引擎上下文（模式、开关、编译期证据）。
pub(crate) struct OptimizeCtx<'a> {
    pub mode: SetOrientedMode,
    pub batch_commit: bool,
    pub rules: &'a crate::rules::RuleRegistry,
    pub snapshot: Option<&'a metadata::SchemaSnapshot>,
    pub catalog: &'a crate::catalog::RoutineCatalog,
}

/// 结构准入 + SQL 生成（前缀执行前的只读证明面）；入口守卫与续跑产物
/// 由 [`entry`] 在前缀执行完毕后消费（设计 §4.5-2 五步状态机）。
pub(crate) fn plan(
    routine: &CompiledRoutine,
    ex: &dyn SqlExecutor,
    ctx: &OptimizeCtx<'_>,
) -> Result<Result<entry::ConversionPlan, SkipReason>, EngineError> {
    plan_from(routine, &schema::Source::Runtime(ex), ctx).map(|r| r.map_err(|e| e.reason))
}

fn plan_from(
    routine: &CompiledRoutine,
    source: &schema::Source<'_>,
    ctx: &OptimizeCtx<'_>,
) -> Result<Result<entry::ConversionPlan, PlanRejection>, EngineError> {
    let mode = ctx.mode;
    // §2.2-3：仅 BatchAtomic × --batch-commit 解除互斥——区域集合 SQL 不经
    // WriteBuffer，TxCommit trace 照发但实际提交延迟到 call_with_sink 出口；
    // Strict 仍拒绝（提交快照语义不允许合并）。
    let selected = if ctx.batch_commit && mode != SetOrientedMode::BatchAtomic {
        Err(PlanRejection::firm(SkipReason::BatchCommitEnabled))
    } else {
        match v2_admit(&routine.prog, mode, ctx.catalog) {
            Err(reason) => Err(PlanRejection::firm(reason)),
            Ok(region) => {
                if !source.sys_dummy_ok()? {
                    Err(PlanRejection::firm(SkipReason::UnknownObjectDependency))
                } else if routine
                    .params
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(&region.record.0))
                {
                    Err(PlanRejection::firm(SkipReason::LiveOutState))
                } else {
                    // §4.2：IN 形参是调用方给定的循环不变输入，可作批写投影；
                    // OUT/INOUT 形参有回写语义，不在此列。
                    let in_params = in_param_names(routine);
                    match analyze_region(&routine.prog, &region, mode, ctx.catalog, &in_params) {
                        Err(reason) => Err(PlanRejection::firm(reason)),
                        Ok(ra) => {
                            // D7 双臂（§12.3/§12.7）：route_site 存在时 THEN
                            // 臂独立走查+生成。臂分析证明失败 → 静默降级
                            // 单臂（PR#62 route_alt 口径：备选失败只丢弃备
                            // 选，不连坐主计划）——discriminator=None，主
                            // 计划 route_guards 守「THEN 不可达」，运行态
                            // 数据走 THEN 时守卫拒绝整段回退解释器（本分支
                            // 无 loop-parallel 派发，PR#62 合入后该缺口交
                            // LP 接管）。两臂均可证时才升级判别式选臂。
                            // ELSE 臂 = 今日走查产物。
                            let then_ra = match ra.route_site {
                                None => None,
                                Some(site) => analyze_then_arm(
                                    &routine.prog,
                                    &region,
                                    mode,
                                    ctx.catalog,
                                    &in_params,
                                    site,
                                )
                                .ok(),
                            };
                            gen_arm_plan(ra, routine, source, ctx, &region, mode, then_ra)?
                        }
                    }
                }
            }
        }
    };
    Ok(selected)
}

/// 单臂/双臂计划生成（D7 §12.3）：`then_ra = Some` 时 ELSE 臂标
/// `dual_arm`（generate 换判别式）、THEN 臂独立 generate 后填入
/// `Discriminator.arms`。主臂失败 = 整体 `PlanRejection`；THEN 臂
/// 生成失败 = 静默降级单臂（route_alt 口径覆盖分析+生成两阶段，
/// 主计划 route_guards 兜底「THEN 不可达」）。
fn gen_arm_plan<'a>(
    ra: analyze::RegionAnalysis<'a>,
    routine: &CompiledRoutine,
    source: &schema::Source<'_>,
    ctx: &OptimizeCtx<'_>,
    region: &region::CursorRegion,
    mode: SetOrientedMode,
    then_ra: Option<analyze::RegionAnalysis<'a>>,
) -> Result<Result<entry::ConversionPlan, PlanRejection>, EngineError> {
    let mut ra = ra;
    // 下沉/查找变量若是 OUT/INOUT 形参，出口写回不可消除
    if live_param_hit(&ra.copy, routine) {
        return Ok(Err(PlanRejection::firm(SkipReason::LiveOutState)));
    }
    // D7 §12.3(b)：两臂写集之差中「本臂恒 NULL」的变量候选——判别式
    // 选臂后兄弟臂不可达，其独占写入不发生；声明初始化/显式置空
    // 之外的写入点出现即放弃候选（保守）。
    let arm_nulls = then_ra
        .as_ref()
        .map(|t| arm_null_candidates(routine, region, ra.route_site, &ra.copy, &t.copy));
    // 先证 THEN 臂可生成：失败静默降级（丢弃备选不连坐主计划），
    // 主臂按单臂口径 generate（discriminator=None + route_guards）。
    let then_plan = match then_ra {
        None => None,
        Some(then) => {
            if live_param_hit(&then.copy, routine) {
                return Ok(Err(PlanRejection::firm(SkipReason::LiveOutState)));
            }
            let mut tcopy = then.copy;
            if let Some(nulls) = &arm_nulls {
                for v in nulls {
                    tcopy
                        .folded
                        .push((v.clone(), crate::value::SpinValue::Null));
                }
            }
            generate(&mut tcopy, source, ctx, region, then.tail_commit)?.ok()
        }
    };
    if then_plan.is_some() {
        ra.copy.dual_arm = true;
    }
    let mut copy = ra.copy;
    let mut plan = match generate(&mut copy, source, ctx, region, ra.tail_commit)? {
        Err(reason) => {
            return Ok(Err(PlanRejection {
                reason: rollback_scope_reason(mode, ra.has_trap, reason),
                // generate 消费 Source 探测：Static 下拒因
                // 可能由探测缺口导出（admit F2 → conservative）。
                probe_suspect: matches!(source, schema::Source::Static(_)),
            }));
        }
        Ok(p) => p,
    };
    if let Some(tplan) = then_plan {
        if let Some(d) = &mut plan.discriminator {
            d.arms.push(entry::RouteArm {
                plan: Box::new(tplan),
            });
        }
    }
    Ok(Ok(plan))
}

/// 臂写集：CopyLoop 各事实结构登记的全部目标变量。
fn arm_write_vars(c: &CopyLoop<'_>) -> std::collections::BTreeSet<VarName> {
    let mut s = std::collections::BTreeSet::new();
    s.extend(c.lookups.iter().map(|l| l.var.clone()));
    s.extend(
        c.tuples
            .iter()
            .flat_map(|t| t.slots.iter().map(|sl| sl.var.clone())),
    );
    s.extend(c.sinks.iter().map(|(v, _, _, _)| v.clone()));
    s.extend(c.arith_writes.iter().map(|w| w.var.clone()));
    s.extend(c.inlined.iter().map(|(v, _)| v.clone()));
    s.extend(c.folded.iter().map(|(v, _)| v.clone()));
    s.extend(c.aggs.iter().map(|a| a.var.clone()));
    s.extend(c.field_writes.iter().map(|f| f.var.clone()));
    s.extend(c.dead.iter().cloned());
    s
}

/// 语句是否写入变量 `v`（覆盖 Assign/查找 INTO/动态 INTO/FETCH/
/// CALL 写回/OPEN 游标名；Field 目标写的是记录字段而非变量本身）。
fn stmt_writes_var(stmt: &IrStmt, v: &VarName) -> bool {
    let is_var = |t: &AssignTarget| matches!(t, AssignTarget::Var(tv) if tv == v);
    match stmt {
        IrStmt::Assign { target, .. } | IrStmt::FoldedQuery { target, .. } => is_var(target),
        IrStmt::SqlQuery { into, .. } => into.iter().any(is_var),
        IrStmt::SqlExecDynamic { into_targets, .. } => into_targets.iter().any(is_var),
        IrStmt::Fetch { targets, .. } => targets.iter().any(is_var),
        IrStmt::CallRoutine { out_targets, .. } => out_targets.iter().flatten().any(is_var),
        IrStmt::OpenCursor { cursor, .. } => cursor == v,
        _ => false,
    }
}

/// D7 §12.3(b) 臂内 NULL 事实候选：`else − then` 写集差中，全部
/// 写入点要么位于兄弟臂 pc 区间（本臂选中时不可达），要么是
/// `Assign Const(Null)`（声明初始化/显式置空）→ 本臂读到的值恒 NULL。
/// 形参（IN/OUT/INOUT）有调用方值、记录变量与游标名排除；任一其它
/// 写入点（非空默认、前缀赋值、处理器写回等）即放弃该候选。
fn arm_null_candidates(
    routine: &CompiledRoutine,
    region: &region::CursorRegion,
    site: Option<analyze::RouteSite>,
    else_copy: &CopyLoop<'_>,
    then_copy: &CopyLoop<'_>,
) -> Vec<VarName> {
    let Some(site) = site else {
        return Vec::new();
    };
    let else_vars = arm_write_vars(else_copy);
    let then_vars = arm_write_vars(then_copy);
    let params: std::collections::BTreeSet<String> = routine
        .params
        .iter()
        .map(|p| p.name.to_ascii_lowercase())
        .collect();
    let mut out = Vec::new();
    'cand: for v in else_vars.difference(&then_vars) {
        if params.contains(&v.0.to_ascii_lowercase()) || *v == region.record {
            continue;
        }
        for (pc, stmt) in routine.prog.code.iter().enumerate() {
            if !stmt_writes_var(stmt, v) || in_else_arm(site, region, pc) {
                continue;
            }
            let null_init = matches!(
                stmt,
                IrStmt::Assign {
                    target: AssignTarget::Var(tv),
                    expr: IrExpr::Const(crate::value::SpinValue::Null),
                } if tv == v
            );
            if !null_init {
                continue 'cand;
            }
        }
        out.push(v.clone());
    }
    out
}

/// pc 是否落在兄弟臂（ELSE）区间内：Branch 形态 ELSE=[to, 汇合点)，
/// TrapGroup 形态 ELSE=查找组 [else_start, resume_pc)。
fn in_else_arm(site: analyze::RouteSite, region: &region::CursorRegion, pc: usize) -> bool {
    match site {
        analyze::RouteSite::Branch { to, .. } => (to..region.close_pc - 1).contains(&pc),
        analyze::RouteSite::TrapGroup {
            else_start,
            resume_pc,
            ..
        } => (else_start..resume_pc).contains(&pc),
    }
}

/// 集合化产物引用了例程形参：该变量的值须随 OUT/INOUT 回写，无法消除。
fn live_param_hit(copy: &CopyLoop<'_>, routine: &CompiledRoutine) -> bool {
    let live_param = |v: &VarName| {
        routine
            .params
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&v.0))
    };
    copy.sinks.iter().any(|(v, _, _, _)| live_param(v))
        || copy.lookups.iter().any(|l| live_param(&l.var))
        || copy.folded.iter().any(|(v, _)| live_param(v))
        || copy.inlined.iter().any(|(v, _)| live_param(v))
        || copy.dead.iter().any(live_param)
        || copy.aggs.iter().any(|a| live_param(&a.var))
        || copy.arith_writes.iter().any(|w| live_param(&w.var))
}

/// 例程 IN 形参名集（小写）：批写投影仅承认这些循环不变输入。
fn in_param_names(routine: &CompiledRoutine) -> std::collections::BTreeSet<String> {
    routine
        .params
        .iter()
        .filter(|p| p.mode == ParamMode::In)
        .map(|p| p.name.to_ascii_lowercase())
        .collect()
}

/// V2 准入前置：区域抽取 + 前缀干净度 + 效果准入 + Trap 体限制 + 写集 + 读写不交 + 出口活跃。
fn v2_admit(
    prog: &IrProg,
    mode: SetOrientedMode,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<region::CursorRegion, SkipReason> {
    let region = region::extract(prog)?;
    // §2.1 不变量 1：run_spanned 每段空陷阱栈起步，分段边界的静态陷阱
    // 深度必须为 0，否则边界两侧的异常处理语义无法跨段保持。
    region::reject_trap_boundaries(prog, &region)?;
    region::reject_prefix_effects(prog, &region)?;
    // 后缀不再做效果准入：RunSpan 在原 IrProg 上按原 PC 解释执行，
    // 例程调用、动态游标与 EXECUTE IMMEDIATE 均为解释器原生能力；
    // 变量依赖由 live_out_check 兜底（设计 §2.1/§4 V3A）。
    if mode == SetOrientedMode::BatchAtomic {
        region::prefix_is_clean(prog, &region)?;
    }
    region::admit_effects(prog, &region, mode, catalog)?;
    region::trap_bodies_admissible(prog, &region, catalog)?;
    region::write_set(prog, &region, catalog)?;
    region::reads_writes_disjoint(prog, &region, catalog)?;
    region::live_out_check(prog, &region)?;
    Ok(region)
}

/// Strict × 跳行 trap：写入全量证明失败时，内核报错经 reset_session 撤销
/// 前序未提交写入，坏行过滤与原回滚效果不等价（设计 §4.5 第 1 项）。
/// 结构性原因（对象缺失/读写同表）保留原诊断，不属回滚域问题。
fn rollback_scope_reason(mode: SetOrientedMode, has_trap: bool, reason: SkipReason) -> SkipReason {
    if mode == SetOrientedMode::Strict
        && has_trap
        && matches!(
            reason,
            SkipReason::UnsupportedWrite
                | SkipReason::UnprovenExpression
                | SkipReason::UnprovenTargetConstraints
                | SkipReason::UnprovenWriteCompleteness
        )
    {
        return SkipReason::UnprovenRollbackScope;
    }
    reason
}

/// 单遍、引号感知的 `$v_*` 占位符替换：SQL 字符串字面量与双引号标识符
/// 内部不替换，替换文本不再重扫（防同名字面量文本污染与级联替换，§4.6 值等价）。
/// `repls` 需按占位符长度降序，长名优先避免 `$v_x` 误伤 `$v_x__f`。
fn substitute_binds(text: &str, repls: &[(String, String)]) -> String {
    fn ident_tail(rest: &str) -> bool {
        rest.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut quote = None;
    while let Some(ch) = rest.chars().next() {
        match quote {
            Some(q) => {
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
                if ch == q {
                    if rest.starts_with(q) {
                        out.push(q);
                        rest = &rest[q.len_utf8()..];
                    } else {
                        quote = None;
                    }
                }
            }
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    out.push(ch);
                    rest = &rest[1..];
                } else if ch == '$' {
                    match repls.iter().find(|(ph, _)| {
                        rest.starts_with(ph.as_str()) && !ident_tail(&rest[ph.len()..])
                    }) {
                        Some((ph, r)) => {
                            out.push_str(r);
                            rest = &rest[ph.len()..];
                        }
                        None => {
                            out.push(ch);
                            rest = &rest[1..];
                        }
                    }
                } else {
                    out.push(ch);
                    rest = &rest[ch.len_utf8()..];
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
