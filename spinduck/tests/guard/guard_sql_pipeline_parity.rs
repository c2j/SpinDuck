//! 行为锁定（AGENTS §4.3-2）：同一 SQL 文本经静态语句编译与动态 SQL
//! （EXECUTE IMMEDIATE）准备，改写产物必须逐字相同——两条路径共用一套改写口径。
//!
//! 期望值在统一改写实现时**不得变更**；若变更即说明口径漂移，必须单独评审。

#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::value::SpinValue;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder {
    sqls: Mutex<Vec<String>>,
}

struct RecordingExecutor(Arc<Recorder>);

impl SqlExecutor for RecordingExecutor {
    fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.sqls.lock().unwrap().push(sql.to_string());
        // 裸 SELECT 在 PL 里即 SELECT INTO：给 1 行避免 NO_DATA_FOUND 干扰比对
        Ok(RowStream::new(
            vec!["v".to_string()],
            vec![vec![SpinValue::Int(1)]],
        ))
    }
    fn execute(&self, sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.0.sqls.lock().unwrap().push(sql.to_string());
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

/// 事务包络语句（统一事务边界）不属于改写产物，比对时剔除。
fn is_transaction_envelope(sql: &str) -> bool {
    let head = sql.trim().to_ascii_uppercase();
    head.starts_with("BEGIN TRANSACTION")
        || head.starts_with("COMMIT")
        || head.starts_with("ROLLBACK")
}

fn rewritten(body: &str) -> Vec<String> {
    let rec = Arc::new(Recorder::default());
    let mut eng = Engine::new(
        Box::new(RecordingExecutor(rec.clone())),
        RuleRegistry::default(),
    );
    eng.register(&format!(
        "CREATE OR REPLACE PROCEDURE prc_probe IS BEGIN {body} END;"
    ))
    .expect("register prc_probe");
    let out = eng.call("prc_probe", &[]);
    let sqls = rec.sqls.lock().unwrap().clone();
    if let Err(e) = out {
        panic!("调用失败: {e}；收到的 SQL: {sqls:?}");
    }
    sqls.into_iter()
        .filter(|s| !is_transaction_envelope(s))
        .collect()
}

/// 静态文本 → 单引号包裹的动态文本（语料无单引号，无需转义）。
fn quote(sql: &str) -> String {
    assert!(!sql.contains('\''), "比对语料不得含单引号");
    format!("EXECUTE IMMEDIATE '{sql}'")
}

/// `[patterns] rownum_first_row` 覆盖范围（validation V13 语料形态）：
/// 顶层 SELECT 与 INSERT ... SELECT 的子查询可摘除谓词并补 LIMIT 1。
const ROWNUM_REWRITTEN: &[&str] = &[
    "SELECT NVL(a, 0) FROM src WHERE rownum = 1",
    "INSERT INTO tgt SELECT NVL(a, 0) FROM src WHERE rownum = 1",
];

/// 未覆盖形态：DuckDB 无 rownum 且 UPDATE/DELETE 无对应 LIMIT 语法位，改写层
/// 原样下发（内核给出明确报错）。锁定"两口径一致"，覆盖面扩到 M8 时改此期望。
const ROWNUM_PASSTHROUGH: &[&str] = &[
    "UPDATE tgt SET tgt.b = NVL(a, 1) WHERE rownum = 1",
    "DELETE FROM tgt WHERE NVL(b, 0) > 1 AND rownum = 1",
];

/// 归一化比对：大小写与空白来自文本渲染器（静态 DML 走 PL 解析器文本、
/// 动态走 SqlFormatter），规则产物必须一致；因此按"去空白 + 小写"比对。
fn normalize(sql: &str) -> String {
    sql.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 两路径产物归一化相同；`strip_rownum` 为真时另须已摘除 rownum 并补 LIMIT 1。
fn assert_parity(stmt: &str, strip_rownum: bool) {
    let static_out = rewritten(&format!("{stmt};"));
    let dynamic_out = rewritten(&format!("{};", quote(stmt)));
    assert_eq!(static_out.len(), 1, "静态应只收到一条改写后的语句");
    assert_eq!(dynamic_out.len(), 1, "动态应只收到一条改写后的语句");
    let (s, d) = (normalize(&static_out[0]), normalize(&dynamic_out[0]));
    assert_eq!(
        s, d,
        "静态与动态改写产物漂移，语句: {stmt}\n静态: {}\n动态: {}",
        static_out[0], dynamic_out[0]
    );
    assert!(!s.contains("nvl"), "NVL 必须按 [functions] 改名，实际: {s}");
    if strip_rownum {
        assert!(!s.contains("rownum"), "rownum 必须被摘除，实际: {s}");
        assert!(s.contains("limit1"), "rownum=1 必须换成 LIMIT 1，实际: {s}");
    } else {
        assert!(s.contains("rownum"), "未覆盖形态须原样下发，实际: {s}");
    }
}

#[test]
fn should_produce_identical_text_for_static_and_dynamic_paths() {
    for stmt in ROWNUM_REWRITTEN {
        assert_parity(stmt, true);
    }
    for stmt in ROWNUM_PASSTHROUGH {
        assert_parity(stmt, false);
    }
}

/// 语料主形态（42 处，validation V13）：rownum 藏在 INSERT ... SELECT 的子查询里，
/// 曾只在顶层 SELECT 生效 → 原样下发必然内核报错。
#[test]
fn should_strip_rownum_in_insert_select_on_both_paths() {
    for (label, out) in [
        (
            "静态",
            rewritten("INSERT INTO tgt SELECT a FROM src WHERE rownum = 1;"),
        ),
        (
            "动态",
            rewritten(&format!(
                "{};",
                quote("INSERT INTO tgt SELECT a FROM src WHERE rownum = 1")
            )),
        ),
    ] {
        assert_eq!(out.len(), 1, "{label}路径应下发一条语句，实际: {out:?}");
        let n = normalize(&out[0]);
        assert!(
            !n.contains("rownum"),
            "{label}路径未摘除 rownum（DuckDB 无该伪列）: {}",
            out[0]
        );
        assert!(n.contains("limit1"), "{label}路径未补 LIMIT 1: {}", out[0]);
    }
}
