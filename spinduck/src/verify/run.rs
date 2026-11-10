//! 链路核验（报告是数据；渲染与退出码归 CLI 壳）。
//!
//! [`ChainFacts`] 是链路事实快照：已登记例程（名 → 种类）、其 IR 程序、内核现存对象。
//! 单测用手工构造的快照断言核验逻辑（不触内核，§4.1）；运行期由
//! [`facts_from`] 从 [`Engine`] 目录快照、[`kernel_objects`] 从内核目录表读取。

use crate::catalog::RoutineKind;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor};
use crate::ir::IrProg;
use crate::project::{CaseKind, EntryConfig};
use crate::verify::chain::{chain_object_refs, routine_refs, ObjectKind, ObjectRef};
use std::collections::{BTreeMap, BTreeSet};

/// 核验项类别（`Entry` = 入口声明本身；其余为链路对象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Entry,
    Scenario,
    Routine,
    Relation,
    Sequence,
}

/// 核验结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// 无数据可核验（显式跳过，不计入失败，但必须出现在报告里）
    Skip,
}

/// 一条核验结果（对象名 + 结论 + 人读诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub kind: CheckKind,
    pub object: String,
    pub status: Status,
    pub detail: String,
}

/// 单条入口的核验报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryReport {
    pub entry: String,
    pub checks: Vec<Check>,
}

impl EntryReport {
    pub fn failed(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .collect()
    }
}

/// 链路事实快照（核验逻辑的唯一输入；可无内核构造）。
#[derive(Debug, Clone, Default)]
pub struct ChainFacts {
    /// 已登记例程：全名（小写）→ 种类
    pub registered: BTreeMap<String, CaseKind>,
    /// 已登记例程的 IR 程序（链路展开用；未登记的例程自然缺席）
    pub progs: BTreeMap<String, IrProg>,
    /// 内核现存对象（表/视图归 Relation，序列归 Sequence）
    pub objects: BTreeSet<ObjectRef>,
}

impl ChainFacts {
    /// 从引擎的例程目录取快照（对象集合由调用方提供，内核侧由 [`kernel_objects`] 读取）。
    pub fn from_engine(eng: &Engine, objects: BTreeSet<ObjectRef>) -> Self {
        let mut registered = BTreeMap::new();
        let mut progs = BTreeMap::new();
        for name in eng.catalog.routine_names() {
            if let Some(r) = eng.catalog.lookup(&name) {
                registered.insert(
                    name.clone(),
                    match r.kind {
                        RoutineKind::Procedure => CaseKind::Procedure,
                        RoutineKind::Function => CaseKind::Function,
                    },
                );
                progs.insert(name, r.prog.clone());
            }
        }
        Self {
            registered,
            progs,
            objects,
        }
    }
}

/// 核验一条入口：声明 → 登记（种类一致）→ 链路例程逐级展开。
pub fn verify_entry(entry: &EntryConfig, facts: &ChainFacts) -> EntryReport {
    let mut checks = vec![entry_check(entry, facts)];
    if checks[0].status == Status::Pass {
        walk_chain(entry, facts, &mut checks);
    }
    EntryReport {
        entry: entry.routine.clone(),
        checks,
    }
}

fn entry_check(entry: &EntryConfig, facts: &ChainFacts) -> Check {
    let mk = |status: Status, detail: String| Check {
        kind: CheckKind::Entry,
        object: entry.routine.clone(),
        status,
        detail,
    };
    let declared = match facts.registered.get(&entry.routine) {
        Some(k) => *k,
        None => {
            return mk(
                Status::Fail,
                "语料中未登记该例程（入口全名或拼写不符，或包体缺失）".to_string(),
            )
        }
    };
    if declared != entry.kind {
        return mk(
            Status::Fail,
            format!(
                "声明为 {:?}，目录登记为 {:?}（口径不一致，需改 spinduck.toml 或语料）",
                entry.kind, declared
            ),
        );
    }
    mk(Status::Pass, "入口在例程目录命中且种类一致".to_string())
}

/// 链路例程展开：广度优先，按对象去重，裸名按所属包补全（与运行期分派同口径）。
fn walk_chain(entry: &EntryConfig, facts: &ChainFacts, checks: &mut Vec<Check>) {
    let mut pending = std::collections::VecDeque::from(vec![entry.routine.clone()]);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(entry.routine.clone());
    let mut reported: BTreeSet<String> = BTreeSet::new();
    let mut refs: BTreeSet<ObjectRef> = BTreeSet::new();
    while let Some(name) = pending.pop_front() {
        let Some(prog) = facts.progs.get(&name) else {
            continue;
        };
        let pkg = name.split('.').next().unwrap_or_default().to_string();
        for r in routine_refs(prog) {
            let resolved = match resolve_routine(&r, &pkg, &facts.registered) {
                Some(full) => full,
                None => {
                    if reported.insert(r.clone()) {
                        checks.push(Check {
                            kind: CheckKind::Routine,
                            object: r.clone(),
                            status: Status::Fail,
                            detail: format!("链路断点：被 {} 调用，但语料中未登记", name),
                        });
                    }
                    continue;
                }
            };
            if !reported.insert(resolved.clone()) {
                continue;
            }
            checks.push(Check {
                kind: CheckKind::Routine,
                object: resolved.clone(),
                status: Status::Pass,
                detail: format!("由 {} 调用，已登记", name),
            });
            if visited.insert(resolved.clone()) {
                pending.push_back(resolved);
            }
        }
        refs.extend(chain_object_refs(prog));
    }
    for r in refs {
        let kind = match r.kind {
            ObjectKind::Relation => CheckKind::Relation,
            ObjectKind::Sequence => CheckKind::Sequence,
        };
        let (status, detail) = match present(&r, &facts.objects) {
            Some(hit) => (Status::Pass, format!("内核存在（{}）", hit)),
            None => (
                Status::Fail,
                "链路 SQL 引用，内核目录无此对象（缺 DDL/桩，或部署顺序不符）".to_string(),
            ),
        };
        checks.push(Check {
            kind,
            object: r.name.clone(),
            status,
            detail,
        });
    }
}

/// 对象名命中判定：全名精确，其次尾段（语料裸名 ↔ 内核 `schema.name` 同口径）。
fn present(r: &ObjectRef, objects: &BTreeSet<ObjectRef>) -> Option<String> {
    let tail = r.name.rsplit('.').next().unwrap_or(r.name.as_str());
    objects
        .iter()
        .find(|o| o.kind == r.kind && o.name == r.name)
        .or_else(|| {
            objects.iter().find(|o| {
                o.kind == r.kind && o.name.rsplit('.').next().unwrap_or(o.name.as_str()) == tail
            })
        })
        .map(|o| o.name.clone())
}

/// 引用名 → 目录全名：全名直接命中；裸名先用调用方所属包补全，再试裸名本身。
fn resolve_routine(r: &str, pkg: &str, registered: &BTreeMap<String, CaseKind>) -> Option<String> {
    if r.contains('.') {
        return if registered.contains_key(r) {
            Some(r.to_string())
        } else {
            None
        };
    }
    let qualified = format!("{}.{}", pkg, r);
    if registered.contains_key(&qualified) {
        return Some(qualified);
    }
    if registered.contains_key(r) {
        return Some(r.to_string());
    }
    None
}

/// 场景核验：对声明了本入口的用例，逐场景实际调用（参数按 SQL 字面量口径）。
///
/// 无匹配用例 → `Skip`（对象名 `-`）：入口链路的对象核验仍然有效，可执行性没有被本命令
/// 证明，报告必须显式区分、不得伪装成通过；若同一项目的 `[[case.assert]]` 已指向该入口，
/// 理由要改说"断言层已覆盖、本命令不执行断言"，不能再写成"未验证"。
pub fn verify_scenarios(
    eng: &mut Engine,
    cases: &[crate::project::CaseConfig],
    reports: &mut [EntryReport],
) {
    for report in reports.iter_mut() {
        // 入口本身未命中：报告只保留那一条诊断，不再追加"场景跳过"噪音
        if report
            .checks
            .iter()
            .any(|c| c.kind == CheckKind::Entry && c.status == Status::Fail)
        {
            continue;
        }
        let matched: Vec<&crate::project::CaseConfig> = cases
            .iter()
            .filter(|c| {
                c.entry.as_deref() == Some(report.entry.as_str())
                    || c.routine.eq_ignore_ascii_case(&report.entry)
            })
            .collect();
        if matched.is_empty() {
            report
                .checks
                .push(scenario_skip_check(cases, &report.entry));
            continue;
        }
        for case in matched {
            for (i, sc) in case.scenarios.iter().enumerate() {
                let args: Vec<crate::value::SpinValue> =
                    sc.args.iter().map(|a| sql_literal_value(a)).collect();
                let (status, detail) = match eng.call(&report.entry, &args) {
                    Ok(res) => (
                        Status::Pass,
                        format!(
                            "调用成功：outs={} return={}",
                            res.outs.len(),
                            res.return_value
                                .as_ref()
                                .map(|v| v.display().to_string())
                                .unwrap_or_else(|| "-".to_string())
                        ),
                    ),
                    Err(e) => (Status::Fail, format!("[{}] {}", e.code, e.message)),
                };
                report.checks.push(Check {
                    kind: CheckKind::Scenario,
                    object: format!("{}#{}", case.name, i + 1),
                    status,
                    detail,
                });
            }
        }
    }
}

/// Skip 理由的两种口径：没有任何用例碰过该入口 = 可执行性未验证；断言集指向该入口 =
/// 覆盖已存在，只是 `verify` 不执行断言（由 `--features native --test cases_native` 跑）。
fn scenario_skip_check(cases: &[crate::project::CaseConfig], entry: &str) -> Check {
    let hit: Vec<&str> = cases
        .iter()
        .filter(|c| c.asserts.iter().any(|a| a.targets_routine(entry)))
        .map(|c| c.name.as_str())
        .collect();
    let count: usize = cases
        .iter()
        .flat_map(|c| c.asserts.iter())
        .filter(|a| a.targets_routine(entry))
        .count();
    let detail = if hit.is_empty() {
        "无 *.case.toml 声明该入口，可执行性未验证".to_string()
    } else {
        format!(
            "无场景声明该入口；{count} 条 [[case.assert]] 已由用例 {} 覆盖该例程，\
             本命令不执行断言（断言层见 cases_native）",
            hit.join("、")
        )
    };
    Check {
        kind: CheckKind::Scenario,
        object: "-".to_string(),
        status: Status::Skip,
        detail,
    }
}

/// SQL 字面量 → 值（与黄金 harness 同口径：`null`→NULL，`'串'`→Str，其余按 Str）。
fn sql_literal_value(lit: &str) -> crate::value::SpinValue {
    use crate::value::SpinValue;
    let t = lit.trim();
    if t.eq_ignore_ascii_case("null") {
        return SpinValue::Null;
    }
    if let Some(inner) = t.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return SpinValue::Str(inner.replace("''", "'"));
    }
    SpinValue::Str(t.to_string())
}

/// 内核现存链路对象（表 + 视图归 Relation，序列归 Sequence）。
///
/// 只走 [`SqlExecutor`] 接缝（§1）；`duckdb_*` 目录函数名属内核侧固定契约。
pub fn kernel_objects(ex: &dyn SqlExecutor) -> Result<BTreeSet<ObjectRef>, EngineError> {
    let mut out = BTreeSet::new();
    for (sql, kind) in [
        (
            "SELECT schema_name || '.' || table_name FROM duckdb_tables()",
            ObjectKind::Relation,
        ),
        (
            "SELECT schema_name || '.' || view_name FROM duckdb_views() WHERE internal = false",
            ObjectKind::Relation,
        ),
        (
            "SELECT schema_name || '.' || sequence_name FROM duckdb_sequences()",
            ObjectKind::Sequence,
        ),
    ] {
        let mut s = ex.query(sql, &BindMap::new())?;
        for row in s.fetch_many(100_000) {
            if let Some(v) = row.first() {
                // 内核按书写保留大小写，链路名归一小写：不归一则大写声明的对象恒判缺件。
                let name = v.as_str_loose().to_ascii_lowercase();
                if !name.is_empty() {
                    out.insert(ObjectRef { kind, name });
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrProg, IrStmt};

    fn entry(name: &str, kind: CaseKind) -> EntryConfig {
        EntryConfig {
            routine: name.into(),
            kind,
        }
    }

    #[test]
    fn should_lowercase_kernel_catalog_names() {
        // 内核按书写保留标识符大小写（DuckDB 的匹配不区分），链路名一律归一小写。
        // 目录侧不归一，`CREATE … VIEW V_DAT_INST_BASE_INFO` 这类大写声明在核验里恒判缺件
        // （考题语料实测）。
        use crate::exec::FakeExecutor;
        use crate::value::SpinValue;
        let ex = FakeExecutor::with_rows(
            vec!["name".into()],
            vec![vec![SpinValue::Str("main.V_X".into())]],
        );
        let objects = kernel_objects(&ex).unwrap();
        assert!(
            objects
                .iter()
                .any(|o| o.kind == ObjectKind::Relation && o.name == "main.v_x"),
            "内核目录名要归一小写: {objects:?}"
        );
    }

    #[test]
    fn should_fail_when_declared_entry_is_not_registered() {
        let facts = ChainFacts::default();
        let r = verify_entry(&entry("pkg_a.prc_x", CaseKind::Procedure), &facts);
        assert_eq!(r.checks.len(), 1, "入口未命中时不再展开链路");
        assert_eq!(r.checks[0].kind, CheckKind::Entry);
        assert_eq!(r.checks[0].status, Status::Fail);
        assert!(
            r.checks[0].detail.contains("未登记"),
            "诊断需说明原因：{}",
            r.checks[0].detail
        );
        assert_eq!(r.failed().len(), 1);
    }

    #[test]
    fn should_fail_when_entry_kind_differs_from_registration() {
        let mut facts = ChainFacts::default();
        facts
            .registered
            .insert("pkg_a.fn_x".into(), CaseKind::Function);
        let r = verify_entry(&entry("pkg_a.fn_x", CaseKind::Procedure), &facts);
        assert_eq!(r.checks[0].status, Status::Fail);
        assert!(
            r.checks[0].detail.contains("Function"),
            "诊断需给出目录实际种类：{}",
            r.checks[0].detail
        );
    }

    #[test]
    fn should_pass_entry_when_name_and_kind_match() {
        let mut facts = ChainFacts::default();
        facts
            .registered
            .insert("pkg_a.prc_x".into(), CaseKind::Procedure);
        let r = verify_entry(&entry("pkg_a.prc_x", CaseKind::Procedure), &facts);
        assert_eq!(r.checks[0].status, Status::Pass);
        assert_eq!(r.failed().len(), 0);
    }

    fn prog_of(calls: &[&str]) -> IrProg {
        IrProg {
            code: calls
                .iter()
                .map(|c| IrStmt::CallRoutine {
                    name: c.to_string(),
                    args: vec![],
                    out_targets: vec![],
                })
                .collect(),
        }
    }

    fn facts(routines: &[(&str, IrProg)]) -> ChainFacts {
        let mut f = ChainFacts::default();
        for (name, prog) in routines {
            f.registered
                .insert((*name).to_string(), CaseKind::Procedure);
            f.progs.insert((*name).to_string(), prog.clone());
        }
        f
    }

    fn routine_checks(r: &EntryReport) -> Vec<(String, Status)> {
        r.checks
            .iter()
            .filter(|c| c.kind == CheckKind::Routine)
            .map(|c| (c.object.clone(), c.status))
            .collect()
    }

    #[test]
    fn should_report_unregistered_routine_as_chain_gap() {
        let f = facts(&[
            (
                "pkg_a.prc_main",
                prog_of(&["pkg_a.prc_b", "pkg_x.prc_gone"]),
            ),
            ("pkg_a.prc_b", prog_of(&[])),
        ]);
        let r = verify_entry(&entry("pkg_a.prc_main", CaseKind::Procedure), &f);
        assert_eq!(
            routine_checks(&r),
            vec![
                ("pkg_a.prc_b".into(), Status::Pass),
                ("pkg_x.prc_gone".into(), Status::Fail)
            ],
            "链路例程按发现序逐条核验，未登记的显式判失败"
        );
        assert_eq!(r.failed().len(), 1);
        let gap = &r
            .checks
            .iter()
            .find(|c| c.object == "pkg_x.prc_gone")
            .unwrap();
        assert!(
            gap.detail.contains("未登记"),
            "诊断需给出缺口原因：{}",
            gap.detail
        );
    }

    #[test]
    fn should_resolve_bare_call_in_same_package_and_walk_transitively() {
        // pkg_a.prc_main →（裸名同包）pkg_a.prc_helper；→ pkg_a.prc_b → pkg_c.fn_d
        let f = facts(&[
            ("pkg_a.prc_main", prog_of(&["prc_helper", "pkg_a.prc_b"])),
            ("pkg_a.prc_helper", prog_of(&[])),
            ("pkg_a.prc_b", prog_of(&["pkg_c.fn_d"])),
            ("pkg_c.fn_d", prog_of(&[])),
        ]);
        let r = verify_entry(&entry("pkg_a.prc_main", CaseKind::Procedure), &f);
        let got: Vec<String> = routine_checks(&r).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            got,
            vec!["pkg_a.prc_b", "pkg_a.prc_helper", "pkg_c.fn_d"],
            "裸名按所属包补全后核验；链路可传递展开"
        );
        assert_eq!(r.failed().len(), 0);
    }

    fn prog_sql(sqls: &[&str]) -> IrProg {
        IrProg {
            code: sqls
                .iter()
                .map(|sql| IrStmt::SqlExec {
                    sql: sql.to_string(),
                    binds: vec![],
                    fn_sites: vec![],
                })
                .collect(),
        }
    }

    fn objects(items: &[(ObjectKind, &str)]) -> BTreeSet<ObjectRef> {
        items
            .iter()
            .map(|(k, n)| ObjectRef {
                kind: *k,
                name: n.to_string(),
            })
            .collect()
    }

    fn object_checks(r: &EntryReport) -> Vec<(CheckKind, String, Status)> {
        r.checks
            .iter()
            .filter(|c| matches!(c.kind, CheckKind::Relation | CheckKind::Sequence))
            .map(|c| (c.kind, c.object.clone(), c.status))
            .collect()
    }

    #[test]
    fn should_report_missing_relation_and_sequence_in_chain() {
        let mut f = facts(&[("pkg_a.prc_main", prog_of(&[]))]);
        f.progs.insert(
            "pkg_a.prc_main".into(),
            prog_sql(&[
                "DELETE FROM T_GONE",
                "INSERT INTO T_OK (A) VALUES (SEQ_X.NEXTVAL)",
            ]),
        );
        f.objects = objects(&[
            (ObjectKind::Relation, "t_ok"),
            (ObjectKind::Sequence, "seq_x"),
        ]);
        let r = verify_entry(&entry("pkg_a.prc_main", CaseKind::Procedure), &f);
        assert_eq!(
            object_checks(&r),
            vec![
                (CheckKind::Relation, "t_gone".into(), Status::Fail),
                (CheckKind::Relation, "t_ok".into(), Status::Pass),
                (CheckKind::Sequence, "seq_x".into(), Status::Pass),
            ],
            "链路 SQL 引用的表/序列逐一核验，缺席者显式判失败"
        );
    }

    #[test]
    fn should_match_bare_reference_against_schema_qualified_kernel_object() {
        // 语料写裸名，内核目录返回 schema.name：按尾段匹配
        let mut f = facts(&[("pkg_a.prc_main", prog_sql(&["SELECT * FROM T_OK"]))]);
        f.objects = objects(&[(ObjectKind::Relation, "gaussdb.t_ok")]);
        let r = verify_entry(&entry("pkg_a.prc_main", CaseKind::Procedure), &f);
        assert_eq!(
            object_checks(&r)[0],
            (CheckKind::Relation, "t_ok".into(), Status::Pass)
        );
    }

    #[test]
    fn should_report_each_chain_object_once_across_routines() {
        let mut f = facts(&[
            ("pkg_a.prc_main", prog_of(&["pkg_a.prc_b"])),
            ("pkg_a.prc_b", prog_sql(&["DELETE FROM T_GONE"])),
        ]);
        f.progs
            .insert("pkg_a.prc_main".into(), prog_sql(&["DELETE FROM T_GONE"]));
        let r = verify_entry(&entry("pkg_a.prc_main", CaseKind::Procedure), &f);
        let n = object_checks(&r)
            .into_iter()
            .filter(|(_, name, _)| name == "t_gone")
            .count();
        assert_eq!(n, 1, "同一对象只核验一次（链路多例程共享）");
    }
}
