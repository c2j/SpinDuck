//! 受限静态游标集合化；保留原始 IR，离线候选在调用入口同事务复核。

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
mod region;
mod resolve;
mod route;
mod schema;
mod shape;
mod vguard;

pub(crate) use entry::{entry_ok, ConversionPlan, FnPrelude};

use super::{AssignTarget, BinOp, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use crate::catalog::{CompiledRoutine, ParamMode};
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use analyze::analyze_region;

/// 集合化语义契约；所有模式均只执行已证明的转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetOrientedMode {
    /// 保持原始解释路径及步数预算语义。
    #[default]
    Off,
    /// 保持提交、值、错误域与异常回滚边界。
    Strict,
    /// 授权合并尾提交及依赖表入口单快照，不承诺崩溃时逐行部分持久化。
    BatchAtomic,
}

impl SetOrientedMode {
    /// 配置及命令行的规范模式名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Strict => "strict",
            Self::BatchAtomic => "batch-atomic",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "strict" => Some(Self::Strict),
            "batch-atomic" => Some(Self::BatchAtomic),
            _ => None,
        }
    }
}

impl From<bool> for SetOrientedMode {
    fn from(enabled: bool) -> Self {
        if enabled {
            Self::Strict
        } else {
            Self::Off
        }
    }
}

/// 未获得集合化证明的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    UnsupportedControlFlow,
    ExceptionScopePresent,
    TransactionBoundaryInsideLoop,
    BatchCommitEnabled,
    LiveOutState,
    UnsupportedWrite,
    TargetReadDependency,
    UnknownObjectDependency,
    UnprovenExpression,
    UnprovenTargetConstraints,
    /// Strict 下受跳行 trap 保护的区域无法证明写入全量：内核报错经
    /// reset_session 撤销前序未提交写入，过滤坏行与原回滚效果不等价。
    UnprovenRollbackScope,
    /// INSERT 表达式无法在 4 层证明中覆盖类型/精度/NULL/异常集等价。
    UnprovenWriteCompleteness,
    /// 省略的目标列含非常量/不确定默认值（序列、时间戳等），批写施加的
    /// 默认与逐行写不可证同值（设计 §4.1 A3）。
    NonDeterministicDefault,
    /// 区域入口 sys_dummy 存在性或恰 1 行不变量被前缀/运行期破坏。
    UnprovenSysDummyInvariant,
    /// 查找/游标查询自身无法证明全定义（非基数运行时错不可排除）。
    UnprovenQueryTotalness,
    /// 有序 Top-1 排序键无 PK/UNIQUE，入口按组并列一致性检查发现
    /// 异值并列（含 NULL 与值并列），首行选择存在真实歧义。
    AmbiguousTop1Tie,
    UnsupportedCall,
    /// 多 OUT 参数被调函数的 LookupTuple 证明失败：处理器漏赋 OUT/返回值、
    /// 处理器 Raise、或成功路径拷贝表达式可抛错（设计 §3.2）。
    MultiOutCallImpure,
    /// 语句残留非空 fn_sites：运行期物化有副作用（distinct_sql/临时表/
    /// MAX_TUPLES），Converted 计划绝不原样穿透（设计 §4.2）。
    UnresolvedFnSite,
    DynamicSql,
}

/// 一次调用实际选择的优化状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Converted { sql: String },
    Skipped { reason: SkipReason },
}

/// 原始例程和独立优化产物的对应报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub routine: String,
    pub status: Status,
}

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
    plan_from(routine, &schema::Source::Runtime(ex), ctx)
}

fn plan_from(
    routine: &CompiledRoutine,
    source: &schema::Source<'_>,
    ctx: &OptimizeCtx<'_>,
) -> Result<Result<entry::ConversionPlan, SkipReason>, EngineError> {
    let mode = ctx.mode;
    // §2.2-3：仅 BatchAtomic × --batch-commit 解除互斥——区域集合 SQL 不经
    // WriteBuffer，TxCommit trace 照发但实际提交延迟到 call_with_sink 出口；
    // Strict 仍拒绝（提交快照语义不允许合并）。
    let selected = if ctx.batch_commit && mode != SetOrientedMode::BatchAtomic {
        Err(SkipReason::BatchCommitEnabled)
    } else {
        match v2_admit(&routine.prog, mode, ctx.catalog) {
            Err(reason) => Err(reason),
            Ok(region) => {
                if !source.sys_dummy_ok()? {
                    Err(SkipReason::UnknownObjectDependency)
                } else if routine
                    .params
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(&region.record.0))
                {
                    Err(SkipReason::LiveOutState)
                } else {
                    // §4.2：IN 形参是调用方给定的循环不变输入，可作批写投影；
                    // OUT/INOUT 形参有回写语义，不在此列。
                    let in_params = in_param_names(routine);
                    match analyze_region(&routine.prog, &region, mode, ctx.catalog, &in_params) {
                        Err(reason) => Err(reason),
                        Ok((mut copy, tail, has_trap)) => {
                            // 下沉/查找变量若是 OUT/INOUT 形参，出口写回不可消除
                            if live_param_hit(&copy, routine) {
                                Err(SkipReason::LiveOutState)
                            } else {
                                match generate(&mut copy, source, ctx, &region, tail)? {
                                    Err(reason) => {
                                        Err(rollback_scope_reason(mode, has_trap, reason))
                                    }
                                    Ok(p) => Ok(p),
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(selected)
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
