//! 审计日志（`--trace`）：事件流 → 逐行落盘，SQL 全文靠 sqlid 反查。
//!
//! 分层：`render` 是"事件 → 行"的纯函数（不碰内核、可单测），`sink` 是汇
//! （writer、级别门、行号 seq、sqlid → 全文的 sidecar），`align` 是"日志 → sqlid 直方图"
//! 的反向解析（`explain --trace` 的对齐视图）。`sql_id` 的归一口径与 `explain` 的
//! `one_line` 同源（都走 [`flat`]），因此日志行可与 IR 逐条对齐。

mod align;
mod ident;
mod render;
mod sink;
mod wallclock;

pub use align::{annotation, histogram, unmatched, Hit};
pub use ident::{call_id, session_id};
pub use render::{flat, head, level_allows, sql_id, BIND_CHARS, HEAD_CHARS};
pub use render::{render_jsonl, render_text};
pub use sink::{AuditSink, SinkCfg};
pub use wallclock::utc_millis;

/// 日志形态（`--trace-format`）：默认 text（人读、可 tail -f），可选 jsonl（§11.2 契约，机器消费）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TraceFormat {
    /// 默认形态：人读文本，可 `tail -f`
    #[default]
    Text,
    Jsonl,
}

impl TraceFormat {
    /// 旗标文本 → 形态（大小写不敏感）；`None` = 非法值，由调用方给用法错。
    pub fn from_flag(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "jsonl" => Some(Self::Jsonl),
            _ => None,
        }
    }

    /// 形态的旗标文本。
    pub fn as_flag(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Jsonl => "jsonl",
        }
    }
}

/// trace 详细级别（`--trace-level`，§11.2）：阶梯式，级别越高记越多。
/// 变体声明序即阶梯序（派生 `Ord` 依赖它，勿随意插入新档）。
///
/// - `Stmt`：语句面骨架——SQL_EXEC 只给 sqlid，外加任何级别都不丢的 BATCH_SKIP
/// - `Sql`：`Stmt` + 游标开/取（`CURSOR_OPEN` 列名、`FETCH` 行号）+ SQL 开头片段
/// - `Bind`：`Sql` + 绑定值（原值，超 `BIND_CHARS` 截断）——**默认**，即已确认的原型口径
/// - `All`：`Bind` + 每条 IR 指令（`IR_STMT`）与 UDF 调用（`UDF_CALL`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TraceLevel {
    Stmt,
    Sql,
    /// 默认档：绑定值入行，即已确认的原型口径
    #[default]
    Bind,
    All,
}

impl TraceLevel {
    /// 旗标文本 → 级别（大小写不敏感）；`None` = 非法值，由调用方给用法错。
    pub fn from_flag(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "stmt" => Some(Self::Stmt),
            "sql" => Some(Self::Sql),
            "bind" => Some(Self::Bind),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// 级别的旗标文本（错误消息与 `explain --trace` 回显用）。
    pub fn as_flag(self) -> &'static str {
        match self {
            Self::Stmt => "stmt",
            Self::Sql => "sql",
            Self::Bind => "bind",
            Self::All => "all",
        }
    }
}

/// 一行的公共头字段（§11.2）：`ts`/`sid`/`cid` 由运行环境注入，渲染器自身不取时钟
/// 也不生成 ID（AGENTS §2.6 接缝 3：时钟与 ID 必须是可注入依赖）。
/// text 形态只用到 `seq`，其余字段留空不影响输出。
#[derive(Debug, Clone)]
pub struct LineCtx {
    /// 本次调用内单调递增的行号（首行为 1）。
    pub seq: u64,
    /// UTC ISO8601 毫秒时间戳。
    pub ts: String,
    /// 会话标识。
    pub sid: String,
    /// 调用标识。
    pub cid: String,
    /// 例程名（`PKG_X.prc_y`）。
    pub proc: String,
}

#[cfg(test)]
mod tests {
    use super::{TraceFormat, TraceLevel};

    #[test]
    fn should_parse_trace_flag_texts_case_insensitively() {
        assert_eq!(TraceFormat::from_flag("JSONL"), Some(TraceFormat::Jsonl));
        assert_eq!(TraceFormat::from_flag("text"), Some(TraceFormat::Text));
        assert_eq!(TraceFormat::from_flag("yaml"), None);
        assert_eq!(TraceLevel::from_flag("All"), Some(TraceLevel::All));
        assert_eq!(TraceLevel::from_flag("stmt"), Some(TraceLevel::Stmt));
        assert_eq!(TraceLevel::from_flag(""), None);
    }

    #[test]
    fn should_name_each_level_by_its_flag_text() {
        // 反查用（explain 对齐视图与错误消息都要把级别写回文本）
        assert_eq!(
            [
                TraceLevel::Stmt.as_flag(),
                TraceLevel::Sql.as_flag(),
                TraceLevel::Bind.as_flag(),
                TraceLevel::All.as_flag(),
            ],
            ["stmt", "sql", "bind", "all"]
        );
        assert_eq!(TraceFormat::Jsonl.as_flag(), "jsonl");
    }
}
