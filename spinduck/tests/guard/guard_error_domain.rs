//! 回归守护（AGENTS §5）：统一事务边界的回滚失败不得改写主因的故障域。
//!
//! `Engine::call` 在异常路径执行 ROLLBACK；若 ROLLBACK 本身失败，主因（业务
//! 异常）必须保留原错误码与域，次因（内核回滚失败）并入消息。§5 明确禁止在
//! 调用上层事后改判归域——回滚失败是次因，不得把 BIZ 改写成 KERNEL。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::error::{EngineError, FaultDomain};
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::rules::RuleRegistry;

/// 假执行器：查询恒返回 0 行（触发 NO_DATA_FOUND）；ROLLBACK 注入失败。
struct RollbackFailingExecutor;

impl SqlExecutor for RollbackFailingExecutor {
    fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        Ok(RowStream::new(vec!["v".to_string()], Vec::new()))
    }
    fn execute(&self, sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        if sql.trim().to_ascii_uppercase().starts_with("ROLLBACK") {
            return Err(EngineError::kernel(11, "kernel refuses rollback"));
        }
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

/// SELECT INTO 0 行 → BIZ-0100；即便统一 ROLLBACK 失败，主因仍是业务域。
#[test]
fn should_keep_business_code_when_rollback_also_fails() {
    let mut eng = Engine::new(Box::new(RollbackFailingExecutor), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_ndf IS v INT; BEGIN \
         SELECT 1 INTO v FROM src_t; END;",
    )
    .expect("register prc_ndf");

    let e = eng
        .call("prc_ndf", &[])
        .expect_err("SELECT INTO 0 行必须外抛 NO_DATA_FOUND");
    assert_eq!(
        e.domain,
        FaultDomain::Business,
        "回滚失败是次因，不得把业务异常改判为内核域（§5）"
    );
    assert_eq!(
        e.code.to_string(),
        "BIZ-0100",
        "主因错误码必须保持 NO_DATA_FOUND，实际: {}",
        e
    );
    assert!(
        e.message.contains("NO_DATA_FOUND"),
        "消息须保留主因片段，实际: {}",
        e.message
    );
    assert!(
        e.message.contains("ROLLBACK"),
        "消息须并入次因（回滚失败），实际: {}",
        e.message
    );
}

/// 回滚成功时保持单因错误，消息不得带上次因痕迹。
#[test]
fn should_report_single_cause_when_rollback_succeeds() {
    struct OkExecutor;
    impl SqlExecutor for OkExecutor {
        fn query(&self, _sql: &str, _b: &BindMap) -> Result<RowStream, EngineError> {
            Ok(RowStream::new(vec!["v".to_string()], Vec::new()))
        }
        fn execute(&self, _sql: &str, _b: &BindMap) -> Result<ExecOutcome, EngineError> {
            Ok(ExecOutcome { rows_affected: 1 })
        }
    }
    let mut eng = Engine::new(Box::new(OkExecutor), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_ndf2 IS v INT; BEGIN \
         SELECT 1 INTO v FROM src_t; END;",
    )
    .expect("register prc_ndf2");
    let e = eng
        .call("prc_ndf2", &[])
        .expect_err("SELECT INTO 0 行必须外抛");
    assert_eq!(e.code.to_string(), "BIZ-0100");
    assert!(
        !e.message.contains("ROLLBACK"),
        "回滚成功不应污染消息，实际: {}",
        e.message
    );
}
