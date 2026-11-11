//! 审计汇：事件 → 行 → writer（逐行 flush），级别门在此生效。
//!
//! 时钟与标识由调用方注入（`clock` 闭包与 [`SinkCfg`]）：本模块不取系统时间、
//! 不生成 ID，测试因此可以逐字断言整行输出（AGENTS §2.6 接缝 3）。

use super::render::{flat, render_jsonl, render_text, sql_id};
use super::{level_allows, LineCtx, TraceFormat, TraceLevel};
use crate::exec::{TraceEvent, TraceSink};
use std::collections::BTreeMap;
use std::io::Write;

/// 一次 CALL 的汇配置（构造后不变）。
#[derive(Debug, Clone)]
pub struct SinkCfg {
    pub fmt: TraceFormat,
    pub level: TraceLevel,
    pub sid: String,
    pub cid: String,
    pub proc: String,
}

/// 事件汇：按级别过滤后逐条渲染、写入并 flush。
pub struct AuditSink<W> {
    cfg: SinkCfg,
    seq: u64,
    clock: Box<dyn Fn() -> String>,
    writer: W,
    /// 落盘失败的首因（`Option<String>`：只留第一条，不被后续同类失败淹没）
    first_io_error: Option<String>,
    /// sqlid → SQL 全文（折叠空白后的形态）：日志行只给开头，全文靠 sidecar 反查。
    sql_index: BTreeMap<String, String>,
}

impl<W: Write> AuditSink<W> {
    pub fn new(cfg: SinkCfg, clock: impl Fn() -> String + 'static, writer: W) -> Self {
        Self {
            cfg,
            seq: 0,
            clock: Box::new(clock),
            writer,
            first_io_error: None,
            sql_index: BTreeMap::new(),
        }
    }
}

impl<W: Write> AuditSink<W> {
    /// 落盘失败的首因；调用方收尾时据此上报，不得让日志通道静默失效。
    pub fn io_error(&self) -> Option<String> {
        self.first_io_error.clone()
    }

    /// sqlid → 全文（按 sqlid 升序）；调用方据此写 sidecar。
    pub fn sql_index(&self) -> &BTreeMap<String, String> {
        &self.sql_index
    }

    fn remember(&mut self, e: std::io::Error) {
        if self.first_io_error.is_none() {
            self.first_io_error = Some(e.to_string());
        }
    }

    /// 事件承载的 SQL 文本（若有）：登记口径与 `sql_id` 逐字一致。
    fn note_sql(&mut self, ev: &TraceEvent) {
        let sql = match ev {
            TraceEvent::SqlExec { sql, .. } => sql,
            TraceEvent::CursorOpen { sql, .. } => sql,
            TraceEvent::Fetch { .. }
            | TraceEvent::UdfCall { .. }
            | TraceEvent::IrStmt { .. }
            | TraceEvent::BatchSkip { .. } => return,
        };
        let text = flat(sql);
        self.sql_index.entry(sql_id(sql)).or_insert(text);
    }
}

impl<W: Write> TraceSink for AuditSink<W> {
    fn on_event(&mut self, ev: TraceEvent) {
        if !level_allows(self.cfg.level, &ev) {
            return;
        }
        self.note_sql(&ev);
        self.seq += 1;
        let ctx = LineCtx {
            seq: self.seq,
            ts: (self.clock)(),
            sid: self.cfg.sid.clone(),
            cid: self.cfg.cid.clone(),
            proc: self.cfg.proc.clone(),
        };
        let line = match self.cfg.fmt {
            TraceFormat::Text => render_text(self.cfg.level, &ctx, &ev),
            TraceFormat::Jsonl => render_jsonl(self.cfg.level, &ctx, &ev),
        };
        // 写入即 flush：进程被 kill 时已写入的行不留在用户态缓冲里（§11.1 at-least-once）
        if let Err(e) = writeln!(self.writer, "{line}") {
            self.remember(e);
        }
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditSink, SinkCfg};
    use crate::exec::{BindMap, TraceEvent, TraceSink};
    use crate::trace::{TraceFormat, TraceLevel};

    fn cfg(fmt: TraceFormat, level: TraceLevel) -> SinkCfg {
        SinkCfg {
            fmt,
            level,
            sid: "s-1".to_string(),
            cid: "c-42".to_string(),
            proc: "PKG_X.prc_y".to_string(),
        }
    }

    fn sql_exec(sql: &str) -> TraceEvent {
        TraceEvent::SqlExec {
            sql: sql.to_string(),
            binds: BindMap::new(),
            elapsed_us: 0,
        }
    }

    /// 记录 flush 次数的 writer：证明"每行落盘即 flush"，而不是等进程退出。
    struct CountingWriter {
        buf: Vec<u8>,
        flushes: usize,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    /// 写入必失败的 writer（磁盘满/只读目录的最小模型）。
    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no space left on device"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn should_keep_the_first_write_failure_so_the_caller_can_report_it() {
        // 日志通道故障不得静默（§11.1 at-least-once）：首因留存，后续失败不覆盖
        let mut sink = AuditSink::new(
            cfg(TraceFormat::Text, TraceLevel::Bind),
            || "2026-11-11T02:31:07.412Z".to_string(),
            FailingWriter,
        );
        sink.on_event(sql_exec("SELECT 1"));
        sink.on_event(sql_exec("SELECT 2"));
        assert_eq!(sink.io_error(), Some("no space left on device".to_string()));
    }

    #[test]
    fn should_flush_after_every_line_so_a_crash_loses_no_written_line() {
        let mut w = CountingWriter {
            buf: Vec::new(),
            flushes: 0,
        };
        {
            let mut sink = AuditSink::new(
                cfg(TraceFormat::Text, TraceLevel::Bind),
                || "2026-11-11T02:31:07.412Z".to_string(),
                &mut w,
            );
            sink.on_event(sql_exec("SELECT 1"));
            sink.on_event(sql_exec("SELECT 2"));
        }
        assert_eq!(w.flushes, 2, "每行一次 flush，不是收尾一次");
    }

    #[test]
    fn should_write_one_text_line_per_allowed_event_with_increasing_seq() {
        let mut buf = Vec::new();
        {
            let mut sink = AuditSink::new(
                cfg(TraceFormat::Text, TraceLevel::Bind),
                || "2026-11-11T02:31:07.412Z".to_string(),
                &mut buf,
            );
            sink.on_event(sql_exec("SELECT 1"));
            // IR 指令只在 all 级记录：bind 级必须整条丢弃（逐条全量的前提是门正确）
            sink.on_event(TraceEvent::IrStmt { variant: "Assign" });
            sink.on_event(sql_exec("SELECT 2"));
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "[trace] 2026-11-11T02:31:07.412Z 000001 SQL_EXEC     sq-7bca63ea84f2  binds=-  :: SELECT 1\n\
             [trace] 2026-11-11T02:31:07.412Z 000002 SQL_EXEC     sq-7aca63ea833f  binds=-  :: SELECT 2\n"
        );
    }

    #[test]
    fn should_index_each_sql_text_once_by_its_id() {
        // sidecar 反查的前提：凡产了行的事件，其 SQL 全文按 sqlid 登记一次
        // （循环里的同一条语句重复执行只留一条；空白折叠后同形即同一条）
        let mut buf = Vec::new();
        let mut sink = AuditSink::new(
            cfg(TraceFormat::Text, TraceLevel::Bind),
            || "2026-11-11T02:31:07.412Z".to_string(),
            &mut buf,
        );
        sink.on_event(sql_exec("SELECT 1"));
        sink.on_event(sql_exec("SELECT  1"));
        sink.on_event(TraceEvent::CursorOpen {
            sql: "SELECT 3 AS a".to_string(),
            schema: Vec::new(),
        });
        // 级别门槛丢弃的事件不产行，也就进不了索引
        sink.on_event(TraceEvent::IrStmt { variant: "Assign" });
        assert_eq!(
            sink.sql_index().clone(),
            [
                ("sq-317bef602817".to_string(), "SELECT 3 AS a".to_string()),
                ("sq-7bca63ea84f2".to_string(), "SELECT 1".to_string()),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<String, String>>()
        );
    }
}
