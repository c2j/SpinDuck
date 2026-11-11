//! trace 事件注入契约（AGENTS §1 接缝）：调用方给的 sink 必须收到引擎埋点。
//!
//! `Engine::call` 保持默认静默（`TraceNoopSink`），注入走新入口 `call_with_sink`——
//! 默认行为逐字不变是这一层的契约，不只是实现选择。

use spinduck_core::cli::NoopExecutor;
use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor, TraceEvent, TraceSink};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::value::SpinValue;
use std::cell::RefCell;
use std::rc::Rc;

type Log = Rc<RefCell<Vec<String>>>;

/// 假 sink：把事件收成一行人可读口径，供逐条断言（顺序即事件顺序）。
struct Recorder {
    log: Log,
}

impl Recorder {
    fn new() -> (Self, Log) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        (Self { log: log.clone() }, log)
    }
}

impl TraceSink for Recorder {
    fn on_event(&mut self, ev: TraceEvent) {
        let line = match ev {
            TraceEvent::SqlExec { sql, binds, .. } => {
                let pairs = binds
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v.as_str_loose()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("SQL_EXEC {sql} binds={{{pairs}}}")
            }
            TraceEvent::CursorOpen { sql, schema } => {
                format!("CURSOR_OPEN {sql} cols={{{}}}", schema.join(","))
            }
            TraceEvent::Fetch { row } => {
                format!("FETCH row={}", row.unwrap_or(0))
            }
            TraceEvent::UdfCall { name } => format!("UDF_CALL {name}"),
            TraceEvent::IrStmt { variant } => format!("IR_STMT {variant}"),
            TraceEvent::BatchSkip { rows, first_error } => {
                format!("BATCH_SKIP rows={rows} first_error={first_error}")
            }
        };
        self.log.borrow_mut().push(line);
    }
}

fn engine() -> Engine {
    Engine::new(Box::new(NoopExecutor), RuleRegistry::default())
}

/// 按脚本返回行的假执行器（游标 schema 只能来自结果流，NoopExecutor 给不了列名）。
/// 每次查询返回同一份行：下沉路径一条语句要问多次，消费式脚本会让第二次查询空转。
struct Scripted {
    schema: Vec<String>,
    rows: Vec<Vec<SpinValue>>,
}

impl Scripted {
    fn with_rows(schema: &[&str], rows: Vec<Vec<SpinValue>>) -> Self {
        Self {
            schema: schema.iter().map(|s| s.to_string()).collect(),
            rows,
        }
    }
}

impl SqlExecutor for Scripted {
    fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        Ok(RowStream::new(self.schema.clone(), self.rows.clone()))
    }
    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        Ok(ExecOutcome { rows_affected: 0 })
    }
}

#[test]
fn should_send_sql_exec_events_to_the_caller_supplied_sink() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_probe(p_date IN VARCHAR2) IS \
         BEGIN \
           INSERT INTO t_log VALUES (p_date); \
         END;",
    )
    .unwrap();
    let (mut sink, log) = Recorder::new();
    eng.call_with_sink(
        "prc_probe",
        &[SpinValue::Str("20260105".to_string())],
        &mut sink,
    )
    .unwrap();
    assert_eq!(
        log.borrow().as_slice(),
        [
            "IR_STMT SqlExec",
            "SQL_EXEC insert into t_log values ( $v_p_date ) binds={$v_p_date=20260105}",
        ]
    );
}

#[test]
fn should_report_the_opened_cursor_columns() {
    // CURSOR_OPEN 的列名是 explain 对齐与人工排障的最小抓手：空 schema = 事件白emit
    let mut eng = Engine::new(
        Box::new(Scripted::with_rows(
            &["qid", "js_days"],
            vec![vec![SpinValue::Int(1), SpinValue::Int(7)]],
        )),
        RuleRegistry::default(),
    );
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_cur IS \
         CURSOR c IS SELECT 1 AS qid, 7 AS js_days; \
         BEGIN \
           OPEN c; \
           CLOSE c; \
         END;",
    )
    .unwrap();
    let (mut sink, log) = Recorder::new();
    eng.call_with_sink("prc_cur", &[], &mut sink).unwrap();
    let lines = log.borrow().clone();
    let line = lines
        .iter()
        .find(|s| s.starts_with("CURSOR_OPEN"))
        .expect("OPEN 应产出 CURSOR_OPEN 事件");
    assert!(
        line.ends_with(" cols={qid,js_days}"),
        "游标列名应来自结果流：{line}"
    );
}

#[test]
fn should_carry_the_cursor_query_text_into_the_open_event() {
    // 只有列名没有查询文本的 CURSOR_OPEN 无法反查 sqlid：审计面必须能指回那条 SQL
    let mut eng = Engine::new(
        Box::new(Scripted::with_rows(&["qid", "js_days"], Vec::new())),
        RuleRegistry::default(),
    );
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_cur_text IS \
         CURSOR c IS SELECT 1 AS qid, 7 AS js_days; \
         BEGIN \
           OPEN c; \
           CLOSE c; \
         END;",
    )
    .unwrap();
    let (mut sink, log) = Recorder::new();
    eng.call_with_sink("prc_cur_text", &[], &mut sink).unwrap();
    let lines = log.borrow().clone();
    let line = lines
        .iter()
        .find(|s| s.starts_with("CURSOR_OPEN"))
        .expect("OPEN 应产出 CURSOR_OPEN 事件");
    assert_eq!(
        line,
        "CURSOR_OPEN SELECT 1 AS qid, 7 AS js_days cols={qid,js_days}"
    );
}

#[test]
fn should_inherit_the_sink_into_a_nested_procedure_call() {
    // 嵌套过程的埋点不得因中途换回 TraceNoopSink 而静默：一条 CALL 的审计面
    // 必须覆盖它调用的全部例程（§11.1 禁止静默丢弃）。
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_inner(p IN NUMBER) IS \
         BEGIN \
           INSERT INTO t_a VALUES (p); \
         END;",
    )
    .unwrap();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_outer IS \
         BEGIN \
           prc_inner(1); \
         END;",
    )
    .unwrap();
    let (mut sink, log) = Recorder::new();
    eng.call_with_sink("prc_outer", &[], &mut sink).unwrap();
    assert_eq!(
        log.borrow().as_slice(),
        [
            "IR_STMT CallRoutine",
            "IR_STMT SqlExec",
            "SQL_EXEC insert into t_a values ( $v_p ) binds={$v_p=1}",
        ]
    );
}

#[test]
fn should_inherit_the_sink_into_a_function_called_from_a_pl_expression() {
    // `v := f(2)` 走的不是 CallRoutine 而是表达式求值：那里的埋点同样不得断链
    let mut eng = Engine::new(
        Box::new(Scripted::with_rows(&["one"], vec![vec![SpinValue::Int(1)]])),
        RuleRegistry::default(),
    );
    eng.register(
        "CREATE OR REPLACE FUNCTION f_stamp(p IN NUMBER) RETURN NUMBER IS \
         v NUMBER; \
         BEGIN \
           SELECT 1 INTO v; \
           RETURN v + p; \
         END;",
    )
    .unwrap();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_use IS \
         v NUMBER; \
         BEGIN \
           v := f_stamp(2); \
         END;",
    )
    .unwrap();
    let (mut sink, log) = Recorder::new();
    eng.call_with_sink("prc_use", &[], &mut sink).unwrap();
    assert_eq!(
        log.borrow().as_slice(),
        [
            // 外层：声明初始化 + `v := f_stamp(2)`；被调函数：声明初始化 + SELECT INTO + RETURN
            "IR_STMT Assign",
            "IR_STMT Assign",
            "IR_STMT Assign",
            "IR_STMT SqlQuery",
            "SQL_EXEC SELECT 1 binds={}",
            "IR_STMT Return",
        ]
    );
}

#[test]
fn should_inherit_the_sink_into_a_function_downshifted_into_sql() {
    // SQL 里的例程调用走下沉物化（§8.3 路径②），与 CALL 下去的是同一条埋点链：
    // 换汇即断链，审计日志会整段看不见被下沉例程干了什么。
    let mut eng = Engine::new(
        Box::new(Scripted::with_rows(&["x"], vec![vec![SpinValue::Int(3)]])),
        RuleRegistry::default(),
    );
    eng.register(
        "CREATE OR REPLACE FUNCTION f_double(p IN NUMBER) RETURN NUMBER IS \
         v NUMBER; \
         BEGIN \
           SELECT 2 INTO v; \
           RETURN v * p; \
         END;",
    )
    .unwrap();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_sqlfn IS \
         v NUMBER; \
         BEGIN \
           SELECT f_double(3) INTO v; \
         END;",
    )
    .unwrap();
    let (mut sink, log) = Recorder::new();
    eng.call_with_sink("prc_sqlfn", &[], &mut sink).unwrap();
    assert_eq!(
        log.borrow().as_slice(),
        [
            // 外层：声明初始化 + SqlQuery 语句头
            "IR_STMT Assign",
            "IR_STMT SqlQuery",
            // 下沉物化里被调函数完整可见：声明初始化 + SELECT INTO + RETURN
            "IR_STMT Assign",
            "IR_STMT SqlQuery",
            "SQL_EXEC SELECT 2 binds={}",
            "IR_STMT Return",
            // 外层语句最终文本（引用缓存子查询）在物化之后才下发
            "SQL_EXEC SELECT (SELECT sd_v FROM sd_fn0 WHERE sd_a0 IS NOT DISTINCT FROM (3)) \
             binds={}",
        ]
    );
}
