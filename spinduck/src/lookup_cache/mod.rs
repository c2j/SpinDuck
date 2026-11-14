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
mod shape;

use crate::error::EngineError;
use crate::exec::{BindMap, ExecOutcome, Row, RowStream, SqlExecutor};
use crate::value::SpinValue;
use ogsql_parser::token::tokenizer::Tokenizer;
use ogsql_parser::token::Token;
use shape::{Kind, Plan, ProbeVal, RowPlan, SemiPlan};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

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
}

/// 包在执行器外层。不改 [`SqlExecutor`] 签名。
pub struct LookupExecutor {
    inner: Box<dyn SqlExecutor>,
    state: Rc<LookupState>,
}

impl LookupExecutor {
    pub fn new(inner: Box<dyn SqlExecutor>, state: Rc<LookupState>) -> Self {
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
                        plan: Box::new(plan),
                        index: Box::new(index),
                        names,
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
        self.invalidate(sql);
        self.inner.execute(sql, binds)
    }
}

impl LookupExecutor {
    fn answer(&self, id: &CacheId, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let outcome = {
            let slots = self.state.slots.borrow();
            match slots.get(id) {
                Some(Slot::Ready { plan, index, names }) => {
                    (probe::probe(plan, index, binds), names.clone())
                }
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
                Slot::Ready { plan, .. } => &plan.tables,
                Slot::Off { tables } => tables,
            };
            !tables.iter().any(|table| names.contains(table))
        });
    }
}

#[derive(Debug)]
enum Slot {
    Ready {
        plan: Box<Plan>,
        index: Box<Index>,
        /// 建槽时解析一次的输出列名（命中路径不再重解析 SQL）
        names: Vec<String>,
    },
    Off {
        tables: Vec<String>,
    },
}

#[derive(Debug)]
pub(super) enum Index {
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
