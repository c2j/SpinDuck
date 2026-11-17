//! §4.5-2 区域入口运行状态机守护：前缀先执行、入口守卫、失败不重放前缀。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        "CREATE TABLE target_rows(val BIGINT)",
        "INSERT INTO source_rows VALUES (1, 2), (3, 4)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val FROM target_rows ORDER BY rowid",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

fn dummy_count(eng: &Engine) -> i64 {
    match eng
        .ex
        .query("SELECT COUNT(*) FROM sys_dummy", &BindMap::new())
        .unwrap()
        .fetch_many(1)
        .first()
        .and_then(|r| r.first())
    {
        Some(SpinValue::Int(n)) => *n,
        other => panic!("unexpected count: {other:?}"),
    }
}

#[test]
fn should_not_abort_prefix_writes_when_pre_region_guard_falls_back() {
    // 前缀自身把 sys_dummy 变成 2 行（已提交）：入口守卫必须在区域入口
    // （前缀执行完毕后）发现不变量破坏，pc 直落 open_pc 跑原始区域，
    // 绝不重放前缀（sys_dummy 恰 2 行而非 3 行），也不得回滚前缀写入。
    let sql = "CREATE PROCEDURE copy_entry IS BEGIN \
        INSERT INTO sys_dummy VALUES ('Y'); COMMIT; \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.a); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_entry", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(dummy_count(&baseline), 2);

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_entry", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(dummy_count(&eng), 2, "前缀写入必须恰好执行一次且不被回滚");
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenSysDummyInvariant
        }
    );
}

#[test]
fn should_preserve_uncommitted_prefix_row_seven_when_entry_guard_falls_back() {
    let sql = "CREATE PROCEDURE copy_uncommitted IS BEGIN \
        INSERT INTO target_rows(val) VALUES (7); \
        INSERT INTO sys_dummy VALUES ('Y'); \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        INSERT INTO target_rows(val) VALUES (r.a); \
        END LOOP; END;";
    for mode in [SetOrientedMode::Off, SetOrientedMode::Strict] {
        let mut eng = engine();
        eng.register(sql).unwrap();
        eng.set_oriented = mode;
        eng.call("copy_uncommitted", &[]).unwrap();
        assert_eq!(
            rows(&eng),
            vec![
                vec![SpinValue::Int(7)],
                vec![SpinValue::Int(1)],
                vec![SpinValue::Int(3)]
            ],
            "mode={mode:?}"
        );
        assert_eq!(dummy_count(&eng), 2);
        if mode == SetOrientedMode::Strict {
            assert_eq!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped {
                    reason: SkipReason::UnprovenSysDummyInvariant
                }
            );
        }
    }
}

fn ordered_date_engine(path: &std::path::Path) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open(path.to_str().unwrap()).unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE sys_dummy AS SELECT 1",
        "CREATE TABLE dates(id BIGINT PRIMARY KEY, eligible BIGINT, d VARCHAR)",
        "CREATE TABLE dated_output(id BIGINT, d VARCHAR)",
        "INSERT INTO dates VALUES (3, 1, '20260103'), (2, 1, 'bad-date'), \
         (1, 1, '20260101'), (0, 0, 'also-bad')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE PROCEDURE copy_dates IS v VARCHAR2; BEGIN \
         FOR r IN SELECT id, d FROM dates WHERE eligible = 1 ORDER BY id LOOP \
         v := to_char(to_date(r.d, 'YYYYMMDD'), 'YYYY-MM-DD'); \
         BEGIN INSERT INTO dated_output(id, d) VALUES (r.id, v); COMMIT; \
         EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
    )
    .unwrap();
    eng
}

#[test]
fn should_preserve_first_commit_on_ordered_second_row_date_failure() {
    use spinduck_core::SqlExecutor;
    let baseline_root = tempfile::tempdir().unwrap();
    let mut baseline = ordered_date_engine(&baseline_root.path().join("baseline.db"));
    let expected = baseline.call("copy_dates", &[]).unwrap_err();
    assert_eq!(expected.code.0, "KERNEL-0010");
    assert_eq!(expected.domain, spinduck_core::error::FaultDomain::Kernel);
    assert!(
        expected.message.contains("bad-date"),
        "{}",
        expected.message
    );
    for mode in [
        SetOrientedMode::Off,
        SetOrientedMode::Strict,
        SetOrientedMode::BatchAtomic,
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ordered.db");
        let mut eng = ordered_date_engine(&path);
        eng.set_oriented = mode;
        let actual = eng.call("copy_dates", &[]).unwrap_err();
        assert_eq!(actual.code, expected.code, "mode={mode:?}");
        assert_eq!(actual.domain, expected.domain, "mode={mode:?}");
        assert_eq!(actual.message, expected.message, "mode={mode:?}");
        if mode != SetOrientedMode::Off {
            assert!(matches!(
                eng.set_oriented_reports()[0].status,
                Status::Skipped { .. }
            ));
        }
        drop(eng);
        let reopened = DuckExecutor::open(path.to_str().unwrap()).unwrap();
        let persisted = reopened
            .query(
                "SELECT id, d FROM dated_output ORDER BY id",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(10);
        assert_eq!(
            persisted,
            vec![vec![SpinValue::Int(1), SpinValue::Str("2026-01-01".into())]]
        );
    }
}

#[test]
fn should_match_error_code_and_message_on_reachable_bare_expression_failure() {
    // 无 trap 的裸表达式写入：坏行错误直接向上传播；集合化必须整段跳过，
    // 且错误码与消息和 Off 逐字一致（不得吞错、不得改判故障域）。
    let sql = "CREATE PROCEDURE copy_bare IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        INSERT INTO target_rows(val) VALUES (r.a / r.b); \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
            "CREATE TABLE target_rows(val DECIMAL(15,3))",
            "INSERT INTO source_rows VALUES (1, 0), (6, 3)",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    let off_err = baseline.call("copy_bare", &[]).unwrap_err();

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let err = eng.call("copy_bare", &[]).unwrap_err();
    assert_eq!(err.code, off_err.code);
    assert_eq!(err.message, off_err.message);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedWrite
        }
    );
}
