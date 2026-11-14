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
//!
//! 重入约束（复核小项）：`flush` / `push` 持有 `Rc<RefCell<WriteBuffer>>` 的
//! `borrow_mut` 期间调用 `ex.execute(&dyn SqlExecutor)`——当前无再入路径
//! （嵌套例程各持独立缓冲，物化由 `sqlstmts` 而非 executor 触发）；若将来
//! executor 反向进入本模块，double-borrow 将 panic，任何改动须先复核本条。

use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor, TraceEvent, TraceSink};
use crate::runtime::interp::dynamic::sql_literal;
use crate::runtime::interp::Env;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

/// 缓冲行数上限：超过即刷盘（内存上界）。
const WRITE_BUF_CAP: usize = 10_000;

pub(in crate::runtime) type WBufOpt = Option<Rc<RefCell<WriteBuffer>>>;

struct BufGroup {
    /// "insert into <表> (列清单)" 前缀（分组键，逐字一致才合并）
    prefix: String,
    /// 每行的字面量元组 "( 'a' , 1 )"
    rows: Vec<String>,
}

/// 顶层调用的延迟写缓冲（嵌套例程各自持有独立缓冲，例程边界即刷盘点）。
pub(in crate::runtime) struct WriteBuffer {
    groups: Vec<BufGroup>,
    buffered: usize,
    /// 攒批行数上限（满则先刷盘，第四轮复核应修 ⑤：测试可注入小上限）
    cap: usize,
    /// 已缓冲表名 token（小写，`schema.table` 全名 + 尾段各一份）：push 时
    /// 增量维护，flush 后按剩余组重建——读自己写判定不再逐条重解析前缀。
    tables: HashSet<String>,
}

impl WriteBuffer {
    /// 按当前剩余组重建表名 token 集（flush 出账后调用：成功路径组已空 →
    /// 集清空；错误路径仅剩余组保留 → 集随之收缩，不误删仍缓冲的表）。
    fn rebuild_tables(&mut self) {
        self.tables.clear();
        for g in &self.groups {
            for t in table_tokens(&g.prefix) {
                self.tables.insert(t);
            }
        }
    }

    /// 攒一条 INSERT；缓冲满则先刷盘。
    fn push(
        &mut self,
        prefix: String,
        values_part: &str,
        binds: &BindMap,
        ex: &dyn SqlExecutor,
        sink: &mut dyn TraceSink,
    ) -> Result<(), EngineError> {
        if self.buffered >= self.cap {
            self.flush(ex, sink)?;
        }
        let row = literalize_values(values_part, binds)?;
        // 同前缀连续攒入（游标循环形态），保持语句顺序
        let is_last_same = self
            .groups
            .last()
            .map(|g| g.prefix == prefix)
            .unwrap_or(false);
        if !is_last_same {
            // 新组：前缀解析一次，表名 token 增量入集（同前缀续攒无需重解析）
            for t in table_tokens(&prefix) {
                self.tables.insert(t);
            }
            self.groups.push(BufGroup {
                prefix,
                rows: Vec::new(),
            });
        }
        // 刚保证非空，按索引取尾组（省去不可达的 None 分支，复核小项）
        let last = self.groups.len() - 1;
        self.groups[last].rows.push(row);
        self.buffered += 1;
        Ok(())
    }

    /// 刷盘：按索引逐组消费（不再 `remove(0)`——交替前缀可攒出上万组，
    /// 头删是 O(N²)），同前缀合并为一条多值 INSERT；失败回退逐行重放。
    /// 组内首行失败视为表级错误上抛：出账已处理组 `[0..=i]`，**剩余组保留**
    /// （错误若被 PL 处理器捕获，剩余组须在例程出口刷盘，不得随错误丢弃）。
    /// 每条实际下发的语句（含回放行）补发 `SqlExec` 事件（复核应修 B：
    /// OFF 路径每条语句一事件，ON 路径不能在 JSONL 面上消失）。
    fn flush(&mut self, ex: &dyn SqlExecutor, sink: &mut dyn TraceSink) -> Result<(), EngineError> {
        let mut fail: Option<(usize, EngineError)> = None;
        for (i, g) in self.groups.iter().enumerate() {
            let merged = format!("{} VALUES {}", g.prefix, g.rows.join(", "));
            self.buffered -= g.rows.len();
            let result = execute_traced(ex, sink, &merged)
                .or_else(|first_err| replay_group(g, ex, sink, first_err));
            if let Err(e) = result {
                fail = Some((i, e));
                break;
            }
        }
        let result = match fail {
            // 出账已处理组（含失败组），剩余组留存待出口刷盘
            Some((idx, e)) => {
                self.groups.drain(..=idx);
                Err(e)
            }
            None => {
                self.groups.clear();
                Ok(())
            }
        };
        self.rebuild_tables();
        result
    }

    fn flush_discard(&mut self) {
        self.groups.clear();
        self.tables.clear();
        self.buffered = 0;
    }
}

fn execute_traced(
    ex: &dyn SqlExecutor,
    sink: &mut dyn TraceSink,
    sql: &str,
) -> Result<(), EngineError> {
    if sink.is_enabled() {
        sink.on_event(TraceEvent::SqlExec {
            sql: sql.to_string(),
            binds: BindMap::new(),
            elapsed_us: 0,
        });
    }
    ex.execute(sql, &BindMap::new()).map(|_| ())
}

fn replay_group(
    group: &BufGroup,
    ex: &dyn SqlExecutor,
    sink: &mut dyn TraceSink,
    first_err: EngineError,
) -> Result<(), EngineError> {
    let mut skipped = 0usize;
    let mut skip_reason = String::new();
    for (i, row) in group.rows.iter().enumerate() {
        let stmt = format!("{} VALUES {}", group.prefix, row);
        let Err(row_err) = execute_traced(ex, sink, &stmt) else {
            continue;
        };
        if i == 0 {
            return Err(first_err);
        }
        skipped += 1;
        if skip_reason.is_empty() {
            skip_reason = format!("{} {}", row_err.code, row_err.message);
        }
    }
    if skipped > 0 && sink.is_enabled() {
        sink.on_event(TraceEvent::BatchSkip {
            rows: skipped,
            first_error: skip_reason,
        });
    }
    Ok(())
}

/// 从 `insert into <表> (列清单)` 前缀提取表名 token（小写）：全名 + 尾段。
/// `INSERT INTO t(a, …)`（表名与列清单无空格，golden case_split_agg 即此写法）
/// 按 '(' 截掉列清单（复核应修 3，原取到 "t(a," 永不命中）；限定名 `sch.t`
/// 全名与尾段都要能命中（V24 评审应修 9：touches 侧分词口径）。
fn table_tokens(prefix: &str) -> Vec<String> {
    let Some(t) = prefix.split_whitespace().nth(2) else {
        return Vec::new();
    };
    let t = t
        .split('(')
        .next()
        .unwrap_or(t)
        .trim_end_matches('(')
        .trim_matches('"')
        .to_ascii_lowercase();
    let mut out = vec![t.clone()];
    if let Some((_, last)) = t.rsplit_once('.') {
        out.push(last.to_string());
    }
    out
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
        // 字符串字面量整体复制（'' 为转义引号）：内部 `$` 不是占位符
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        out.push_str("''");
                        i += 2;
                        continue;
                    }
                    out.push('\'');
                    i += 1;
                    break;
                }
                let Some(ch) = values_part[i..].chars().next() else {
                    break;
                };
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }
        if bytes[i] != b'$' {
            let Some(ch) = values_part[i..].chars().next() else {
                break;
            };
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
    // 按 char 取前缀（复核小项：字节切片遇非 ASCII 前导即 panic，同 is_ddl）
    let head: String = sql.trim_start().chars().take(12).collect::<String>();
    head.eq_ignore_ascii_case("insert into ")
}

/// SQL 文本的标识符 token 集（小写），用于「是否触及缓冲表」判定。
fn touches_buffered_table(sql: &str, tables: &HashSet<String>) -> bool {
    if tables.is_empty() {
        return false;
    }
    // 标识符字符集含 '.' 与 '"'：限定名 `sch.t` 成单 token 参与匹配
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '"'))
        .map(|t| t.trim_matches('"').to_ascii_lowercase())
        .any(|t| tables.contains(&t))
}

/// 开关 ON 时：INSERT 攒缓冲（返回 true = 已缓冲，调用方跳过即时下发）；
/// 其余语句先按表命中刷盘（读自己写），返回 false 走原路径。
///
/// fn_sites 非空：仅「非嵌套 INSERT VALUES 单值」形态可内联攒批——push 时
/// 求值例程、把缓存子查询替换为字面量（见 `fnsites_batch`），消除对 sd_fnN
/// 临时表的依赖后再攒批；嵌套下沉 / INSERT … SELECT 回落即时下发，由
/// `sqlstmts::exec` 的 `materialize_fn_sites` 物化（语义一字不改）。
pub(in crate::runtime) fn take_insert(
    env: &mut Env<'_>,
    sql: &str,
    binds_src: &[(String, crate::ir::BindSrc)],
    fn_sites: &[crate::sql::FnSite],
) -> Result<bool, EngineError> {
    let Some(rc) = env.wbuf.clone() else {
        return Ok(false);
    };
    if !is_insert(sql) {
        flush_if_touches(env, sql)?;
        return Ok(false);
    }
    // fn_sites 非空：先按目标表刷盘（读自己写），再尝试内联；不可内联回落即时。
    let inlined;
    let sql = if fn_sites.is_empty() {
        sql
    } else {
        flush_if_touches(env, sql)?;
        match super::fnsites_batch::inline_fn_sites(env, sql, fn_sites)? {
            Some(s) => {
                inlined = s;
                inlined.as_str()
            }
            None => return Ok(false),
        }
    };
    // 可缓冲判定前置：只有「INSERT INTO 表 (列) VALUES (标量列表)」形态才攒批；
    // INSERT … SELECT 等不可字面量化的形态回落即时下发（V24 评审阻塞项 2——
    // 该形态是形态覆盖门禁已断言的合法输入，不能当作错误）。
    let lower = sql.to_ascii_lowercase();
    let Some(vpos) = lower.find(" values") else {
        flush_if_touches(env, sql)?;
        return Ok(false);
    };
    let prefix = sql[..vpos].trim_end().to_string();
    let values_part = &sql[vpos + " values".len()..];
    // 仅此处（攒批路径）解析绑定源；OFF 路径不付解析成本（评审：双重解析）
    let resolved = crate::runtime::interp::resolve_binds(binds_src, env.frame)?;
    rc.borrow_mut()
        .push(prefix, values_part, &resolved, env.ex, env.sink)?;
    Ok(true)
}

/// 开关 ON 时：语句触及缓冲表则先刷盘（读自己写）。
pub(in crate::runtime) fn flush_if_touches(
    env: &mut Env<'_>,
    sql: &str,
) -> Result<(), EngineError> {
    let Some(rc) = env.wbuf.clone() else {
        return Ok(());
    };
    let hit = {
        let buf = rc.borrow();
        touches_buffered_table(sql, &buf.tables)
    };
    if hit {
        rc.borrow_mut().flush(env.ex, env.sink)?;
    }
    Ok(())
}

/// 开关 ON 时：无条件刷盘（例程出口 / 动态 SQL 前的保守点）。
pub(in crate::runtime) fn flush_all(env: &mut Env<'_>) -> Result<(), EngineError> {
    if let Some(rc) = env.wbuf.clone() {
        rc.borrow_mut().flush(env.ex, env.sink)?;
    }
    Ok(())
}

/// 放弃缓冲（ROLLBACK 路径：缓冲从未下发，直接丢弃）。
pub(in crate::runtime) fn discard(env: &mut Env<'_>) {
    if let Some(rc) = env.wbuf.clone() {
        rc.borrow_mut().flush_discard();
    }
}

/// 顶层调用入口构造缓冲（`Engine::batch_commit` = true 时为 Some）。
pub(in crate::runtime) fn shared_buffer() -> WBufOpt {
    Some(Rc::new(RefCell::new(WriteBuffer {
        groups: Vec::new(),
        buffered: 0,
        tables: HashSet::new(),
        cap: WRITE_BUF_CAP,
    })))
}

/// 测试专用：注入小上限以锁定「满则先刷盘」语义（复核应修 ⑤；
/// 生产入口 `shared_buffer` 恒为 WRITE_BUF_CAP）。
#[cfg(test)]
pub(in crate::runtime) fn shared_buffer_with_cap(cap: usize) -> WBufOpt {
    Some(Rc::new(RefCell::new(WriteBuffer {
        groups: Vec::new(),
        buffered: 0,
        tables: HashSet::new(),
        cap,
    })))
}
