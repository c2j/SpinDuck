//! 参数表点查缓存。
//!
//! 第一次碰到认得出的 SELECT 时，按日期区间和字面量等值让 DuckDB 扫一遍，
//! 行按等值键放进进程内哈希表；之后每行只做探测。`SELECT INTO` 的 0 行和多行
//! 仍由调用方的 `fetch_many(2)` 判定。结果列名跟 SELECT 列表走，游标按名取字段。
//!
//! 刷新点只有两处。写入都走当前连接的 [`SqlExecutor::execute`]，调用内没有
//! 第二写者：顶层 `call` 入口清空；`execute` 的标识符命中已缓存表时丢掉该表
//! 的索引，下一次查询重建，因此能看见本事务刚写入的行。认不出、构建失败、
//! 或 `LIMIT` 在排序键上并列时，原样下发，不改变结果。

mod probe;
pub(crate) mod shape;

use crate::error::EngineError;
use crate::exec::{BindMap, ExecOutcome, Row, RowStream, SqlExecutor};
use crate::value::SpinValue;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::Token;
use shape::{Kind, Plan, ProbeVal, RowPlan, SemiPlan};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "native"))]
mod tests_duck;
#[cfg(test)]
mod tests_invalidate;

/// 调用级缓存。`Engine::call` 入口 [`LookupState::clear`]。
#[derive(Debug, Default)]
pub struct LookupState {
    slots: RefCell<HashMap<CacheId, Slot>>,
}

impl LookupState {
    pub fn clear(&self) {
        self.slots.borrow_mut().clear();
    }

    /// 事务回滚失效（P0-1 审核）：只丢数据承载的 `Slot::Ready`，
    /// 保留 `Slot::Off` 降级标记。`Off` 槽位本就不服务数据（恒
    /// fallback 到 `inner.query`），跨回滚保留它既正确又避免回滚后
    /// 重复执行必然失败的 build SQL——那条失败语句会把事务打入
    /// aborted 态，改变后续语句成败序列（`set_oriented_dummy`
    /// 视图 CAST 场景实测）。
    pub(crate) fn invalidate_on_rollback(&self) {
        self.slots
            .borrow_mut()
            .retain(|_, slot| matches!(slot, Slot::Off { .. }));
    }
}

/// 包在执行器外层。不改 [`SqlExecutor`] 签名。
///
/// `inner`/`state` 均为 `Rc` 共享：`Engine` 除把本执行器装进 `ex` 链外，
/// 另持一份克隆句柄供 loop-parallel 的 `try_prewarm_only` /
/// `export_frozen` 使用（两者必须落在**同一条**底层连接与缓存状态上）。
#[derive(Clone)]
pub struct LookupExecutor {
    inner: Rc<dyn SqlExecutor>,
    state: Rc<LookupState>,
}

impl LookupExecutor {
    pub fn new(inner: Rc<dyn SqlExecutor>, state: Rc<LookupState>) -> Self {
        Self { inner, state }
    }
}

impl SqlExecutor for LookupExecutor {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let Some(plan) = shape::recognize(sql) else {
            return self.inner.query(sql, binds);
        };
        let Some(range) = range_key(&plan, binds) else {
            return self.inner.query(sql, binds);
        };
        if range.iter().any(|part| matches!(part, KeyPart::Null)) {
            return Ok(empty_stream(sql, plan.proj_len()));
        }
        let id = CacheId {
            sql: sql.to_string(),
            range,
        };
        if self.state.slots.borrow().contains_key(&id) {
            return self.answer(&id, sql, binds);
        }
        match self.build(&plan, binds) {
            Ok(index) => {
                let names = output_schema(sql, plan.proj_len());
                self.state.slots.borrow_mut().insert(
                    id.clone(),
                    Slot::Ready {
                        frozen: Arc::new(FrozenSlot { plan, index, names }),
                    },
                );
                self.answer(&id, sql, binds)
            }
            Err(_) => {
                let tables = plan.tables.clone();
                self.state
                    .slots
                    .borrow_mut()
                    .insert(id.clone(), Slot::Off { tables });
                self.inner.query(sql, binds)
            }
        }
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        // 事务控制语句不产生表名，`invalidate` 的表名提取对其天然
        // 失效——回滚改变可见数据，必须按回滚口径失效 Ready 数据槽
        // （P0-1 审核：否则回滚后读到回滚前的缓存值）。
        if crate::exec::is_rollback_stmt(sql) {
            self.state.invalidate_on_rollback();
        } else {
            self.invalidate(sql);
        }
        self.inner.execute(sql, binds)
    }
}

impl LookupExecutor {
    fn answer(&self, id: &CacheId, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let outcome = {
            let slots = self.state.slots.borrow();
            match slots.get(id) {
                Some(Slot::Ready { frozen }) => (
                    probe::probe(&frozen.plan, &frozen.index, binds),
                    frozen.names.clone(),
                ),
                _ => (probe::ProbeOut::Fallback, Vec::new()),
            }
        };
        match outcome {
            (probe::ProbeOut::Rows(rows), names) => Ok(RowStream::new(names, rows)),
            (probe::ProbeOut::Fallback, _) => self.inner.query(sql, binds),
        }
    }

    fn build(&self, plan: &Plan, binds: &BindMap) -> Result<Index, EngineError> {
        let mut streams = Vec::new();
        for build in &plan.builds {
            let bound = bind_subset(binds, &build.binds)?;
            let mut stream = self.inner.query(&build.sql, &bound)?;
            let rows = stream.fetch_many(stream.len());
            if rows.iter().any(|row| row.len() < build.width) {
                return Err(EngineError::invariant(65, "lookup build row is short"));
            }
            streams.push(rows);
        }
        Index::from_streams(plan, streams)
    }

    fn invalidate(&self, sql: &str) {
        if self.state.slots.borrow().is_empty() {
            return;
        }
        let Some(names) = mentioned(sql) else {
            self.state.clear();
            return;
        };
        self.state.slots.borrow_mut().retain(|_, slot| {
            let tables = match slot {
                Slot::Ready { frozen } => &frozen.plan.tables,
                Slot::Off { tables } => tables,
            };
            !tables.iter().any(|table| names.contains(table))
        });
    }

    /// 三步安全预热协议的 Step B（loop-parallel 设计 §5.1）：只建冻结槽位，
    /// 绝不写 `Slot::Off`、绝不兜底执行 `inner.query` 原语句。
    ///
    /// - `shape::recognize` / `range_key` 在内存中判定，失败返回
    ///   [`PrewarmOutcome::UnsupportedShapeNoSql`]（零 SQL 下发）；
    /// - 槽位已存在或 range 含 NULL（空结果语义无需建槽）返回 `Frozen`；
    /// - 建索引 SQL 报错返回 [`PrewarmOutcome::SqlFailed`]，由调用方
    ///   `reset_session` + `lookup.clear()` 后退回串行（R3-P1-1）。
    pub(crate) fn try_prewarm_only(&self, sql: &str, binds: &BindMap) -> PrewarmOutcome {
        let Some(plan) = shape::recognize(sql) else {
            return PrewarmOutcome::UnsupportedShapeNoSql;
        };
        let Some(range) = range_key(&plan, binds) else {
            return PrewarmOutcome::UnsupportedShapeNoSql;
        };
        if range.iter().any(|part| matches!(part, KeyPart::Null)) {
            return PrewarmOutcome::Frozen;
        }
        let id = CacheId {
            sql: sql.to_string(),
            range,
        };
        if self.state.slots.borrow().contains_key(&id) {
            return PrewarmOutcome::Frozen;
        }
        match self.build(&plan, binds) {
            Ok(index) => {
                let names = output_schema(sql, plan.proj_len());
                self.state.slots.borrow_mut().insert(
                    id,
                    Slot::Ready {
                        frozen: Arc::new(FrozenSlot { plan, index, names }),
                    },
                );
                PrewarmOutcome::Frozen
            }
            Err(_) => PrewarmOutcome::SqlFailed,
        }
    }

    /// 导出只读快照（§5.2）：仅克隆 `Arc` 指针，零负载拷贝。
    /// 快照有效性由调用方按"洁净入口 + 无 reset_session/TxRollback"
    /// 双条件保证；live 侧失效不影响已导出快照。
    pub(crate) fn export_frozen(&self) -> FrozenLookupSnapshot {
        FrozenLookupSnapshot {
            slots: self
                .state
                .slots
                .borrow()
                .iter()
                .filter_map(|(id, slot)| match slot {
                    Slot::Ready { frozen } => Some((id.clone(), frozen.clone())),
                    Slot::Off { .. } => None,
                })
                .collect(),
        }
    }
}

/// `try_prewarm_only` 的三态结果（loop-parallel 设计 §5.1）。
#[derive(Debug)]
pub(crate) enum PrewarmOutcome {
    /// 已有缓存或成功建出 `Arc<FrozenSlot>`。
    Frozen,
    /// `shape::recognize` / `range_key` 内存判定不支持——零 SQL 下发。
    UnsupportedShapeNoSql,
    /// 建索引 SQL 执行失败（未写 `Slot::Off`、未兜底原查询；
    /// 错误按设计 §5.1 显式吞掉——不得以预热错误替换例程原错误，
    /// 调用方统一退回串行）。
    SqlFailed,
}

/// `Slot::Ready` 的只读负载（§5.2 零拷贝冻结）：`Arc` 共享给 Worker。
#[derive(Debug)]
pub(crate) struct FrozenSlot {
    pub plan: Plan,
    pub index: Index,
    /// 建槽时解析一次的输出列名。
    pub names: Vec<String>,
}

/// Worker 侧只读快照：`CacheId -> Arc<FrozenSlot>` 的时间点克隆。
/// `probe` 只读不写、零 SQL；未命中/探测回退返回 `None` 交权主线程。
#[derive(Debug, Default, Clone)]
pub(crate) struct FrozenLookupSnapshot {
    slots: HashMap<CacheId, Arc<FrozenSlot>>,
}

impl FrozenLookupSnapshot {
    /// Worker 侧快照体积估算（LP 报告 `peak_snapshot_bytes`；
    /// 估算口径见 [`Index::approx_bytes`]）。
    pub(crate) fn approx_bytes(&self) -> usize {
        self.slots
            .values()
            .map(|frozen| {
                frozen.index.approx_bytes()
                    + frozen.names.iter().map(|n| n.len() + 16).sum::<usize>()
                    + frozen
                        .plan
                        .tables
                        .iter()
                        .map(|t| t.len() + 16)
                        .sum::<usize>()
            })
            .sum()
    }

    /// 与 [`LookupExecutor::query`] 同口径的命中判定，但只查冻结槽位：
    /// `None` = 形状不支持 / 未预热 / 探测回退（Worker 必须交权）。
    pub(crate) fn probe(&self, sql: &str, binds: &BindMap) -> Option<RowStream> {
        let plan = shape::recognize(sql)?;
        let range = range_key(&plan, binds)?;
        if range.iter().any(|part| matches!(part, KeyPart::Null)) {
            return Some(empty_stream(sql, plan.proj_len()));
        }
        let id = CacheId {
            sql: sql.to_string(),
            range,
        };
        let frozen = self.slots.get(&id)?;
        match probe::probe(&frozen.plan, &frozen.index, binds) {
            probe::ProbeOut::Rows(rows) => Some(RowStream::new(frozen.names.clone(), rows)),
            probe::ProbeOut::Fallback => None,
        }
    }
}

#[derive(Debug)]
enum Slot {
    Ready { frozen: Arc<FrozenSlot> },
    Off { tables: Vec<String> },
}

#[derive(Debug)]
pub(crate) enum Index {
    Rows(HashMap<Vec<KeyPart>, Vec<Row>>),
    Semi {
        outer: HashMap<Vec<KeyPart>, Vec<Row>>,
        inner: HashMap<Vec<KeyPart>, HashSet<KeyPart>>,
    },
}

impl Index {
    fn from_streams(plan: &Plan, streams: Vec<Vec<Row>>) -> Result<Self, EngineError> {
        let index = match &plan.kind {
            Kind::Rows(rows) => streams
                .first()
                .map(|rows_in| Index::Rows(bucket_rows(rows_in, &rows.key_at))),
            Kind::Semi(semi) => {
                streams
                    .first()
                    .zip(streams.get(1))
                    .map(|(outer, inner)| Index::Semi {
                        outer: bucket_rows(outer, &semi.outer_key_at),
                        inner: bucket_set(inner, &semi.inner_key_at, semi.inner_match_at),
                    })
            }
        };
        index.ok_or_else(|| EngineError::invariant(67, "lookup build stream count mismatch"))
    }

    /// 快照体积估算（LP 报告 `peak_snapshot_bytes`）：单元格数 ×
    /// `SpinValue` 粒度 + 行/键固定开销。堆布局估算而非精确字节，
    /// 仅供容量观测，不做正确性判据。
    fn approx_bytes(&self) -> usize {
        const CELL: usize = std::mem::size_of::<SpinValue>();
        const ROW_OVERHEAD: usize = 48;
        const KEY_OVERHEAD: usize = 32;
        let bucket = |k: &[KeyPart], rows: &[Row]| {
            KEY_OVERHEAD
                + std::mem::size_of_val(k)
                + rows.len() * ROW_OVERHEAD
                + rows.iter().map(|r| r.len()).sum::<usize>() * CELL
        };
        match self {
            Index::Rows(map) => map.iter().map(|(k, rows)| bucket(k, rows)).sum(),
            Index::Semi { outer, inner } => {
                outer.iter().map(|(k, rows)| bucket(k, rows)).sum::<usize>()
                    + inner
                        .iter()
                        .map(|(k, set)| {
                            KEY_OVERHEAD
                                + std::mem::size_of_val(k)
                                + set
                                    .iter()
                                    .map(|_| std::mem::size_of::<KeyPart>())
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            }
        }
    }
}

fn bucket_rows(rows: &[Row], key_at: &[usize]) -> HashMap<Vec<KeyPart>, Vec<Row>> {
    let mut map = HashMap::new();
    for row in rows {
        let Some(key) = row_key(row, key_at) else {
            continue;
        };
        map.entry(key).or_insert_with(Vec::new).push(row.clone());
    }
    map
}

fn bucket_set(
    rows: &[Row],
    key_at: &[usize],
    match_at: usize,
) -> HashMap<Vec<KeyPart>, HashSet<KeyPart>> {
    let mut map = HashMap::new();
    for row in rows {
        let Some(key) = row_key(row, key_at) else {
            continue;
        };
        let Some(member) = row.get(match_at).and_then(KeyPart::from_value) else {
            continue;
        };
        if matches!(member, KeyPart::Null) {
            continue;
        }
        map.entry(key).or_insert_with(HashSet::new).insert(member);
    }
    map
}

/// 等值键里出现 NULL 的行永远配不上 `col = $bind`，建索引时跳过。
fn row_key(row: &Row, key_at: &[usize]) -> Option<Vec<KeyPart>> {
    let mut key = Vec::with_capacity(key_at.len());
    for at in key_at {
        let part = KeyPart::from_value(row.get(*at)?)?;
        if matches!(part, KeyPart::Null) {
            return None;
        }
        key.push(part);
    }
    Some(key)
}

fn range_key(plan: &Plan, binds: &BindMap) -> Option<Vec<KeyPart>> {
    plan.range_binds()
        .iter()
        .map(|name| KeyPart::from_value(binds.get(name)?))
        .collect()
}

fn bind_subset(binds: &BindMap, names: &[String]) -> Result<BindMap, EngineError> {
    let mut out = BindMap::new();
    for name in names {
        let value = binds.get(name).cloned().ok_or_else(|| {
            EngineError::invariant(66, format!("lookup build missing bind {name}"))
        })?;
        out.insert(name.clone(), value);
    }
    Ok(out)
}

fn mentioned(sql: &str) -> Option<HashSet<String>> {
    // 无分号 INSERT 只失效写表，免去万行合并 VALUES 的全串分词；
    // INSERT … SELECT 读到的表不被写入，不再过度失效。
    if let Some(target) = insert_target(sql) {
        return Some(HashSet::from([target]));
    }
    let tokens = Tokenizer::new(sql).tokenize().ok()?;
    Some(
        tokens
            .iter()
            .filter_map(|tok| match &tok.token {
                Token::Ident(name) | Token::QuotedIdent(name) => Some(name.to_ascii_lowercase()),
                _ => None,
            })
            .collect(),
    )
}

/// 无分号 INSERT 的写目标（尾段、小写）；其余形态回落全串分词。
fn insert_target(sql: &str) -> Option<String> {
    // 连字面量内的分号也保守回落，避免把多语句后续写入漏出失效集。
    if sql.as_bytes().contains(&b';') {
        return None;
    }
    let t = crate::exec::strip_leading_comments(sql);
    let t = keyword(t, "insert")?;
    let t = keyword(t, "into")?;
    let (last, rest) = qualified_ident(t.trim_start())?;
    match rest.chars().next() {
        None | Some(' ' | '\t' | '\n' | '\r' | '(' | ';') => Some(last.to_ascii_lowercase()),
        _ => None,
    }
}

/// 大小写不敏感关键字（后随空白才算词边界），返回剩余文本。
fn keyword<'a>(t: &'a str, kw: &str) -> Option<&'a str> {
    let t = t.trim_start();
    let head = t.get(..kw.len())?;
    if !head.eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = &t[kw.len()..];
    rest.starts_with(|c: char| c.is_ascii_whitespace())
        .then_some(rest)
}

/// 限定标识符（a.b."C"）：返回（尾段内容, 剩余文本）。引号内的点是名字的一部分。
fn qualified_ident(t: &str) -> Option<(String, &str)> {
    let mut rest = t;
    loop {
        let (seg, r) = ident_segment(rest)?;
        rest = r;
        match rest.strip_prefix('.') {
            Some(r) => rest = r,
            None => return Some((seg, rest)),
        }
    }
}

fn ident_segment(t: &str) -> Option<(String, &str)> {
    if let Some(r) = t.strip_prefix('"') {
        let end = r.find('"')?;
        // "" 转义引号形态少见：保守回落全串分词
        if r[end + 1..].starts_with('"') {
            return None;
        }
        return Some((r[..end].to_string(), &r[end + 1..]));
    }
    let end = t
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(t.len());
    if end == 0 {
        return None;
    }
    Some((t[..end].to_string(), &t[end..]))
}

fn empty_stream(sql: &str, proj_len: usize) -> RowStream {
    RowStream::new(output_schema(sql, proj_len), Vec::new())
}

fn output_schema(sql: &str, proj_len: usize) -> Vec<String> {
    match shape::sqlgen::output_names(sql) {
        Some(names) if names.len() == proj_len => names,
        _ => schema_len(proj_len),
    }
}

fn schema_len(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("c{i}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheId {
    sql: String,
    range: Vec<KeyPart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum KeyPart {
    Null,
    Bool(bool),
    Int(i64),
    Dec(i128, u8, u8),
    Str(String),
    Blob(Vec<u8>),
}

impl KeyPart {
    fn from_value(v: &SpinValue) -> Option<Self> {
        match v {
            SpinValue::Null => Some(Self::Null),
            SpinValue::Bool(b) => Some(Self::Bool(*b)),
            SpinValue::Int(i) => Some(Self::Int(*i)),
            SpinValue::Decimal(u, p, s) => Some(Self::Dec(*u, *p, *s)),
            SpinValue::Str(s) => Some(Self::Str(s.clone())),
            SpinValue::Blob(b) => Some(Self::Blob(b.clone())),
            SpinValue::Record(_) => None,
        }
    }
}
