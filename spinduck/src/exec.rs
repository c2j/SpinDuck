//! SQL 执行接缝：引擎内除本文件的原生实现外，任何代码不得触碰 DuckDB API
//! （AGENTS.md §1）。`FakeExecutor` 供单元/集成测试注入。

use crate::error::EngineError;
use crate::value::SpinValue;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

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
    /// 已载入的行数（物化快照行数）。
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// 执行结果：DML 返回受影响行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOutcome {
    pub rows_affected: usize,
}

/// 纯标量函数白名单（仅这些名字的 FROM-less SELECT 记忆化）。
/// 判据从严：不在名单内的未知内建（可能非确定性，如 CURRENT_TIMESTAMP）
/// 一律照常下发。来源：改写规则映射的确定性方言函数（rewrite-rules.toml）。
const PURE_SCALAR_FNS: [&str; 5] = ["substring", "strftime", "strptime", "to_days", "date_diff"];

/// 记忆化条目：结果 schema + 全行集（RowStream 可重建的最小集）。
type MemoRows = (Vec<String>, Vec<Row>);

#[derive(Debug, Default)]
pub struct MemoCache {
    entries: std::cell::RefCell<HashMap<String, MemoRows>>,
}

impl MemoCache {
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }
}

/// 纯标量 SELECT（`SELECT fn(args)`，无 FROM）的执行期记忆化执行器。
///
/// 语料游标循环把同一表达式语句按行重复下发，而参数组合通常只有少数几种
/// （V24 计量：10k 行 78,400 次宏下发仅对应 4 种参数组合）。按
/// （SQL 文本, 绑定值）缓存命中即免去内核 parse/plan/execute 全程。
/// 表查询（含 FROM）与非白名单函数一律照常下发——不判断数据新鲜度，
/// 因为白名单内的表达式是绑定的纯函数，调用期内结果不依赖任何表状态。
pub struct MemoizingExecutor {
    inner: Box<dyn SqlExecutor>,
    cache: std::rc::Rc<MemoCache>,
    inner_calls: std::cell::Cell<u64>,
}

fn memo_key(sql: &str, binds: &BindMap) -> Option<String> {
    let t = sql.trim_start();
    let rest = t.strip_prefix("SELECT")?;
    let upper = t.to_ascii_uppercase();
    if upper.contains(" FROM ") || upper.contains(';') {
        return None;
    }
    let name = rest
        .trim()
        .split(|c: char| c == '(' || c.is_whitespace())
        .next()?
        .to_ascii_lowercase();
    if !PURE_SCALAR_FNS.contains(&name.as_str()) {
        return None;
    }
    Some(format!("{sql}\u{1}{binds:?}"))
}

impl MemoizingExecutor {
    pub fn new(inner: Box<dyn SqlExecutor>) -> Self {
        Self::with_cache(inner, std::rc::Rc::new(MemoCache::default()))
    }

    pub fn with_cache(inner: Box<dyn SqlExecutor>, cache: std::rc::Rc<MemoCache>) -> Self {
        Self {
            inner,
            cache,
            inner_calls: std::cell::Cell::new(0),
        }
    }

    pub fn shared_cache(&self) -> std::rc::Rc<MemoCache> {
        self.cache.clone()
    }

    pub fn clear_memo(&self) {
        self.cache.clear();
    }

    pub fn inner_calls(&self) -> u64 {
        self.inner_calls.get()
    }
}

impl SqlExecutor for MemoizingExecutor {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let Some(key) = memo_key(sql, binds) else {
            self.inner_calls.set(self.inner_calls.get() + 1);
            return self.inner.query(sql, binds);
        };
        if let Some((schema, rows)) = self.cache.entries.borrow().get(&key) {
            return Ok(RowStream::new(schema.clone(), rows.clone()));
        }
        self.inner_calls.set(self.inner_calls.get() + 1);
        let out = self.inner.query(sql, binds)?;
        self.cache
            .entries
            .borrow_mut()
            .insert(key, (out.schema.clone(), out.rows.clone()));
        Ok(out)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.inner_calls.set(self.inner_calls.get() + 1);
        self.inner.execute(sql, binds)
    }
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

/// 空埋点（引擎默认 sink；AGENTS §5：日志只经 TraceSink）。
pub struct TraceNoopSink;
impl TraceSink for TraceNoopSink {
    fn on_event(&mut self, _event: TraceEvent) {}
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

    struct CountingExec {
        queries: std::cell::Cell<u32>,
        value: SpinValue,
    }
    impl CountingExec {
        fn new(value: SpinValue) -> Self {
            Self {
                queries: std::cell::Cell::new(0),
                value,
            }
        }
    }
    impl SqlExecutor for CountingExec {
        fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
            self.queries.set(self.queries.get() + 1);
            Ok(RowStream::new(
                vec!["v".into()],
                vec![vec![self.value.clone()]],
            ))
        }
        fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
            self.queries.set(self.queries.get() + 1);
            Ok(ExecOutcome { rows_affected: 1 })
        }
    }

    fn binds(date: &str) -> BindMap {
        let mut b = BindMap::new();
        b.insert("$p0".to_string(), SpinValue::Str(date.to_string()));
        b
    }

    #[test]
    fn should_memoize_repeated_pure_scalar_select() {
        let ex = MemoizingExecutor::new(Box::new(CountingExec::new(SpinValue::Int(90))));
        let sql =
            "SELECT strftime(strptime($p0, '%Y%m%d') + to_days(CAST($p1 AS INTEGER)), '%Y%m%d')";
        let mut b = binds("20250715");
        b.insert("$p1".to_string(), SpinValue::Int(90));
        let v1 = ex
            .query(sql, &b)
            .unwrap()
            .fetch_many(1)
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .unwrap();
        let v2 = ex
            .query(sql, &b)
            .unwrap()
            .fetch_many(1)
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .unwrap();
        assert_eq!(v1, SpinValue::Int(90));
        assert_eq!(v2, SpinValue::Int(90));
        assert_eq!(ex.inner_calls(), 1);
    }

    #[test]
    fn should_memoize_per_bind_values() {
        let ex = MemoizingExecutor::new(Box::new(CountingExec::new(SpinValue::Int(1))));
        let sql = "SELECT SUBSTRING($p0, 1, 2)";
        let _ = ex.query(sql, &binds("20250715")).unwrap();
        let _ = ex.query(sql, &binds("20250716")).unwrap();
        assert_eq!(ex.inner_calls(), 2);
    }

    #[test]
    fn should_not_memoize_table_selects() {
        let ex = MemoizingExecutor::new(Box::new(CountingExec::new(SpinValue::Int(1))));
        let sql = "SELECT t.fund_code FROM par_fund_partner AS t WHERE t.partner_code = $p0";
        let _ = ex.query(sql, &binds("A0001")).unwrap();
        let _ = ex.query(sql, &binds("A0001")).unwrap();
        assert_eq!(ex.inner_calls(), 2);
    }

    #[test]
    fn should_not_memoize_nondeterministic_functions() {
        let ex = MemoizingExecutor::new(Box::new(CountingExec::new(SpinValue::Null)));
        let sql = "SELECT CURRENT_TIMESTAMP()";
        let _ = ex.query(sql, &binds("x")).unwrap();
        let _ = ex.query(sql, &binds("x")).unwrap();
        assert_eq!(ex.inner_calls(), 2);
    }

    #[test]
    fn should_not_memoize_execute_statements() {
        let ex = MemoizingExecutor::new(Box::new(CountingExec::new(SpinValue::Null)));
        let _ = ex.execute("UPDATE t SET a = $p0", &binds("1")).unwrap();
        let _ = ex.execute("UPDATE t SET a = $p0", &binds("1")).unwrap();
        assert_eq!(ex.inner_calls(), 2);
    }

    #[test]
    fn should_reexecute_after_memo_clear() {
        let ex = MemoizingExecutor::new(Box::new(CountingExec::new(SpinValue::Int(7))));
        let sql = "SELECT SUBSTRING($p0, 1, 2)";
        let _ = ex.query(sql, &binds("20250715")).unwrap();
        ex.clear_memo();
        let _ = ex.query(sql, &binds("20250715")).unwrap();
        assert_eq!(ex.inner_calls(), 2);
    }
}
