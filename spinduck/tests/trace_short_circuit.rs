//! Trace 短路契约：`is_enabled() = false` 的 sink 不得收到任何事件——
//! 引擎必须在构造事件（sql.to_string() / binds.clone()）之前短路，
//! 而不是依赖 sink 自身空实现吞掉事件。

use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor, TraceEvent, TraceSink};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::value::SpinValue;

/// 禁用 sink：收到事件即短路失效，panic 就是断言。
struct DisabledSink;

impl TraceSink for DisabledSink {
    fn is_enabled(&self) -> bool {
        false
    }
    fn on_event(&mut self, event: TraceEvent) {
        panic!("disabled sink must not receive events: {event:?}");
    }
}

/// 任意查询返回同一份行的假执行器（SELECT INTO / 游标需要 schema）。
struct Scripted;

impl SqlExecutor for Scripted {
    fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        Ok(RowStream::new(
            vec!["a".to_string()],
            vec![vec![SpinValue::Str("x".to_string())]],
        ))
    }
    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

fn engine() -> Engine {
    Engine::new(Box::new(Scripted), RuleRegistry::default())
}

const SQL_PROC: &str = "CREATE OR REPLACE PROCEDURE prc_sql IS \
     v_a VARCHAR2(10); \
     BEGIN \
       SELECT 'y' INTO v_a; \
       INSERT INTO t_log VALUES (v_a); \
       COMMIT; \
     END;";

#[test]
fn should_not_construct_events_for_disabled_sink_on_sql_paths() {
    let mut eng = engine();
    eng.register(SQL_PROC).unwrap();
    eng.call_with_sink("prc_sql", &[], &mut DisabledSink)
        .unwrap();
}

#[test]
fn should_not_construct_events_for_disabled_sink_on_batch_flush() {
    let mut eng = engine();
    eng.batch_commit = true;
    eng.register(SQL_PROC).unwrap();
    eng.call_with_sink("prc_sql", &[], &mut DisabledSink)
        .unwrap();
}

#[test]
fn should_not_construct_events_for_disabled_sink_on_cursor_paths() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_cur IS \
         CURSOR c IS SELECT 'x' AS a; \
         v_a VARCHAR2(10); \
         BEGIN \
           OPEN c; \
           FETCH c INTO v_a; \
           FETCH c INTO v_a; \
           CLOSE c; \
         END;",
    )
    .unwrap();
    eng.call_with_sink("prc_cur", &[], &mut DisabledSink)
        .unwrap();
}

struct FailBadRow {
    calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl SqlExecutor for FailBadRow {
    fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        Ok(RowStream::new(Vec::new(), Vec::new()))
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        if sql.to_ascii_lowercase().starts_with("insert") {
            assert_eq!(binds, &BindMap::new());
            self.calls.borrow_mut().push(
                sql.split_whitespace()
                    .collect::<String>()
                    .to_ascii_lowercase(),
            );
        }
        if sql.contains("'bad'") {
            return Err(EngineError::kernel(10, "bad row"));
        }
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

#[derive(Default)]
struct EnabledSink(Vec<TraceEvent>);

impl TraceSink for EnabledSink {
    fn on_event(&mut self, event: TraceEvent) {
        self.0.push(event);
    }
}

#[test]
fn should_preserve_batch_skip_replay_with_disabled_sink() {
    for enabled in [false, true] {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut eng = Engine::new(
            Box::new(FailBadRow {
                calls: calls.clone(),
            }),
            RuleRegistry::default(),
        );
        eng.batch_commit = true;
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_skip IS BEGIN \
            INSERT INTO t_log VALUES ('first'); INSERT INTO t_log VALUES ('bad'); \
            INSERT INTO t_log VALUES ('last'); END;",
        )
        .unwrap();
        let mut recording = EnabledSink::default();
        let mut disabled = DisabledSink;
        let sink: &mut dyn TraceSink = if enabled {
            &mut recording
        } else {
            &mut disabled
        };
        eng.call_with_sink("prc_skip", &[], sink).unwrap();
        assert_eq!(
            *calls.borrow(),
            vec![
                "insertintot_logvalues('first'),('bad'),('last')",
                "insertintot_logvalues('first')",
                "insertintot_logvalues('bad')",
                "insertintot_logvalues('last')",
            ]
        );
        let skips: Vec<_> = recording
            .0
            .iter()
            .filter_map(|e| match e {
                TraceEvent::BatchSkip { rows, first_error } => Some((*rows, first_error.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            skips,
            if enabled {
                vec![(1, "KERNEL-0010 bad row")]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn should_default_trace_sink_to_enabled_and_noop_to_disabled() {
    assert!(!spinduck_core::exec::TraceNoopSink.is_enabled());
    assert!(!spinduck_core::engine::TraceNoopSink.is_enabled());
    struct Plain;
    impl TraceSink for Plain {
        fn on_event(&mut self, _event: TraceEvent) {}
    }
    assert!(
        Plain.is_enabled(),
        "未覆写的 sink 必须默认启用（不破坏现有消费者）"
    );
}
