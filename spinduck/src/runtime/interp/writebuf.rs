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
}

impl WriteBuffer {
    /// 已缓冲表名（读自己写的刷盘判定）：保留 `schema.table` 全名与
    /// 去引号形态各一份——touches 侧分词会把 `sch.t` 切成两段，
    /// 全名与尾段都要能命中（V24 评审应修 9：限定名漏判 → 读到旧数据）。
    fn tables(&self) -> Vec<String> {
        let mut out = Vec::new();
        for g in &self.groups {
            let Some(t) = g.prefix.split_whitespace().nth(2) else {
                continue;
            };
            // `INSERT INTO t(a, …)`（表名与列清单无空格，golden case_split_agg
            // 即此写法）：按 '(' 截掉列清单（复核应修 3，原取到 "t(a," 永不命中）
            let t = t
                .split('(')
                .next()
                .unwrap_or(t)
                .trim_end_matches('(')
                .trim_matches('"')
                .to_string();
            out.push(t.clone());
            if let Some((_, last)) = t.rsplit_once('.') {
                out.push(last.to_string());
            }
        }
        out
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
        if self.buffered >= WRITE_BUF_CAP {
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

    /// 刷盘：逐组消费（每组处理完立即出账），同前缀合并为一条多值 INSERT；
    /// 失败回退逐行重放。任何时刻失败，已下发组不重复（V24 评审阻塞项 3）。
    /// 每条实际下发的语句（含回放行）补发 `SqlExec` 事件（复核应修 B：
    /// OFF 路径每条语句一事件，ON 路径不能在 JSONL 面上消失）。
    fn flush(&mut self, ex: &dyn SqlExecutor, sink: &mut dyn TraceSink) -> Result<(), EngineError> {
        let emit = |sink: &mut dyn TraceSink, sql: &str| {
            sink.on_event(TraceEvent::SqlExec {
                sql: sql.to_string(),
                binds: BindMap::new(),
                elapsed_us: 0,
            });
        };
        while !self.groups.is_empty() {
            let g = self.groups.remove(0);
            // push 保证每组入账即带行，空组不可达（复核文档 7：不再保留分支）
            let merged = format!("{} VALUES {}", g.prefix, g.rows.join(", "));
            emit(sink, &merged);
            if let Err(first_err) = ex.execute(&merged, &BindMap::new()) {
                // 回退逐行：失败行按数据语义跳过；组内首行失败视为表级错误上抛。
                // 上抛前先出账本组（已重放的行不重复），剩余组随外层错误丢弃。
                self.buffered -= g.rows.len();
                let mut first_row = true;
                let mut skipped = 0usize;
                let mut skip_reason = String::new();
                for row in &g.rows {
                    let stmt = format!("{} VALUES {}", g.prefix, row);
                    emit(sink, &stmt);
                    match ex.execute(&stmt, &BindMap::new()) {
                        Ok(_) => first_row = false,
                        Err(row_err) => {
                            if first_row {
                                return Err(first_err);
                            }
                            // 跳行留痕（复核应修 2）：不上抛但必须可观测
                            skipped += 1;
                            if skip_reason.is_empty() {
                                skip_reason = row_err.message.clone();
                            }
                            first_row = false;
                        }
                    }
                }
                if skipped > 0 {
                    sink.on_event(TraceEvent::BatchSkip {
                        rows: skipped,
                        first_error: skip_reason,
                    });
                }
            } else {
                self.buffered -= g.rows.len();
            }
        }
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
fn touches_buffered_table(sql: &str, tables: &[String]) -> bool {
    // 标识符字符集含 '.' 与 '"'：限定名 `sch.t` 成单 token 参与匹配
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '"'))
        .map(|t| t.trim_matches('"').to_ascii_lowercase())
        .any(|t| tables.iter().any(|bt| bt.eq_ignore_ascii_case(&t)))
}

/// 开关 ON 时：INSERT 攒缓冲（返回 true = 已缓冲，调用方跳过即时下发）；
/// 其余语句先按表命中刷盘（读自己写），返回 false 走原路径。
///
/// 显式准入条件（复核新增，与「INSERT … SELECT 回落」同一原则——缓冲只能
/// 替换 `ex.execute`，原路径的其余前置副作用一律不允许被略过）：
/// fn_sites 为空（非空则 `materialize_fn_sites` 必须在 sqlstmts::exec
/// 里执行，攒批会跳过它 → sd_fnN 不物化 / 滞留旧元组）。
pub(in crate::runtime) fn take_insert(
    env: &mut Env<'_>,
    sql: &str,
    binds_src: &[(String, crate::ir::BindSrc)],
    fn_sites: &[crate::sql::FnSite],
) -> Result<bool, EngineError> {
    let Some(rc) = env.wbuf.clone() else {
        return Ok(false);
    };
    // fn_sites 非空：下沉物化是 sqlstmts::exec 的执行期副作用，不可缓冲
    if !fn_sites.is_empty() {
        flush_if_touches(env, sql)?;
        return Ok(false);
    }
    if !is_insert(sql) {
        flush_if_touches(env, sql)?;
        return Ok(false);
    }
    // 可缓冲判定前置：只有「INSERT INTO 表 (列) VALUES (标量列表)」形态才攒批；
    // INSERT … SELECT 等不可字面量化的形态回落即时下发（V24 评审阻塞项 2——
    // 该形态是形态覆盖门禁已断言的合法输入，不能当作错误）。
    let lower = sql.to_ascii_lowercase();
    let Some(vpos) = lower.find(" values") else {
        // 不可缓冲形态（如 INSERT … SELECT）：回落即时下发（评审阻塞项 2——
        // 该形态是形态覆盖门禁已断言的合法输入，不能当作错误）。
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
    let tables = rc.borrow().tables();
    if !tables.is_empty() && touches_buffered_table(sql, &tables) {
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
    })))
}
