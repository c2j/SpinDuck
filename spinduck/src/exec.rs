//! SQL 执行接缝：引擎内除本文件的原生实现外，任何代码不得触碰 DuckDB API
//! （AGENTS.md §1）。`FakeExecutor` 供单元/集成测试注入。

use crate::error::EngineError;
use crate::value::SpinValue;
use std::cell::RefCell;
use std::collections::BTreeMap;

/// 按名绑定表：编译期变量替换产生 `$v_<名>` 占位符，运行期按名绑定。
pub type BindMap = BTreeMap<String, SpinValue>;

/// 一行结果（按结果 schema 位置序）。
pub type Row = Vec<SpinValue>;

/// 查询结果流（快照语义：OPEN 时产出；FETCH 逐行拉取）。
pub struct RowStream {
    pub schema: Vec<String>,
    rows: Vec<Row>,
    pos: usize,
}

impl RowStream {
    pub fn new(schema: Vec<String>, rows: Vec<Row>) -> Self {
        Self {
            schema,
            rows,
            pos: 0,
        }
    }
    /// 拉取至多 n 行（SELECT INTO 行数语义的探测原语：fetchmany(2)）。
    pub fn fetch_many(&mut self, n: usize) -> Vec<Row> {
        let end = (self.pos + n).min(self.rows.len());
        let out = self.rows[self.pos..end].to_vec();
        self.pos = end;
        out
    }
    pub fn reset(&mut self) {
        self.pos = 0;
    }
}

/// 执行结果：DML 返回受影响行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOutcome {
    pub rows_affected: usize,
}

/// SQL 执行接缝：解释器/编译器唯一可见的内核能力面。
pub trait SqlExecutor {
    /// 执行查询并返回流（SELECT 类）。
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError>;
    /// 执行 DML/DDL，返回受影响行数。
    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError>;
}

/// 可观测埋点接缝：core 一次埋点，CLI 渲染与生产监控共用（§2.1/§10.5）。
pub trait TraceSink {
    fn on_event(&mut self, event: TraceEvent);
}

/// trace 事件（§11.2 JSONL 契约的引擎侧枚举形态）。
#[derive(Debug, Clone)]
pub enum TraceEvent {
    SqlExec {
        sql: String,
        binds: BindMap,
        elapsed_us: u64,
    },
    CursorOpen {
        schema: Vec<String>,
    },
    Fetch {
        row: Option<usize>,
    },
    UdfCall {
        name: String,
    },
    IrStmt {
        variant: &'static str,
    },
}

/// 测试用假执行器：记录收到的 SQL 与绑定表，按预置脚本返回结果。
/// 单元/集成测试验证"改写产物 + 按名绑定"的唯一可信方式（AGENTS §4.2）。
#[derive(Default)]
pub struct FakeExecutor {
    calls: RefCell<Vec<(String, BindMap)>>,
    scripted: RefCell<Vec<RowStream>>,
}

impl FakeExecutor {
    pub fn with_rows(schema: Vec<String>, rows: Vec<Row>) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            scripted: RefCell::new(vec![RowStream::new(schema, rows)]),
        }
    }
    pub fn push_script(&self, stream: RowStream) {
        self.scripted.borrow_mut().push(stream);
    }
    pub fn calls(&self) -> Vec<(String, BindMap)> {
        self.calls.borrow().clone()
    }
}

impl SqlExecutor for FakeExecutor {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.calls
            .borrow_mut()
            .push((sql.to_string(), binds.clone()));
        let next = self.scripted.borrow_mut().pop_front_or_empty();
        Ok(next)
    }
    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.calls
            .borrow_mut()
            .push((sql.to_string(), binds.clone()));
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

trait PopFrontOrEmpty {
    fn pop_front_or_empty(&mut self) -> RowStream;
}

impl PopFrontOrEmpty for Vec<RowStream> {
    fn pop_front_or_empty(&mut self) -> RowStream {
        if self.is_empty() {
            RowStream::new(Vec::new(), Vec::new())
        } else {
            self.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_record_calls_with_named_binds() {
        let ex = FakeExecutor::with_rows(vec!["a".into()], vec![vec![SpinValue::Int(1)]]);
        let mut b = BindMap::new();
        b.insert("v_date".to_string(), SpinValue::Str("20240105".into()));
        let rows = ex
            .query("SELECT * FROM t WHERE bcrq = $v_date", &b)
            .unwrap()
            .fetch_many(2);
        assert_eq!(rows.len(), 1);
        assert_eq!(ex.calls().len(), 1);
        assert_eq!(ex.calls()[0].1["v_date"], SpinValue::Str("20240105".into()));
    }

    #[test]
    fn should_probe_row_count_for_select_into_semantics() {
        // SELECT INTO 语义：fetchmany(2) → 0 行 NO_DATA_FOUND / ≥2 行 TOO_MANY_ROWS
        let ex = FakeExecutor::with_rows(
            vec!["k".into()],
            vec![vec![SpinValue::Int(1)], vec![SpinValue::Int(2)]],
        );
        let mut s = ex.query("SELECT k", &BindMap::new()).unwrap();
        assert_eq!(s.fetch_many(2).len(), 2);
    }

    #[test]
    fn should_reset_stream_for_cursor_reopen() {
        let ex = FakeExecutor::with_rows(vec!["a".into()], vec![vec![SpinValue::Null]]);
        let mut s = ex.query("SELECT 1", &BindMap::new()).unwrap();
        s.fetch_many(2);
        s.reset();
        assert_eq!(s.fetch_many(2).len(), 1);
    }

    #[test]
    fn should_return_empty_stream_when_script_exhausted() {
        let ex = FakeExecutor::default();
        let mut s = ex.query("SELECT 1", &BindMap::new()).unwrap();
        assert_eq!(s.fetch_many(2).len(), 0);
    }
}
