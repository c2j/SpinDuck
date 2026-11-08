//! 合并 COMMIT 开关（`Engine::batch_commit`，默认关）的写缓冲。
//!
//! 开关 ON 的语义边界（与 §3.5「COMMIT 即时生效」的显式偏离，opt-in）：
//! ① 过程内 COMMIT 被吞并，INSERT 攒批合并为多值语句，持久化延迟到调用出口
//!    的统一 COMMIT（Engine::call 的最外层边界不受影响）；
//! ② 读自己写（同表 SELECT INTO / DELETE / UPDATE）在语句前刷盘，事务内可见性不变；
//! ③ 合并语句失败时回退逐行重放，失败行按「异常行跳过」的数据语义丢弃——
//!    与 OFF 模式的差异是该行不再进入 PL 异常处理器（inst_log 缺失），
//!    且组内首行失败视为表级错误上抛；
//! ④ 嵌套例程边界即刷盘点（嵌套例程持有自己的缓冲）。

use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor};
use crate::runtime::interp::dynamic::sql_literal;
use std::cell::RefCell;
use std::rc::Rc;

/// 缓冲行数上限：超过即刷盘（内存上界）。
const WRITE_BUF_CAP: usize = 10_000;

pub(super) type WBufOpt = Option<Rc<RefCell<WriteBuffer>>>;

struct BufGroup {
    /// "insert into <表> (列清单)" 前缀（分组键，逐字一致才合并）
    prefix: String,
    /// 每行的字面量元组 "( 'a' , 1 )"
    rows: Vec<String>,
}

/// 顶层调用的延迟写缓冲（嵌套例程各自持有独立缓冲，例程边界即刷盘点）。
pub(super) struct WriteBuffer {
    groups: Vec<BufGroup>,
    buffered: usize,
}

impl WriteBuffer {
    /// 已缓冲表名（读自己写的刷盘判定）。
    fn tables(&self) -> Vec<String> {
        let mut out = Vec::new();
        for g in &self.groups {
            if let Some(t) = g.prefix.split_whitespace().nth(2) {
                out.push(t.trim_end_matches('(').to_string());
            }
        }
        out
    }

    /// 攒一条 INSERT；缓冲满则先刷盘。
    fn push(
        &mut self,
        sql: &str,
        binds: &BindMap,
        ex: &dyn SqlExecutor,
    ) -> Result<(), EngineError> {
        if self.buffered >= WRITE_BUF_CAP {
            self.flush(ex)?;
        }
        let lower = sql.to_ascii_lowercase();
        let Some(vpos) = lower.find(" values") else {
            return Err(EngineError::invariant(
                33,
                "batch_commit: INSERT 语句缺少 VALUES 段".to_string(),
            ));
        };
        let prefix = sql[..vpos].trim_end().to_string();
        let values_part = &sql[vpos + " values".len()..];
        let row = literalize_values(values_part, binds)?;
        let group = match self.groups.last_mut() {
            // 同前缀连续攒入（游标循环形态），保持语句顺序
            Some(g) if g.prefix == prefix => g,
            _ => {
                self.groups.push(BufGroup {
                    prefix,
                    rows: Vec::new(),
                });
                self.groups.last_mut().expect("just pushed")
            }
        };
        group.rows.push(row);
        self.buffered += 1;
        Ok(())
    }

    /// 刷盘：同前缀合并为一条多值 INSERT；失败回退逐行重放。
    fn flush(&mut self, ex: &dyn SqlExecutor) -> Result<(), EngineError> {
        for g in &self.groups {
            if g.rows.is_empty() {
                continue;
            }
            let merged = format!("{} VALUES {}", g.prefix, g.rows.join(", "));
            if let Err(first_err) = ex.execute(&merged, &BindMap::new()) {
                // 回退逐行：失败行按数据语义跳过；组内首行失败视为表级错误上抛
                let mut first_row = true;
                for row in &g.rows {
                    let stmt = format!("{} VALUES {}", g.prefix, row);
                    match ex.execute(&stmt, &BindMap::new()) {
                        Ok(_) => first_row = false,
                        Err(row_err) => {
                            let _ = row_err;
                            if first_row {
                                return Err(first_err);
                            }
                            first_row = false;
                        }
                    }
                }
            }
        }
        self.groups.clear();
        self.buffered = 0;
        Ok(())
    }

    fn flush_discard(&mut self) {
        self.groups.clear();
        self.buffered = 0;
    }
}

/// VALUES 段字面量化：把 `$ident` 占位符就地替换为对应字面量，其余文本
/// （运算符、函数包裹、字面量）原样保留——VALUES 表达式可以是含多个占位符
/// 的复合形态（如 `round(($a - $b) / $c, 2)`），按值逐个提取会拆散表达式
/// 导致列/值错位（V24 实测 22 列膨胀为 26 值）。
fn literalize_values(values_part: &str, binds: &BindMap) -> Result<String, EngineError> {
    let bytes = values_part.as_bytes();
    let mut out = String::with_capacity(values_part.len() + 32);
    let mut i = 0;
    let mut replaced = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let ch = values_part[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        let key = &values_part[i..j];
        let v = binds
            .get(key)
            .ok_or_else(|| EngineError::invariant(33, format!("batch_commit: 绑定缺失 {key}")))?;
        out.push_str(&sql_literal(v));
        replaced += 1;
        i = j;
    }
    if replaced == 0 {
        // 无占位符：VALUES 段本身即字面量元组（如 "( 'x' )"），原样采用
        let t = values_part.trim();
        if t.starts_with('(') && t.ends_with(')') {
            return Ok(t.to_string());
        }
        return Err(EngineError::invariant(
            33,
            "batch_commit: VALUES 段无占位符且非字面量元组".to_string(),
        ));
    }
    Ok(out)
}

/// INSERT 判定（改写产物为小写形态，大小写不敏感兜底）。
fn is_insert(sql: &str) -> bool {
    let t = sql.trim_start();
    t.len() >= 12 && t[..12].eq_ignore_ascii_case("insert into ")
}

/// SQL 文本的标识符 token 集（小写），用于「是否触及缓冲表」判定。
fn touches_buffered_table(sql: &str, tables: &[String]) -> bool {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|t| tables.iter().any(|bt| t.eq_ignore_ascii_case(bt)))
}

/// 开关 ON 时：INSERT 攒缓冲（返回 true = 已缓冲，调用方跳过即时下发）；
/// 其余语句先按表命中刷盘（读自己写），返回 false 走原路径。
pub(super) fn take_insert(
    ex: &dyn SqlExecutor,
    wbuf: &mut WBufOpt,
    sql: &str,
    binds: &BindMap,
) -> Result<bool, EngineError> {
    let Some(rc) = wbuf else {
        return Ok(false);
    };
    if !is_insert(sql) {
        let tables = rc.borrow().tables();
        if !tables.is_empty() && touches_buffered_table(sql, &tables) {
            rc.borrow_mut().flush(ex)?;
        }
        return Ok(false);
    }
    rc.borrow_mut().push(sql, binds, ex)?;
    Ok(true)
}

/// 开关 ON 时：语句触及缓冲表则先刷盘（读自己写）。
pub(super) fn flush_if_touches(
    ex: &dyn SqlExecutor,
    wbuf: &mut WBufOpt,
    sql: &str,
) -> Result<(), EngineError> {
    let Some(rc) = wbuf else {
        return Ok(());
    };
    let tables = rc.borrow().tables();
    if !tables.is_empty() && touches_buffered_table(sql, &tables) {
        rc.borrow_mut().flush(ex)?;
    }
    Ok(())
}

/// 开关 ON 时：无条件刷盘（例程出口 / 动态 SQL 前的保守点）。
pub(super) fn flush_all(ex: &dyn SqlExecutor, wbuf: &mut WBufOpt) -> Result<(), EngineError> {
    if let Some(rc) = wbuf {
        rc.borrow_mut().flush(ex)?;
    }
    Ok(())
}

/// 放弃缓冲（ROLLBACK 路径：缓冲从未下发，直接丢弃）。
pub(super) fn discard(wbuf: &mut WBufOpt) {
    if let Some(rc) = wbuf {
        rc.borrow_mut().flush_discard();
    }
}

/// 顶层调用入口构造缓冲（`Engine::batch_commit` = true 时为 Some）。
pub(super) fn shared_buffer() -> WBufOpt {
    Some(Rc::new(RefCell::new(WriteBuffer {
        groups: Vec::new(),
        buffered: 0,
    })))
}
