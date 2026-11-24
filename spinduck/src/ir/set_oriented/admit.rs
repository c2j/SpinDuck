//! `compile --admit` 判定（设计 v3.1 §4.2）：L1 静态候选 + 入口守卫清单。
//!
//! `executor=None` 只出 `StaticOnly`/`conservative`，不碰内核；
//! `executor=Some` 走 L2 运行期重证并全量求值可求值的静态守卫
//! （F1 prelude 闸 / F3 宏闸在 PR-3 接入；EngineError 一律上抛，
//! 绝不降级为判定）。admit 不发 TraceSink 事件、不物化 preludes、
//! 不执行例程体——守卫求值面与 `call` 的 `entry_ok` 共用 GuardSlot
//! 原始次序，但 admit 全量求值而非短路。

use super::entry::{eval_guard, guard_slots, ConversionPlan, Discriminator};
use super::metadata::SchemaSnapshot;
use super::schema::Source;
use super::{plan_from, Arm, GuardKind, GuardScope, OptimizeCtx, PlanRejection, SetOrientedMode};
use crate::catalog::{CompiledRoutine, RoutineCatalog};
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use std::collections::BTreeSet;

/// admit 的数据面注入（生产 = codeweb 快照 + 可选真实库；测试 = 字面量构造）。
pub struct AdmitInput {
    /// 已按部署规则规范化的 codeweb 结构快照（L1 入参）。
    pub snapshot: SchemaSnapshot,
    /// `Some` = `--db` 数据面（L2 重证 + L3-a 求值）；`None` = 纯静态。
    pub executor: Option<Box<dyn SqlExecutor>>,
    /// 已宏化纯函数名集（F3 闸：守卫 SQL 命中 → Unchecked(macro-dependent)）。
    pub macro_names: BTreeSet<String>,
}

impl AdmitInput {
    /// 生产路径：codeweb 快照 + 可选真实库 + 已宏化名集。
    /// 只读取证：不注册例程、不写 catalog、不建临时表。
    #[cfg(feature = "native")]
    pub fn open(
        project: &crate::project::ProjectConfig,
        rules: &crate::rules::RuleRegistry,
        db: Option<&str>,
        macro_names: BTreeSet<String>,
    ) -> Result<Self, EngineError> {
        let root = crate::plan::codeweb::codeweb_project_root(project)?;
        let snapshot = crate::plan::codeweb::schema_snapshot(&root, rules)?;
        let executor = db
            .map(crate::DuckExecutor::open)
            .transpose()?
            .map(|e| Box::new(e) as Box<dyn SqlExecutor>);
        Ok(Self {
            snapshot,
            executor,
            macro_names,
        })
    }
}

/// 单条例程的判定结论。
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// 静态面与数据面守卫全部通过。
    Converted,
    /// 存在未决守卫（帧绑定 / prelude / 宏依赖 / 无数据面），不判红。
    Conditional,
    /// firm 或 conservative 拒因（confidence 字段区分档位）。
    Skipped(crate::ir::set_oriented::SkipReason),
    /// L1 通过但无数据面：守卫清单全 Unchecked，不作结论。
    StaticOnly,
}

impl Verdict {
    /// 工件 `verdict=` 取值（§6 契约）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Converted => "converted",
            Self::Conditional => "conditional",
            Self::Skipped(_) => "skipped",
            Self::StaticOnly => "static-only",
        }
    }
}

/// 判定档位（F2）：与 verdict 正交——conservative 不判红、不冒充 firm。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Firm,
    Conservative,
}

impl Confidence {
    /// stdout `confidence=` 与工件取值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firm => "firm",
            Self::Conservative => "conservative",
        }
    }
}

/// 守卫未决原因（§6 工件 `reason=` 取值域）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncheckedReason {
    /// 依赖运行帧绑定（top1/expr），admit 无帧。
    FrameBound,
    /// SQL 引用 sd_fn*/sd_vw*/sd_cur 缓存表，admit 不物化 preludes。
    PreludeDependent,
    /// SQL 引用已宏化纯函数，admit 不注册宏。
    MacroDependent,
    /// 无 --db 数据面，L3 一律不求值（§2.1 防假绿）。
    NoDb,
    /// L1 拒因可能由探测缺口导出（F2 conservative 的守卫侧对应）。
    ProbeUnavailable,
}

impl UncheckedReason {
    /// 工件/stdout 取值（snake-kebab）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FrameBound => "frame-bound",
            Self::PreludeDependent => "prelude-dependent",
            Self::MacroDependent => "macro-dependent",
            Self::NoDb => "no-db",
            Self::ProbeUnavailable => "probe-unavailable",
        }
    }
}

/// 单条守卫的终态与元数据（§6 `[[routine.guard]]` 段）。
#[derive(Debug, Clone, PartialEq)]
pub struct GuardOutcome {
    pub kind: GuardKind,
    pub scope: GuardScope,
    pub state: GuardState,
    /// 关联对象名（drift/sys-dummy 有值；其余 None）。
    pub object: Option<String>,
    /// 守卫 SQL 原文（route/top1/expr；目录面守卫 None）。
    pub sql: Option<String>,
    /// 须随行的按名绑定占位符。
    pub binds: Vec<String>,
}

/// 守卫终态：`Failed` 携带逻辑拒因（非 EngineError）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardState {
    Passed,
    Failed(crate::ir::set_oriented::SkipReason),
    Unchecked(UncheckedReason),
}

/// 一次例程的完整判定。
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionVerdict {
    pub routine: String,
    pub verdict: Verdict,
    pub confidence: Confidence,
    /// 按 entry_ok 原始次序的守卫清单（admit 全量登记，不短路）。
    pub guards: Vec<GuardOutcome>,
    /// 「结论」话术（仅数据型守卫失败时；D5，不参与契约断言）。
    pub note: Option<String>,
    /// 双臂计划（§12 D7）判别式在 admit 数据面上选中的臂；
    /// 单臂计划或无法判别（静态面/判别式非全值）恒 `None`。
    pub arm: Option<Arm>,
}

/// 单一判定入口：`admit.executor=None` → 纯静态面；`Some` → L2 重证
/// + 可求值静态守卫。`entries` 为归一化例程名（compile 项目入口）。
pub fn admit_verify(
    catalog: &RoutineCatalog,
    admit: &AdmitInput,
    mode: SetOrientedMode,
    entries: &[String],
) -> Result<Vec<AdmissionVerdict>, EngineError> {
    let ctx = OptimizeCtx {
        mode,
        batch_commit: false,
        rules: catalog.sql_rules(),
        snapshot: Some(&admit.snapshot),
        catalog,
    };
    let mut out = Vec::with_capacity(entries.len());
    for name in entries {
        let Some(routine) = catalog.lookup(name) else {
            continue;
        };
        match admit.executor.as_deref() {
            None => out.push(static_verdict(name, routine, &ctx, admit)?),
            Some(ex) => out.push(runtime_verdict(name, routine, &ctx, admit, ex)?),
        }
    }
    Ok(out)
}

/// executor=None：L1 静态候选；通过 → StaticOnly + 全 Unchecked。
fn static_verdict(
    name: &str,
    routine: &CompiledRoutine,
    ctx: &OptimizeCtx<'_>,
    admit: &AdmitInput,
) -> Result<AdmissionVerdict, EngineError> {
    let source = Source::Static(&admit.snapshot);
    match plan_from(routine, &source, ctx)? {
        Err(reject) => Ok(skipped_verdict(name, reject, Vec::new())),
        Ok(plan) => Ok(AdmissionVerdict {
            routine: name.to_string(),
            verdict: Verdict::StaticOnly,
            confidence: Confidence::Firm,
            guards: guard_outcomes(&plan),
            note: None,
            // 静态面无数据可判别式，臂标签只能由 runtime_verdict 给出。
            arm: None,
        }),
    }
}

/// executor=Some：L2 运行期重证（Source::Runtime）+ 静态槽位全量求值；
/// 帧槽位一律 Unchecked(frame-bound)。EngineError 原样上抛。
/// F1/F3 闸：守卫 SQL 命中 prelude 缓存表或宏名 → Unchecked、不求值、
/// verdict 上限 Conditional（reason 不得由这些守卫导出）。
fn runtime_verdict(
    name: &str,
    routine: &CompiledRoutine,
    ctx: &OptimizeCtx<'_>,
    admit: &AdmitInput,
    ex: &dyn SqlExecutor,
) -> Result<AdmissionVerdict, EngineError> {
    let source = Source::Runtime(ex);
    let plan = match plan_from(routine, &source, ctx)? {
        Err(reject) => return Ok(skipped_verdict(name, reject, Vec::new())),
        Ok(plan) => plan,
    };
    // D7 双臂：判别式在 admit 数据面上先行选臂（与 `call` 的 pick_arm
    // 同一 SQL、同一标签口径），守卫扫描只覆盖选中臂计划——未选中臂
    // 的守卫不进入清单，保证 ADMIT 与 SET_ORIENTED 同源（§3.2 铁律）。
    // 判别式非全值 → 该数据面下整段回退，与运行期 NotTotal 同判。
    let (eval_plan, arm) = match &plan.discriminator {
        None => (&plan, None),
        Some(d) => match pick_admit_arm(d, &plan, ex)? {
            ArmPick::Plan(p, a) => (p, Some(a)),
            ArmPick::NotTotal => {
                return Ok(skipped_verdict(
                    name,
                    PlanRejection::firm(
                        crate::ir::set_oriented::SkipReason::RouteDiscriminatorNotTotal,
                    ),
                    Vec::new(),
                ));
            }
        },
    };
    let scan = eval_plan_guards(eval_plan, ex, ctx, &admit.macro_names)?;
    let verdict = if let Some(reason) = scan.first_failed {
        Verdict::Skipped(reason)
    } else if scan.has_unchecked {
        Verdict::Conditional
    } else {
        Verdict::Converted
    };
    Ok(AdmissionVerdict {
        routine: name.to_string(),
        verdict,
        confidence: Confidence::Firm,
        guards: scan.guards,
        note: None,
        arm,
    })
}

/// admit 面的判别式选臂（`engine::pick_arm` 的无帧/无前导物化变体）：
/// `d.sql` 恰一行整数标签 → `0..arms.len()-1` 选对应臂计划、
/// `arms.len()` 选主计划（ELSE 臂）；其余一律 `NotTotal`。
enum ArmPick<'p> {
    Plan(&'p ConversionPlan, Arm),
    NotTotal,
}

fn pick_admit_arm<'p>(
    d: &'p Discriminator,
    plan: &'p ConversionPlan,
    ex: &dyn SqlExecutor,
) -> Result<ArmPick<'p>, EngineError> {
    let mut stream = ex.query(&d.sql, &crate::exec::BindMap::new())?;
    let tag = match stream.fetch_many(1).first().and_then(|r| r.first()) {
        Some(crate::value::SpinValue::Int(t)) => *t,
        _ => return Ok(ArmPick::NotTotal),
    };
    if tag == d.arms.len() as i64 {
        return Ok(ArmPick::Plan(plan, Arm::Else));
    }
    match usize::try_from(tag).ok().and_then(|i| d.arms.get(i)) {
        Some(arm) => Ok(ArmPick::Plan(arm.plan.as_ref(), Arm::Then)),
        None => Ok(ArmPick::NotTotal),
    }
}

/// 已证计划的守卫全量扫描：静态槽位先过 F1/F3 闸再求值，帧槽位恒
/// Unchecked(frame-bound)；`first_failed` 只由静态槽位导出（帧守卫
/// 在 admit 面不产 reason），次序即 `entry_ok` 原始交错次序。
struct GuardScan {
    guards: Vec<GuardOutcome>,
    first_failed: Option<crate::ir::set_oriented::SkipReason>,
    has_unchecked: bool,
}

fn eval_plan_guards(
    plan: &ConversionPlan,
    ex: &dyn SqlExecutor,
    ctx: &OptimizeCtx<'_>,
    macro_names: &BTreeSet<String>,
) -> Result<GuardScan, EngineError> {
    let preludes = prelude_names(plan);
    let frame = crate::runtime::Frame::new();
    let mut guards = Vec::new();
    let mut first_failed: Option<crate::ir::set_oriented::SkipReason> = None;
    let mut has_unchecked = false;
    for slot in guard_slots(plan) {
        let mut outcome = GuardOutcome {
            kind: slot.kind(),
            scope: slot.scope(),
            state: GuardState::Unchecked(UncheckedReason::FrameBound),
            object: slot.object(),
            sql: slot.sql().map(str::to_string),
            binds: slot.bind_names(),
        };
        if slot.scope() == GuardScope::Static {
            if let Some(reason) = guard_block_reason(slot.sql(), &preludes, macro_names) {
                outcome.state = GuardState::Unchecked(reason);
                has_unchecked = true;
            } else {
                outcome.state = match eval_guard(&slot, ex, ctx, &frame)? {
                    Ok(()) => GuardState::Passed,
                    Err(reason) => {
                        if first_failed.is_none() {
                            first_failed = Some(reason);
                        }
                        GuardState::Failed(reason)
                    }
                };
            }
        } else {
            // 帧槽位同样过 F1/F3 闸：命中 prelude/宏名时报更精确的
            // Unchecked 原因（frame-bound 之外的第二重缺面）。
            outcome.state = match guard_block_reason(slot.sql(), &preludes, macro_names) {
                Some(reason) => GuardState::Unchecked(reason),
                None => GuardState::Unchecked(UncheckedReason::FrameBound),
            };
            has_unchecked = true;
        }
        guards.push(outcome);
    }
    Ok(GuardScan {
        guards,
        first_failed,
        has_unchecked,
    })
}

/// F1：plan 的 prelude 缓存表名集（`sd_fn*`/`sd_bf*` 显式携带，
/// `sd_vw*` 从物化 SQL 提取，`sd_cur` 恒在——游标缓存表）。
fn prelude_names(plan: &ConversionPlan) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = plan.fn_preludes.iter().map(|p| p.table.clone()).collect();
    for vp in &plan.view_preludes {
        // `CREATE OR REPLACE TEMP TABLE sd_vwN AS ...`：取 TEMP TABLE 后的标识符。
        let lower = vp.sql.to_ascii_lowercase();
        if let Some(pos) = lower.find("temp table") {
            let ident: String = vp.sql[pos + "temp table".len()..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty() {
                names.insert(ident);
            }
        }
    }
    names.insert("sd_cur".to_string());
    names
}

/// 标识符边界匹配（沿用 `substitute_binds` 的 `ident_tail` 口径，§4.2 F3）：
/// 前后不为 ASCII 字母数字/下划线才算命中；整体大小写不敏感（DuckDB 标识符折叠）。
fn contains_ident(haystack: &str, needle: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let lower = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    lower.match_indices(n.as_str()).any(|(i, _)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + n.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

fn any_ident(haystack: &str, needles: &BTreeSet<String>) -> bool {
    needles.iter().any(|n| contains_ident(haystack, n))
}

/// F1/F3 闸判定：守卫 SQL 命中 prelude 缓存表 → PreludeDependent；
/// 命中已宏化名 → MacroDependent。无 SQL 的目录面守卫（sys-dummy/drift/
/// snapshot）不经过本闸——其探测不引用业务标识符。
fn guard_block_reason(
    sql: Option<&str>,
    preludes: &BTreeSet<String>,
    macro_names: &BTreeSet<String>,
) -> Option<UncheckedReason> {
    let sql = sql?;
    if any_ident(sql, preludes) {
        return Some(UncheckedReason::PreludeDependent);
    }
    if any_ident(sql, macro_names) {
        return Some(UncheckedReason::MacroDependent);
    }
    None
}

/// 守卫清单（无求值）：静态槽位按数据面标 no-db，帧槽位标 frame-bound。
fn guard_outcomes(plan: &ConversionPlan) -> Vec<GuardOutcome> {
    guard_slots(plan)
        .iter()
        .map(|slot| GuardOutcome {
            kind: slot.kind(),
            scope: slot.scope(),
            state: GuardState::Unchecked(match slot.scope() {
                GuardScope::Frame => UncheckedReason::FrameBound,
                GuardScope::Static => UncheckedReason::NoDb,
            }),
            object: slot.object(),
            sql: slot.sql().map(str::to_string),
            binds: slot.bind_names(),
        })
        .collect()
}

fn skipped_verdict(
    name: &str,
    reject: PlanRejection,
    guards: Vec<GuardOutcome>,
) -> AdmissionVerdict {
    AdmissionVerdict {
        routine: name.to_string(),
        verdict: Verdict::Skipped(reject.reason),
        confidence: if reject.probe_suspect {
            Confidence::Conservative
        } else {
            Confidence::Firm
        },
        guards,
        note: None,
        arm: None,
    }
}

#[cfg(test)]
mod tests;
