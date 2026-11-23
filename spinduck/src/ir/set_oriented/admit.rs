//! `compile --admit` 判定（设计 v3.1 §4.2）：L1 静态候选 + 入口守卫清单。
//!
//! `executor=None` 只出 `StaticOnly`/`conservative`，不碰内核；
//! `executor=Some` 走 L2 运行期重证并全量求值可求值的静态守卫
//! （F1 prelude 闸 / F3 宏闸在 PR-3 接入；EngineError 一律上抛，
//! 绝不降级为判定）。admit 不发 TraceSink 事件、不物化 preludes、
//! 不执行例程体——守卫求值面与 `call` 的 `entry_ok` 共用 GuardSlot
//! 原始次序，但 admit 全量求值而非短路。

use super::entry::{eval_guard, guard_slots, ConversionPlan};
use super::metadata::SchemaSnapshot;
use super::schema::Source;
use super::{plan_from, GuardKind, GuardScope, OptimizeCtx, PlanRejection, SetOrientedMode};
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
    let scan = eval_plan_guards(&plan, ex, ctx, &admit.macro_names)?;
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
    })
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
    }
}

#[cfg(test)]
mod tests {
    use super::super::metadata::{ColumnMetadata, TableMetadata};
    use super::*;
    use crate::catalog::compile_sources;
    use crate::exec::{BindMap, ExecOutcome, FakeExecutor, RowStream};
    use crate::rules::RuleRegistry;
    use crate::types::SpinType;
    use std::cell::RefCell;
    use std::rc::Rc;

    const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id FROM source_rows LOOP \
        INSERT INTO target_rows(id) VALUES (r.id); END LOOP; END;";

    fn int_table(name: &str) -> TableMetadata {
        TableMetadata {
            name: vec![name.to_string()],
            columns: vec![ColumnMetadata {
                name: "id".into(),
                source_type: "BIGINT".into(),
                canonical_type: Some(SpinType::BigInt),
                nullable: true,
                default_expr_raw: None,
            }],
            constraints: Vec::new(),
            indexes: Vec::new(),
            indexes_complete: true,
            has_opaque_ddl: false,
        }
    }

    fn snapshot(with_tables: bool) -> SchemaSnapshot {
        SchemaSnapshot {
            tables: if with_tables {
                vec![int_table("source_rows"), int_table("target_rows")]
            } else {
                Vec::new()
            },
            diagnostics: Vec::new(),
            fingerprint: "t".into(),
        }
    }

    fn input(with_tables: bool) -> AdmitInput {
        AdmitInput {
            snapshot: snapshot(with_tables),
            executor: None,
            macro_names: BTreeSet::new(),
        }
    }

    fn catalog() -> RoutineCatalog {
        compile_sources(&[COPY], RuleRegistry::default())
            .unwrap()
            .catalog
    }

    #[test]
    fn should_leave_all_l3_guards_unchecked_without_database() {
        // 2b+3：字面量 AdmitInput（无 codeweb 项目）+ 无数据面 →
        // L1 通过时 StaticOnly，守卫清单全 Unchecked、绝不出现 Passed。
        let catalog = catalog();
        let admit = input(true);
        let verdicts = admit_verify(
            &catalog,
            &admit,
            SetOrientedMode::Strict,
            &["copy_rows".into()],
        )
        .unwrap();
        assert_eq!(verdicts.len(), 1);
        let v = &verdicts[0];
        assert_eq!(v.verdict, Verdict::StaticOnly);
        assert_eq!(v.confidence, Confidence::Firm);
        assert!(!v.guards.is_empty());
        assert!(
            v.guards
                .iter()
                .all(|g| matches!(g.state, GuardState::Unchecked(_))),
            "无数据面不得出现 Passed: {:?}",
            v.guards
        );
        // 槽位次序 = entry_ok 原始次序：sys-dummy → drift×2 → snapshot。
        let kinds: Vec<GuardKind> = v.guards.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GuardKind::SysDummy,
                GuardKind::Drift,
                GuardKind::Drift,
                GuardKind::Snapshot,
            ]
        );
    }

    #[test]
    fn should_not_report_firm_verdict_for_probe_dependent_rejection() {
        // 4c：空快照下 generate 阶段拒因可能由探测缺口导出 →
        // executor=None 只允许 conservative，绝不 firm skipped。
        let catalog = catalog();
        let admit = input(false);
        let verdicts = admit_verify(
            &catalog,
            &admit,
            SetOrientedMode::Strict,
            &["copy_rows".into()],
        )
        .unwrap();
        let v = &verdicts[0];
        assert!(
            !matches!(v.verdict, Verdict::Skipped(_)) || v.confidence != Confidence::Firm,
            "无数据面不得 firm skipped: {v:?}"
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_allow_firm_verdict_after_runtime_reverify() {
        // 4c 对照面：executor=Some 走 L2 运行期重证，拒因即可 firm。
        // 内存库：sys_dummy 恰 1 行 + 两表；空快照 → snapshot 守卫实测失败。
        let ex = crate::DuckExecutor::open_in_memory().unwrap();
        for sql in [
            "CREATE TABLE sys_dummy AS SELECT 1",
            "CREATE TABLE source_rows (id BIGINT)",
            "CREATE TABLE target_rows (id BIGINT)",
        ] {
            ex.execute(sql, &crate::exec::BindMap::new()).unwrap();
        }
        let catalog = catalog();
        let admit = AdmitInput {
            snapshot: snapshot(false),
            executor: Some(Box::new(ex)),
            macro_names: BTreeSet::new(),
        };
        let verdicts = admit_verify(
            &catalog,
            &admit,
            SetOrientedMode::Strict,
            &["copy_rows".into()],
        )
        .unwrap();
        assert_eq!(verdicts[0].confidence, Confidence::Firm);
    }

    #[test]
    fn should_mark_prelude_dependent_guard_sql_unchecked() {
        // 4d（F1）：守卫 SQL 引用 sd_vw*/sd_fn*/sd_bf*/sd_cur → PreludeDependent，
        // 不参与求值；prelude_names 从 fn_preludes 表名 + view 物化 SQL 提取。
        let mut plan = entry_plan();
        plan.route_guards = vec!["SELECT * FROM sd_vw0".into()];
        plan.view_preludes = vec![super::super::entry::ViewPrelude {
            sql: "CREATE OR REPLACE TEMP TABLE sd_vw0 AS SELECT 1".into(),
        }];
        let preludes = prelude_names(&plan);
        assert!(preludes.contains("sd_vw0"), "{preludes:?}");
        assert!(preludes.contains("sd_cur"));
        let reason = guard_block_reason(
            Some("SELECT count(*) FROM sd_vw0"),
            &preludes,
            &BTreeSet::new(),
        );
        assert_eq!(reason, Some(UncheckedReason::PreludeDependent));
        // sd_fnN/sd_bfN 走 fn_preludes 显式表名。
        plan.fn_preludes = vec![super::super::entry::FnPrelude {
            table: "sd_fn3".into(),
            name: "f".into(),
            distinct_sql: String::new(),
            binds: Vec::new(),
            arity: 0,
            needs_sd_cur: false,
        }];
        let preludes = prelude_names(&plan);
        assert!(preludes.contains("sd_fn3"));
        assert_eq!(
            guard_block_reason(Some("SELECT 1 FROM sd_fn3"), &preludes, &BTreeSet::new()),
            Some(UncheckedReason::PreludeDependent)
        );
        // 干净 SQL 不命中。
        assert_eq!(
            guard_block_reason(Some("SELECT 1 FROM t"), &preludes, &BTreeSet::new()),
            None
        );
        // 无 SQL 的目录面守卫不过闸。
        assert_eq!(guard_block_reason(None, &preludes, &BTreeSet::new()), None);
    }

    #[test]
    fn should_mark_macro_dependent_guard_sql_unchecked() {
        // 4e（F3）：守卫 SQL 命中 macro_names（标识符边界、大小写不敏感）
        // → MacroDependent；同名列/前后缀不误伤。
        let preludes = BTreeSet::new();
        let macros: BTreeSet<String> = ["fnc_nv".to_string()].into_iter().collect();
        assert_eq!(
            guard_block_reason(Some("SELECT fnc_nv(a) FROM t"), &preludes, &macros),
            Some(UncheckedReason::MacroDependent)
        );
        assert_eq!(
            guard_block_reason(Some("SELECT FNC_NV(a) FROM t"), &preludes, &macros),
            Some(UncheckedReason::MacroDependent)
        );
        // 边界：fnc_nv2 / xfnc_nv / 'fnc_nv' 字面量内不命中（保守方向只允许少判）。
        assert_eq!(
            guard_block_reason(Some("SELECT fnc_nv2(a) FROM t"), &preludes, &macros),
            None
        );
        assert_eq!(
            guard_block_reason(Some("SELECT xfnc_nv(a) FROM t"), &preludes, &macros),
            None
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_report_static_reason_when_frame_guard_precedes_it() {
        // 4b：top1（帧）排在 route（静态）之前；admit 下帧守卫恒
        // Unchecked(frame-bound) 不产 reason，verdict reason 只由首个
        // 失败的静态守卫导出——route 失败 → UnsupportedControlFlow。
        use super::super::schema::Column;
        use crate::ir::set_oriented::SkipReason;
        let ex = crate::DuckExecutor::open_in_memory().unwrap();
        for sql in [
            "CREATE TABLE sys_dummy AS SELECT 1",
            "CREATE TABLE s (id BIGINT)",
            "CREATE TABLE t (id BIGINT)",
        ] {
            ex.execute(sql, &BindMap::new()).unwrap();
        }
        let col = || Column {
            name: super::super::shape::Name("id".into()),
            kind: "bigint".into(),
            nullable: true,
            default: None,
        };
        let mut plan = entry_plan();
        plan.source_table.columns = vec![col()];
        plan.target_table.columns = vec![col()];
        plan.top1_guards = vec![("SELECT 0".into(), Vec::new())];
        plan.route_guards = vec!["SELECT 1".into()]; // Int(1) ≠ Bool(true) → route 失败
        let catalog = catalog();
        let ctx = OptimizeCtx {
            mode: SetOrientedMode::Strict,
            batch_commit: false,
            rules: catalog.sql_rules(),
            snapshot: None, // 无快照 → snapshot 守卫实测通过
            catalog: &catalog,
        };
        let scan = eval_plan_guards(&plan, &ex, &ctx, &BTreeSet::new()).unwrap();
        assert_eq!(scan.first_failed, Some(SkipReason::UnsupportedControlFlow));
        let kinds: Vec<GuardKind> = scan.guards.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GuardKind::SysDummy,
                GuardKind::Drift,
                GuardKind::Drift,
                GuardKind::Snapshot,
                GuardKind::Top1Tie,
                GuardKind::Route,
            ]
        );
        assert!(matches!(
            scan.guards[4].state,
            GuardState::Unchecked(UncheckedReason::FrameBound)
        ));
        assert!(matches!(
            scan.guards[5].state,
            GuardState::Failed(SkipReason::UnsupportedControlFlow)
        ));
    }

    /// 最小 ConversionPlan（仅供 prelude_names/guard_block_reason 测试）。
    fn entry_plan() -> ConversionPlan {
        ConversionPlan {
            sql: String::new(),
            binds: Vec::new(),
            fn_preludes: Vec::new(),
            view_preludes: Vec::new(),
            preludes_first: false,
            tail_commit: false,
            open_pc: 0,
            close_pc: 0,
            source: super::super::shape::Name("s".into()),
            target: super::super::shape::Name("t".into()),
            source_table: super::super::schema::Table {
                qualified: super::super::shape::Name("s".into()).sql(),
                columns: Vec::new(),
            },
            target_table: super::super::schema::Table {
                qualified: super::super::shape::Name("t".into()).sql(),
                columns: Vec::new(),
            },
            lookup_tables: Vec::new(),
            top1_guards: Vec::new(),
            route_guards: Vec::new(),
            expr_guards: Vec::new(),
            miss_outs: Vec::new(),
        }
    }

    /// 记录下发 SQL 的执行器：admission 只允许守卫/目录 SELECT。
    struct Rec {
        inner: FakeExecutor,
        seen: Rc<RefCell<Vec<String>>>,
    }

    impl SqlExecutor for Rec {
        fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
            self.seen.borrow_mut().push(sql.to_string());
            self.inner.query(sql, binds)
        }
        fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
            self.seen.borrow_mut().push(sql.to_string());
            self.inner.execute(sql, binds)
        }
    }

    #[test]
    fn should_not_execute_routine_body_during_admission() {
        // 4：FakeExecutor 空脚本 → sys_dummy 探测即拒；admit 只发
        // 目录探测 SELECT，绝不发例程体业务 SQL（INSERT/CALL/UPDATE）。
        let seen = Rc::new(RefCell::new(Vec::new()));
        let ex = Rec {
            inner: FakeExecutor::default(),
            seen: seen.clone(),
        };
        let catalog = catalog();
        let admit = AdmitInput {
            snapshot: snapshot(true),
            executor: Some(Box::new(ex)),
            macro_names: BTreeSet::new(),
        };
        let verdicts = admit_verify(
            &catalog,
            &admit,
            SetOrientedMode::Strict,
            &["copy_rows".into()],
        )
        .unwrap();
        assert_eq!(verdicts.len(), 1);
        let sqls = seen.borrow();
        assert!(!sqls.is_empty());
        assert!(
            sqls.iter().all(|s| {
                let t = s.trim_start().to_ascii_uppercase();
                t.starts_with("SELECT") && !t.contains("INSERT") && !t.contains("CALL")
            }),
            "admission 只允许守卫/目录 SELECT: {sqls:?}"
        );
    }
}
