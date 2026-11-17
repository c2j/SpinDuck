//! 集合化 transactions 语义守护。

use super::*;

#[test]
fn should_not_commit_at_exit_when_batch_atomic_writes_zero_rows() {
    use spinduck_core::exec::TraceEvent;
    let sql = COPY.replace("END LOOP", "COMMIT; END LOOP");
    let commits = |trace: &Trace| {
        trace
            .0
            .iter()
            .filter(|e| matches!(e, TraceEvent::IrStmt { variant } if *variant == "TxCommit"))
            .count()
    };
    for empty in [true, false] {
        let mut baseline = engine();
        baseline.register(&sql).unwrap();
        if empty {
            baseline
                .ex
                .execute("DELETE FROM source_rows", &BindMap::new())
                .unwrap();
        }
        let mut want_trace = Trace::default();
        baseline
            .call_with_sink("copy_rows", &[], &mut want_trace)
            .unwrap();
        let mut eng = engine();
        eng.register(&sql).unwrap();
        if empty {
            eng.ex
                .execute("DELETE FROM source_rows", &BindMap::new())
                .unwrap();
        }
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        let mut trace = Trace::default();
        eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
        assert_eq!(rows(&eng), rows(&baseline), "empty={empty}");
        // 空游标：两侧都 0 次提交；非空：原逐行 3 次，折叠后恰 1 次出口提交
        assert_eq!(commits(&want_trace), if empty { 0 } else { 3 });
        assert_eq!(commits(&trace), if empty { 0 } else { 1 }, "empty={empty}");
    }
}

#[test]
fn should_skip_batch_atomic_when_batch_commit_enabled_and_avoid_empty_commit() {
    use spinduck_core::exec::TraceEvent;
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    // batch_commit=true：首期保守拒绝，走原逐行路径（4 次 FETCH），数据不变。
    let mut eng = engine();
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.batch_commit = true;
    let mut trace = Trace::default();
    eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
    assert_eq!(rows(&eng), expected());
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::BatchCommitEnabled
        }
    );
    let fetches = trace
        .0
        .iter()
        .filter(|e| matches!(e, TraceEvent::Fetch { .. }))
        .count();
    assert_eq!(fetches, 4);
    // batch_commit=false × 空游标：折叠尾提交但写入 0 行 → 出口 0 次提交。
    let sql = COPY.replace("END LOOP", "COMMIT; END LOOP");
    let commits = |trace: &Trace| {
        trace
            .0
            .iter()
            .filter(|e| matches!(e, TraceEvent::IrStmt { variant } if *variant == "TxCommit"))
            .count()
    };
    let mut eng = engine();
    eng.register(&sql).unwrap();
    eng.ex
        .execute("DELETE FROM source_rows", &BindMap::new())
        .unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.batch_commit = false;
    let mut trace = Trace::default();
    eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
    assert!(rows(&eng).is_empty());
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));
    assert_eq!(commits(&trace), 0);
}

#[test]
fn should_fallback_safely_when_sys_dummy_missing_or_wrong_cardinality() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    for setup in [
        None,
        Some("CREATE TABLE sys_dummy(dummy VARCHAR)"),
        Some("CREATE TABLE sys_dummy(dummy VARCHAR); INSERT INTO sys_dummy VALUES ('X'), ('Y')"),
    ] {
        let mut eng = engine();
        eng.ex
            .execute("DROP TABLE sys_dummy", &BindMap::new())
            .unwrap();
        if let Some(sql) = setup {
            for stmt in sql.split(';') {
                eng.ex.execute(stmt, &BindMap::new()).unwrap();
            }
        }
        eng.register(COPY).unwrap();
        eng.set_oriented = SetOrientedMode::Strict;
        eng.call("copy_rows", &[]).unwrap();
        assert_eq!(rows(&eng), expected(), "{setup:?}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnknownObjectDependency
            },
            "{setup:?}"
        );
    }
    let mut eng = engine();
    eng.ex
        .execute("DROP TABLE sys_dummy", &BindMap::new())
        .unwrap();
    eng.ex
        .execute(
            "CREATE TABLE sys_dummy(dummy VARCHAR); INSERT INTO sys_dummy VALUES ('X')",
            &BindMap::new(),
        )
        .unwrap();
    eng.register(COPY).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));
}

#[test]
fn should_preserve_rows_when_sys_dummy_is_unresolvable_or_a_fallible_view() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    for setup in [
        "DROP TABLE sys_dummy; CREATE SCHEMA elsewhere; \
         CREATE TABLE elsewhere.sys_dummy AS SELECT 1",
        "DROP TABLE sys_dummy; CREATE TABLE dummy_input(value VARCHAR); \
         INSERT INTO dummy_input VALUES ('bad'); \
         CREATE VIEW sys_dummy AS SELECT 1 FROM dummy_input WHERE CAST(value AS INTEGER) > 0",
        "CREATE TABLE dummy_input(value VARCHAR); INSERT INTO dummy_input VALUES ('bad'); \
         CREATE TEMP VIEW sys_dummy AS SELECT 1 FROM dummy_input WHERE CAST(value AS INTEGER) > 0",
    ] {
        for mode in [
            SetOrientedMode::Off,
            SetOrientedMode::Strict,
            SetOrientedMode::BatchAtomic,
        ] {
            let mut eng = engine();
            eng.ex.execute(setup, &BindMap::new()).unwrap();
            eng.register(COPY).unwrap();
            eng.set_oriented = mode;
            let result = eng.call("copy_rows", &[]).unwrap();
            assert_eq!(rows(&eng), expected(), "{mode:?}: {setup}");
            assert_eq!(result.outs, Default::default());
            assert_eq!(result.return_value, None);
            if mode != SetOrientedMode::Off {
                assert_eq!(
                    eng.set_oriented_reports()[0].status,
                    Status::Skipped {
                        reason: SkipReason::UnknownObjectDependency,
                    },
                    "{mode:?}: {setup}"
                );
            }
        }
    }
}

#[test]
fn should_convert_strict_mode_skip_trap_when_write_proven_total() {
    // 设计 §4.5 第 1 项：Strict 下受跳行 trap 保护的区域，写入可证全量
    // （纯字段拷贝、无约束、可空兼容）时 trap 为死代码，允许转换。
    // 原断言 ExceptionScopePresent 属 V2 早期保守口径，按设计修订。
    use spinduck_core::ir::set_oriented::Status;
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), expected());
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_reject_strict_mode_skip_trap_when_write_not_proven_total() {
    // 设计 §4.5 第 1 项：非全量写入下，第 2 行重复键报错经 reset_session
    // 撤销第 1 行未提交写入（Off 基线实测：call OK，仅剩 NULL 行）；
    // 过滤坏行的集合 SQL 会保留第 1 行，故必须整段退回。
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let with_index = || {
        let mut eng = engine();
        eng.ex
            .execute(
                "CREATE UNIQUE INDEX target_key ON target_rows(id)",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = with_index();
    baseline.call("copy_rows", &[]).unwrap();
    let baseline_rows = rows(&baseline);
    assert_eq!(baseline_rows, vec![vec![SpinValue::Null, SpinValue::Null]]);

    let mut eng = with_index();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), baseline_rows);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenRollbackScope
        }
    );
}

#[test]
fn should_fold_guarded_tail_commit_in_batch_atomic_with_row_and_commit_parity() {
    // 守卫块内 INSERT+COMMIT（设计 §4.5 第 1 项「分支末端单条 INSERT + 紧随 TxCommit」）：
    // 转换以写入全定义为前提——trap 永不触发，逐行提交才可折叠为条件出口提交。
    use spinduck_core::exec::TraceEvent;
    use spinduck_core::ir::set_oriented::Status;
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let commits = |trace: &Trace| {
        trace
            .0
            .iter()
            .filter(|e| matches!(e, TraceEvent::IrStmt { variant } if *variant == "TxCommit"))
            .count()
    };
    for empty in [true, false] {
        let mut baseline = engine();
        baseline.register(sql).unwrap();
        if empty {
            baseline
                .ex
                .execute("DELETE FROM source_rows", &BindMap::new())
                .unwrap();
        }
        let mut want_trace = Trace::default();
        baseline
            .call_with_sink("copy_rows", &[], &mut want_trace)
            .unwrap();
        let mut eng = engine();
        eng.register(sql).unwrap();
        if empty {
            eng.ex
                .execute("DELETE FROM source_rows", &BindMap::new())
                .unwrap();
        }
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        let mut trace = Trace::default();
        eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
        assert_eq!(rows(&eng), rows(&baseline), "empty={empty}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "empty={empty} {:?}",
            eng.set_oriented_reports()[0]
        );
        // 空游标两侧 0 次提交；非空原逐行 3 次，折叠后恰 1 次条件出口提交
        assert_eq!(commits(&want_trace), if empty { 0 } else { 3 });
        assert_eq!(commits(&trace), if empty { 0 } else { 1 }, "empty={empty}");
    }
}

#[test]
fn should_not_fold_guarded_loop_when_write_not_proven_total() {
    // 安全耦合守护：trap 跳行语义只有在写入全定义时才可折叠。目标带唯一索引
    // （重复行被原 trap 跳过）时严禁转换——未经证明的行过滤会篡改坏行语义。
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let sql = "CREATE PROCEDURE copy_rows IS BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut baseline = engine();
    baseline
        .ex
        .execute(
            "CREATE UNIQUE INDEX target_key ON target_rows(id)",
            &BindMap::new(),
        )
        .unwrap();
    baseline.register(sql).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let mut eng = engine();
    eng.ex
        .execute(
            "CREATE UNIQUE INDEX target_key ON target_rows(id)",
            &BindMap::new(),
        )
        .unwrap();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenTargetConstraints
        }
    );
    // 原语义：首个 7 写入并提交，重复 7 被 trap 跳过，NULL 行写入
    assert_eq!(
        rows(&eng),
        vec![
            vec![SpinValue::Int(7), SpinValue::Str("a".into())],
            vec![SpinValue::Null, SpinValue::Null],
        ]
    );
    assert_eq!(rows(&eng), rows(&baseline));
}
