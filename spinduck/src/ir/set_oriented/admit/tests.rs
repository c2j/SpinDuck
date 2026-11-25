//! `compile --admit` 判定单元测试（设计 v3.1 §4.2/§6）：
//! 静态/运行期两档 verdict、F1/F3 闸、守卫清单次序与工件字段。

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

/// caller_view_lookup 例程（与 tests/set_oriented_dummy.rs 同源）：
/// UNION ALL 视图查找 → view_preludes → preludes_first。
const VIEW_LOOKUP: &str = "CREATE OR REPLACE PROCEDURE caller_view_lookup(\
        p_o_succeed OUT VARCHAR2) IS \
        v_x VARCHAR2(20); \
        BEGIN \
          p_o_succeed := '0'; \
          FOR r IN SELECT security_id, cjje, tag FROM cur_rows LOOP \
            BEGIN \
              SELECT v.val INTO v_x FROM v_dim v WHERE v.k = r.security_id; \
            EXCEPTION WHEN OTHERS THEN v_x := 'D'; END; \
            INSERT INTO dst_rows(security_id, yj, tag) \
              VALUES (r.security_id, r.cjje, v_x); \
          END LOOP; \
        END;";

fn tcol(name: &str, ty: &str, canonical: SpinType) -> ColumnMetadata {
    ColumnMetadata {
        name: name.into(),
        source_type: ty.into(),
        canonical_type: Some(canonical),
        nullable: true,
        default_expr_raw: None,
    }
}

fn tmeta(name: &str, cols: Vec<ColumnMetadata>) -> TableMetadata {
    TableMetadata {
        name: vec![name.to_string()],
        columns: cols,
        constraints: Vec::new(),
        indexes: Vec::new(),
        indexes_complete: true,
        has_opaque_ddl: false,
    }
}

#[cfg(feature = "native")]
#[test]
fn should_mark_view_prelude_dependent_guards_unchecked() {
    // 5/6（F1 端到端）：视图查找例程 → preludes_first 计划；守卫 SQL
    // 命中 sd_vwN → Unchecked(prelude-dependent)，verdict 上限 Conditional，
    // reason 不得由被拦守卫导出。
    use crate::types::SpinType::*;
    let c = |n: &str, ty: &str, ct: SpinType| tcol(n, ty, ct);
    let d152 = || Decimal {
        precision: 15,
        scale: 2,
    };
    let d206 = || Decimal {
        precision: 20,
        scale: 6,
    };
    let vc = || Varchar;
    let tables = vec![
        tmeta(
            "cur_rows",
            vec![
                c("security_id", "VARCHAR", vc()),
                c("cjje", "DECIMAL(15,2)", d152()),
                c("yj11", "DECIMAL(15,2)", d152()),
                c("yj12", "DECIMAL(15,2)", d152()),
                c("tag", "VARCHAR", vc()),
            ],
        ),
        tmeta(
            "dst_rows",
            vec![
                c("security_id", "VARCHAR", vc()),
                c("yj", "DECIMAL(20,6)", d206()),
                c("tag", "VARCHAR", vc()),
            ],
        ),
        tmeta(
            "dim_a",
            vec![c("k", "VARCHAR", vc()), c("val", "VARCHAR", vc())],
        ),
        tmeta(
            "dim_b",
            vec![c("k", "VARCHAR", vc()), c("val", "VARCHAR", vc())],
        ),
        tmeta(
            "v_dim",
            vec![c("k", "VARCHAR", vc()), c("val", "VARCHAR", vc())],
        ),
    ];
    let ex = crate::DuckExecutor::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(15,2), \
               yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR)",
        "CREATE TABLE dst_rows(security_id VARCHAR, yj DECIMAL(20,6), tag VARCHAR)",
        "CREATE TABLE dim_a(k VARCHAR, val VARCHAR)",
        "CREATE TABLE dim_b(k VARCHAR, val VARCHAR)",
        "CREATE VIEW v_dim AS SELECT k, val FROM dim_a \
               UNION ALL SELECT k, val FROM dim_b",
        "CREATE TABLE sys_dummy AS SELECT 1 AS x",
        "INSERT INTO dim_a VALUES ('K1','va1'),('K2','va2')",
        "INSERT INTO dim_b VALUES ('K3','vb3'),('K1','vb1')",
        "INSERT INTO cur_rows VALUES \
               ('K1',1.00,0,0,'a'),('K2',2.00,0,0,'b'),\
               ('K3',3.00,0,0,'c'),('K4',4.00,0,0,'d')",
    ] {
        ex.execute(sql, &BindMap::new()).unwrap();
    }
    let catalog = compile_sources(&[VIEW_LOOKUP], RuleRegistry::default())
        .unwrap()
        .catalog;
    let admit = AdmitInput {
        snapshot: SchemaSnapshot {
            tables,
            diagnostics: Vec::new(),
            fingerprint: "t".into(),
        },
        executor: Some(Box::new(ex)),
        macro_names: BTreeSet::new(),
    };
    for mode in [SetOrientedMode::Strict, SetOrientedMode::BatchAtomic] {
        let verdicts =
            admit_verify(&catalog, &admit, mode, &["caller_view_lookup".into()]).unwrap();
        assert_eq!(verdicts.len(), 1, "{mode:?}");
        let v = &verdicts[0];
        assert_eq!(v.verdict, Verdict::Conditional, "{mode:?}: {v:?}");
        assert_eq!(v.confidence, Confidence::Firm, "{mode:?}");
        // preludes_first 计划：引用 sd_vwN/sd_cur 的守卫标
        // Unchecked(prelude-dependent)；其余帧守卫 frame-bound；
        // 静态面全 Passed；无 Failed → reason 不由缺面守卫导出。
        let prelude = v
            .guards
            .iter()
            .find(|g| {
                matches!(
                    g.state,
                    GuardState::Unchecked(UncheckedReason::PreludeDependent)
                ) && g.sql.as_deref().is_some_and(|s| s.contains("sd_vw"))
            })
            .unwrap_or_else(|| panic!("{mode:?} 缺 prelude-dependent 守卫: {v:?}"));
        assert!(prelude.sql.as_deref().unwrap().contains("sd_cur"));
        assert!(
            v.guards
                .iter()
                .all(|g| !matches!(g.state, GuardState::Failed(_))),
            "{mode:?}: {v:?}"
        );
    }
}

#[cfg(feature = "native")]
#[test]
fn should_mark_macro_dependent_static_guard_unchecked() {
    // 6（F3 端到端）：静态守卫 SQL 命中 macro_names →
    // Unchecked(macro-dependent)，不下发求值，verdict 不导出其 reason。
    use super::super::schema::Column;
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
    plan.route_guards = vec!["SELECT my_macro(id) FROM t".into()];
    let catalog = catalog();
    let ctx = OptimizeCtx {
        mode: SetOrientedMode::Strict,
        batch_commit: false,
        rules: catalog.sql_rules(),
        snapshot: None,
        catalog: &catalog,
    };
    let macros: BTreeSet<String> = ["my_macro".to_string()].into_iter().collect();
    let scan = eval_plan_guards(&plan, &ex, &ctx, &macros).unwrap();
    let route = scan.guards.last().unwrap();
    assert_eq!(route.kind, GuardKind::Route);
    assert!(matches!(
        route.state,
        GuardState::Unchecked(UncheckedReason::MacroDependent)
    ));
    assert!(scan.first_failed.is_none());
    assert!(scan.has_unchecked);
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
        exc_guards: Vec::new(),
        hybrid: None,
        seg_sql: None,
        discriminator: None,
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
