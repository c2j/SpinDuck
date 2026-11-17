//! 集合化执行接缝的 SQL、绑定、事务与无回放守护。

use spinduck_core::exec::{ExecOutcome, FakeExecutor, RowStream, SqlExecutor};
use spinduck_core::{BindMap, Engine, EngineError, RuleRegistry, SpinValue};
use std::rc::Rc;

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id FROM source_rows LOOP \
    INSERT INTO target_rows(id) VALUES (r.id); END LOOP; END;";

#[cfg(feature = "native")]
struct RecordedNative {
    inner: spinduck_core::DuckExecutor,
    calls: Rc<std::cell::RefCell<Vec<(String, BindMap)>>>,
}

#[cfg(feature = "native")]
impl SqlExecutor for RecordedNative {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.inner.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.calls
            .borrow_mut()
            .push((sql.to_string(), binds.clone()));
        self.inner.execute(sql, binds)
    }
}

#[cfg(feature = "native")]
#[test]
fn should_not_commit_the_batch_when_every_source_row_is_filtered() {
    use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
    let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut eng = Engine::new(
        Box::new(RecordedNative {
            inner: spinduck_core::DuckExecutor::open_in_memory().unwrap(),
            calls: calls.clone(),
        }),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        "CREATE TABLE target_rows(val DECIMAL(15,3))",
        "INSERT INTO source_rows VALUES (1, 0), (0, 0)",
        "CREATE TABLE sys_dummy AS SELECT 1",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE PROCEDURE copy_filtered IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP BEGIN \
        INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
    )
    .unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    calls.borrow_mut().clear();
    eng.call("copy_filtered", &[]).unwrap();
    let Status::Converted { sql } = &eng.set_oriented_reports()[0].status else {
        panic!("{:?}", eng.set_oriented_reports());
    };
    assert_eq!(
        *calls.borrow(),
        vec![
            ("BEGIN TRANSACTION".into(), BindMap::new()),
            (sql.clone(), BindMap::new()),
            ("COMMIT".into(), BindMap::new()),
        ]
    );
    assert_eq!(
        eng.ex
            .query("SELECT val FROM target_rows", &BindMap::new())
            .unwrap()
            .fetch_many(10),
        Vec::<Vec<SpinValue>>::new()
    );
}

struct FailingBulk(Rc<FakeExecutor>);

impl SqlExecutor for FailingBulk {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        let result = self.0.execute(sql, binds)?;
        if sql.starts_with("WITH sd_cur") {
            Err(EngineError::kernel(10, "injected bulk failure"))
        } else {
            Ok(result)
        }
    }
}

#[test]
fn should_rollback_bulk_failure_without_replaying_original_instructions() {
    let recorder = Rc::new(FakeExecutor::default());
    // 证明期与入口守卫各一轮：sys_dummy 存在性 + COUNT(*)=1 + 双表结构
    for _ in 0..2 {
        recorder.push_script(RowStream::new(Vec::new(), vec![vec![SpinValue::Int(1)]]));
        recorder.push_script(RowStream::new(Vec::new(), vec![vec![SpinValue::Int(1)]]));
        for name in ["source_rows", "target_rows"] {
            recorder.push_script(RowStream::new(
                Vec::new(),
                vec![vec![
                    SpinValue::Str("memory".into()),
                    SpinValue::Str("main".into()),
                    SpinValue::Str(name.into()),
                    SpinValue::Str(format!("CREATE TABLE {name}(id BIGINT);")),
                    SpinValue::Bool(true),
                ]],
            ));
        }
    }
    let mut eng = Engine::new(
        Box::new(FailingBulk(recorder.clone())),
        RuleRegistry::default(),
    );
    eng.register(COPY).unwrap();
    eng.set_oriented = spinduck_core::ir::set_oriented::SetOrientedMode::Strict;
    let error = eng.call("copy_rows", &[]).unwrap_err();
    assert_eq!(error.code.0, "KERNEL-0010");
    assert_eq!(error.message, "injected bulk failure");
    let calls = recorder.calls();
    assert_eq!(calls.len(), 11);
    assert_eq!(calls[0], ("BEGIN TRANSACTION".into(), BindMap::new()));
    for base in [1, 5] {
        assert!(calls[base].0.contains("FROM duckdb_tables()"));
        assert!(calls[base].0.contains("FROM duckdb_views()"));
        assert_eq!(calls[base + 1].0, "SELECT COUNT(*) FROM sys_dummy");
        for (call, table) in calls[base + 2..base + 4]
            .iter()
            .zip(["source_rows", "target_rows"])
        {
            assert!(call.0.contains("FROM duckdb_tables()"));
            assert!(call.0.contains("FROM duckdb_indexes()"));
            assert_eq!(
                call.1,
                BindMap::from([("$table".into(), SpinValue::Str(table.into()))])
            );
        }
    }
    assert_eq!(calls[9], (
        "WITH sd_cur AS (SELECT \"id\" FROM \"memory\".\"main\".\"source_rows\") INSERT INTO \"memory\".\"main\".\"target_rows\" (\"id\") SELECT sd_cur.\"id\" FROM sd_cur".into(),
        BindMap::new(),
    ));
    assert_eq!(calls[10], ("ROLLBACK".into(), BindMap::new()));
}
