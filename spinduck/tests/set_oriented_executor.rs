//! 集合化执行接缝的 SQL、绑定、事务与无回放守护。

use spinduck_core::exec::{ExecOutcome, FakeExecutor, RowStream, SqlExecutor};
use spinduck_core::{BindMap, Engine, EngineError, RuleRegistry, SpinValue};
use std::rc::Rc;

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id FROM source_rows LOOP \
    INSERT INTO target_rows(id) VALUES (r.id); END LOOP; END;";

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
    let mut eng = Engine::new(
        Box::new(FailingBulk(recorder.clone())),
        RuleRegistry::default(),
    );
    eng.register(COPY).unwrap();
    eng.set_oriented = true;
    let error = eng.call("copy_rows", &[]).unwrap_err();
    assert_eq!(error.code.0, "KERNEL-0010");
    assert_eq!(error.message, "injected bulk failure");
    let calls = recorder.calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[0], ("BEGIN TRANSACTION".into(), BindMap::new()));
    for (call, table) in calls[1..3].iter().zip(["source_rows", "target_rows"]) {
        assert!(call.0.contains("FROM duckdb_tables()"));
        assert!(call.0.contains("FROM duckdb_indexes()"));
        assert_eq!(
            call.1,
            BindMap::from([("$table".into(), SpinValue::Str(table.into()))])
        );
    }
    assert_eq!(calls[3], (
        "WITH sd_cur AS (SELECT \"id\" FROM \"memory\".\"main\".\"source_rows\") INSERT INTO \"memory\".\"main\".\"target_rows\" (\"id\") SELECT sd_cur.\"id\" FROM sd_cur".into(),
        BindMap::new(),
    ));
    assert_eq!(calls[4], ("ROLLBACK".into(), BindMap::new()));
}
