//! 审计日志渲染：`TraceEvent` → 稳定文本行（text 默认形态 / jsonl §11.2 形态）。
//!
//! 口径要点（原型 v2 已确认）：SQL 全文**不入日志行**——每条 SQL 由文本指纹
//! `sqlid` 标识，日志只给开头片段，全文经 sidecar 反查；归一折叠空白与
//! `explain` 的 `one_line` 同口径，因此 `sqlid` 可直接与 explain 输出对齐。

use super::{LineCtx, TraceLevel};
use crate::exec::{BindMap, TraceEvent};

/// SQL 开头片段的最大字符数（超出以 `…(+N chars)` 收尾）。
pub const HEAD_CHARS: usize = 96;

/// 单个绑定值文本的最大字符数。
pub const BIND_CHARS: usize = 32;

/// 折叠连续空白为单个空格——与 `explain` 的 `one_line` 同口径，
/// 是 `sql_id` 与 explain 输出可对齐的前提。
pub fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// SQL 指纹：归一文本的 FNV-1a 64 位取低 48 bit，渲染成 `sq-` + 12 位小写十六进制。
pub fn sql_id(sql: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in flat(sql).as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("sq-{:012x}", h & 0xffff_ffff_ffff)
}

/// 单行片段：归一后取前 `n` 字符；被削掉时在 `…(+N chars)` 里报出省略的字符数。
pub fn head(text: &str, n: usize) -> String {
    let flat = flat(text);
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= n {
        return flat;
    }
    let picked: String = chars.iter().take(n).collect();
    format!("{}…(+{} chars)", picked.trim_end(), chars.len() - n)
}

/// 绑定摘要：`k=值` 以空格分隔，无绑定为 `-`，值按 `BIND_CHARS` 截断。
fn binds_brief(binds: &BindMap) -> String {
    if binds.is_empty() {
        return "-".to_string();
    }
    binds
        .iter()
        .map(|(k, v)| format!("{k}={}", head(&v.display(), BIND_CHARS)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 渲染一条事件的 text 形态日志行；`seq` 由调用方推进（首行为 1）。
/// 级别只影响 SQL_EXEC 的字段面，其余事件 kinds 由 [`level_allows`] 决定。
pub fn render_text(level: TraceLevel, ctx: &LineCtx, ev: &TraceEvent) -> String {
    let seq = ctx.seq;
    let ts = &ctx.ts;
    match ev {
        TraceEvent::SqlExec { sql, binds, .. } => {
            let id = sql_id(sql);
            let text = head(sql, HEAD_CHARS);
            match level {
                TraceLevel::Stmt => format!("[trace] {ts} {seq:06} SQL_EXEC     {id}"),
                TraceLevel::Sql => format!("[trace] {ts} {seq:06} SQL_EXEC     {id}  :: {text}"),
                TraceLevel::Bind | TraceLevel::All => format!(
                    "[trace] {ts} {seq:06} SQL_EXEC     {id}  binds={}  :: {text}",
                    binds_brief(binds)
                ),
            }
        }
        TraceEvent::CursorOpen { sql, schema } => format!(
            "[trace] {ts} {seq:06} CURSOR_OPEN  {}  cols={}  :: {}",
            sql_id(sql),
            if schema.is_empty() {
                "-".to_string()
            } else {
                schema.join(",")
            },
            head(sql, HEAD_CHARS)
        ),
        TraceEvent::Fetch { row } => format!(
            "[trace] {ts} {seq:06} FETCH        row={}",
            row.map(|n| n.to_string()).unwrap_or_else(|| "end".into())
        ),
        TraceEvent::UdfCall { name } => format!("[trace] {ts} {seq:06} UDF_CALL     name={name}"),
        TraceEvent::IrStmt { variant } => format!("[trace] {ts} {seq:06} IR_STMT      {variant}"),
        TraceEvent::BatchSkip { rows, first_error } => format!(
            "[trace] {ts} {seq:06} BATCH_SKIP   rows={rows} first_error={}",
            head(first_error, HEAD_CHARS)
        ),
    }
}

/// 渲染一条事件的 jsonl 形态日志行（单行 JSON，字段序由 `serde_json` 稳定排序）。
/// 级别门槛之下的字段**整键缺席**，而不是给空值——机器消费者按键存在性判读。
pub fn render_jsonl(level: TraceLevel, ctx: &LineCtx, ev: &TraceEvent) -> String {
    use serde_json::{json, Map, Value};
    let (event, elapsed_us, detail): (&str, u64, Map<String, Value>) = match ev {
        TraceEvent::SqlExec {
            sql,
            binds,
            elapsed_us,
        } => {
            let mut d = Map::new();
            if level >= TraceLevel::Bind {
                let mut b = Map::new();
                for (k, v) in binds {
                    b.insert(k.clone(), json!(head(&v.display(), BIND_CHARS)));
                }
                d.insert("binds".into(), Value::Object(b));
            }
            if level >= TraceLevel::Sql {
                d.insert("sql_head".into(), json!(head(sql, HEAD_CHARS)));
            }
            d.insert("sql_id".into(), json!(sql_id(sql)));
            d.insert("sql_chars".into(), json!(sql.chars().count()));
            ("SQL_EXEC", *elapsed_us, d)
        }
        TraceEvent::CursorOpen { sql, schema } => {
            let mut d = Map::new();
            d.insert("columns".into(), json!(schema));
            d.insert("sql_head".into(), json!(head(sql, HEAD_CHARS)));
            d.insert("sql_id".into(), json!(sql_id(sql)));
            d.insert("sql_chars".into(), json!(sql.chars().count()));
            ("CURSOR_OPEN", 0, d)
        }
        TraceEvent::Fetch { row } => {
            let mut d = Map::new();
            d.insert("row".into(), json!(row));
            ("FETCH", 0, d)
        }
        TraceEvent::UdfCall { name } => {
            let mut d = Map::new();
            d.insert("name".into(), json!(name));
            ("UDF_CALL", 0, d)
        }
        TraceEvent::IrStmt { variant } => {
            let mut d = Map::new();
            d.insert("variant".into(), json!(variant));
            ("IR_STMT", 0, d)
        }
        TraceEvent::BatchSkip { rows, first_error } => {
            let mut d = Map::new();
            d.insert("rows".into(), json!(rows));
            d.insert("first_error".into(), json!(first_error));
            ("BATCH_SKIP", 0, d)
        }
    };
    json!({
        "ts": ctx.ts,
        "lvl": "INFO",
        "ch": "trace",
        "sid": ctx.sid,
        "cid": ctx.cid,
        "seq": ctx.seq,
        "proc": ctx.proc,
        "msg": "",
        "err": null,
        "event": event,
        "detail": Value::Object(detail),
        "elapsed_us": elapsed_us,
    })
    .to_string()
}

/// 该级别是否记录这类事件。
pub fn level_allows(level: TraceLevel, ev: &TraceEvent) -> bool {
    match ev {
        TraceEvent::SqlExec { .. } => true,
        // 丢行是失败信号，任何级别都不许丢，否则批量回放的丢行重新变成静默
        TraceEvent::BatchSkip { .. } => true,
        TraceEvent::CursorOpen { .. } | TraceEvent::Fetch { .. } => level >= TraceLevel::Sql,
        TraceEvent::UdfCall { .. } | TraceEvent::IrStmt { .. } => level >= TraceLevel::All,
    }
}
#[cfg(test)]
mod tests {
    use super::{level_allows, render_jsonl, render_text, LineCtx, TraceLevel};
    use crate::exec::{BindMap, TraceEvent};
    use crate::trace::TraceFormat;
    use crate::value::SpinValue;

    #[test]
    fn should_render_sql_exec_as_text_line_with_id_binds_and_head() {
        // seq 零补齐 + sqlid + binds 摘要 + SQL 开头片段（全文不入行）
        let ev = TraceEvent::SqlExec {
            sql: "SELECT\n     1".to_string(),
            binds: BindMap::new(),
            elapsed_us: 0,
        };
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(7), &ev),
            "[trace] 2026-11-11T02:31:07.412Z 000007 SQL_EXEC     sq-7bca63ea84f2  binds=-  :: SELECT 1"
        );
    }

    #[test]
    fn should_clip_a_long_sql_and_report_the_dropped_char_count() {
        // 全文不入行：只留前 HEAD_CHARS 个**字符**（按字符而非字节，多字节不切碎）
        let sql = format!("SELECT {} FROM t", "a".repeat(120));
        let ev = TraceEvent::SqlExec {
            sql,
            binds: BindMap::new(),
            elapsed_us: 0,
        };
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(1), &ev),
            format!(
                "[trace] 2026-11-11T02:31:07.412Z 000001 SQL_EXEC     sq-0b81c0136ab9  binds=-  :: SELECT {}…(+38 chars)",
                "a".repeat(89)
            )
        );
    }

    #[test]
    fn should_count_multibyte_sql_in_chars_not_bytes_when_clipping() {
        let sql = format!("SELECT {}", "字".repeat(200));
        let ev = TraceEvent::SqlExec {
            sql,
            binds: BindMap::new(),
            elapsed_us: 0,
        };
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(1), &ev),
            format!(
                "[trace] 2026-11-11T02:31:07.412Z 000001 SQL_EXEC     sq-f21b9511d49f  binds=-  :: SELECT {}…(+111 chars)",
                "字".repeat(89)
            )
        );
    }

    #[test]
    fn should_clip_bind_values_and_list_them_in_key_order() {
        // 绑定原值入日志，只截长度；键序按 BindMap 的字典序（可 diff 的前提）
        let mut binds = BindMap::new();
        binds.insert("p_note".to_string(), SpinValue::Str("n".repeat(50)));
        binds.insert("p_date".to_string(), SpinValue::Int(20260105));
        binds.insert("p_null".to_string(), SpinValue::Null);
        let ev = TraceEvent::SqlExec {
            sql: "SELECT 1".to_string(),
            binds,
            elapsed_us: 0,
        };
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(2), &ev),
            "[trace] 2026-11-11T02:31:07.412Z 000002 SQL_EXEC     sq-7bca63ea84f2  binds=p_date=20260105 p_note=nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn…(+18 chars) p_null=NULL  :: SELECT 1"
        );
    }

    #[test]
    fn should_render_cursor_open_with_column_list_or_dash() {
        let with_cols = TraceEvent::CursorOpen {
            sql: "SELECT 1 AS qid, 7 AS js_days".to_string(),
            schema: vec!["qid".to_string(), "js_days".to_string()],
        };
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(3), &with_cols),
            "[trace] 2026-11-11T02:31:07.412Z 000003 CURSOR_OPEN  sq-8cfb7e071ef0  cols=qid,js_days  :: SELECT 1 AS qid, 7 AS js_days"
        );
        assert_eq!(
            render_text(
                TraceLevel::Bind,
                &ctx(4),
                &TraceEvent::CursorOpen {
                    sql: "SELECT 1".to_string(),
                    schema: Vec::new()
                }
            ),
            "[trace] 2026-11-11T02:31:07.412Z 000004 CURSOR_OPEN  sq-7bca63ea84f2  cols=-  :: SELECT 1"
        );
    }

    #[test]
    fn should_render_fetch_row_index_and_end_of_cursor() {
        assert_eq!(
            render_text(
                TraceLevel::Bind,
                &ctx(5),
                &TraceEvent::Fetch { row: Some(1) }
            ),
            "[trace] 2026-11-11T02:31:07.412Z 000005 FETCH        row=1"
        );
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(6), &TraceEvent::Fetch { row: None }),
            "[trace] 2026-11-11T02:31:07.412Z 000006 FETCH        row=end"
        );
    }

    #[test]
    fn should_render_udf_call_and_ir_stmt() {
        assert_eq!(
            render_text(
                TraceLevel::Bind,
                &ctx(7),
                &TraceEvent::UdfCall {
                    name: "fnc_get_purchase_js_days".to_string()
                }
            ),
            "[trace] 2026-11-11T02:31:07.412Z 000007 UDF_CALL     name=fnc_get_purchase_js_days"
        );
        assert_eq!(
            render_text(
                TraceLevel::Bind,
                &ctx(8),
                &TraceEvent::IrStmt {
                    variant: "OpenCursor"
                }
            ),
            "[trace] 2026-11-11T02:31:07.412Z 000008 IR_STMT      OpenCursor"
        );
    }

    #[test]
    fn should_render_sql_exec_as_jsonl_with_contract_fields() {
        // §11.2 公共头 + trace 专属 event/detail/elapsed_us；本轮追加 seq 与
        // detail 的 sql_id/sql_head/sql_chars/binds（SQL 全文不入行，靠 sidecar 反查）
        let ctx = LineCtx {
            seq: 7,
            ts: "2026-11-11T02:31:07.412Z".to_string(),
            sid: "s-1".to_string(),
            cid: "c-42".to_string(),
            proc: "PKG_X.prc_y".to_string(),
        };
        let ev = TraceEvent::SqlExec {
            sql: "SELECT 1".to_string(),
            binds: BindMap::new(),
            elapsed_us: 97,
        };
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx, &ev),
            "{\"ch\":\"trace\",\"cid\":\"c-42\",\"detail\":{\"binds\":{},\"sql_chars\":8,\"sql_head\":\"SELECT 1\",\"sql_id\":\"sq-7bca63ea84f2\"},\"elapsed_us\":97,\"err\":null,\"event\":\"SQL_EXEC\",\"lvl\":\"INFO\",\"msg\":\"\",\"proc\":\"PKG_X.prc_y\",\"seq\":7,\"sid\":\"s-1\",\"ts\":\"2026-11-11T02:31:07.412Z\"}"
        );
    }

    #[test]
    fn should_jsonl_omit_fields_below_their_level() {
        // 字段低于级别门槛时整键缺席（不是空串/空对象）：机器消费者按键存在性判读
        let mut binds = BindMap::new();
        binds.insert("p_date".to_string(), SpinValue::Int(20260105));
        let ev = TraceEvent::SqlExec {
            sql: "SELECT $p_date".to_string(),
            binds,
            elapsed_us: 0,
        };
        assert_eq!(
            render_jsonl(TraceLevel::Stmt, &ctx(1), &ev),
            expect_jsonl(
                1,
                "SQL_EXEC",
                "{\"sql_chars\":14,\"sql_id\":\"sq-0492c48ec308\"}"
            )
        );
        assert_eq!(
            render_jsonl(TraceLevel::Sql, &ctx(1), &ev),
            expect_jsonl(
                1,
                "SQL_EXEC",
                "{\"sql_chars\":14,\"sql_head\":\"SELECT $p_date\",\"sql_id\":\"sq-0492c48ec308\"}"
            )
        );
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx(1), &ev),
            expect_jsonl(
                1,
                "SQL_EXEC",
                "{\"binds\":{\"p_date\":\"20260105\"},\"sql_chars\":14,\"sql_head\":\"SELECT $p_date\",\"sql_id\":\"sq-0492c48ec308\"}"
            )
        );
    }

    #[test]
    fn should_render_batch_skip_with_clipped_first_error() {
        let err = format!("KERNEL-0001 {}", "e".repeat(120));
        let line = render_text(
            TraceLevel::Bind,
            &ctx(9),
            &TraceEvent::BatchSkip {
                rows: 7,
                first_error: err,
            },
        );
        assert_eq!(
            line,
            format!(
                "[trace] 2026-11-11T02:31:07.412Z 000009 BATCH_SKIP   rows=7 first_error=KERNEL-0001 {}…(+36 chars)",
                "e".repeat(84)
            )
        );
    }

    /// 期望的 jsonl 行：公共头字段按 serde_json 的字典序固定，只有事件段可变。
    fn expect_jsonl(seq: u64, event: &str, detail: &str) -> String {
        format!(
            "{{\"ch\":\"trace\",\"cid\":\"c-42\",\"detail\":{detail},\"elapsed_us\":0,\
             \"err\":null,\"event\":\"{event}\",\"lvl\":\"INFO\",\"msg\":\"\",\
             \"proc\":\"PKG_X.prc_y\",\"seq\":{seq},\"sid\":\"s-1\",\
             \"ts\":\"2026-11-11T02:31:07.412Z\"}}"
        )
    }

    fn ctx(seq: u64) -> LineCtx {
        LineCtx {
            seq,
            ts: "2026-11-11T02:31:07.412Z".to_string(),
            sid: "s-1".to_string(),
            cid: "c-42".to_string(),
            proc: "PKG_X.prc_y".to_string(),
        }
    }

    #[test]
    fn should_render_cursor_and_fetch_jsonl_detail() {
        let cols = TraceEvent::CursorOpen {
            sql: "SELECT 1 AS qid, 7 AS js_days".to_string(),
            schema: vec!["qid".to_string(), "js_days".to_string()],
        };
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx(3), &cols),
            expect_jsonl(
                3,
                "CURSOR_OPEN",
                "{\"columns\":[\"qid\",\"js_days\"],\"sql_chars\":29,\"sql_head\":\"SELECT 1 AS qid, 7 AS js_days\",\"sql_id\":\"sq-8cfb7e071ef0\"}"
            )
        );
        let no_cols = TraceEvent::CursorOpen {
            sql: "SELECT 1".to_string(),
            schema: Vec::new(),
        };
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx(4), &no_cols),
            expect_jsonl(
                4,
                "CURSOR_OPEN",
                "{\"columns\":[],\"sql_chars\":8,\"sql_head\":\"SELECT 1\",\"sql_id\":\"sq-7bca63ea84f2\"}"
            )
        );
        assert_eq!(
            render_jsonl(
                TraceLevel::Bind,
                &ctx(5),
                &TraceEvent::Fetch { row: Some(1) }
            ),
            expect_jsonl(5, "FETCH", "{\"row\":1}")
        );
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx(6), &TraceEvent::Fetch { row: None }),
            expect_jsonl(6, "FETCH", "{\"row\":null}")
        );
    }

    #[test]
    fn should_render_udf_ir_and_batch_skip_jsonl_detail() {
        let udf = TraceEvent::UdfCall {
            name: "fnc_get_purchase_js_days".to_string(),
        };
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx(7), &udf),
            expect_jsonl(7, "UDF_CALL", "{\"name\":\"fnc_get_purchase_js_days\"}")
        );
        assert_eq!(
            render_jsonl(
                TraceLevel::Bind,
                &ctx(8),
                &TraceEvent::IrStmt {
                    variant: "OpenCursor"
                }
            ),
            expect_jsonl(8, "IR_STMT", "{\"variant\":\"OpenCursor\"}")
        );
        // first_error 在 jsonl 面不截断：结构化字段本就该给全量，截断只属 text
        let skip = TraceEvent::BatchSkip {
            rows: 7,
            first_error: "KERNEL-0010 constraint violation".to_string(),
        };
        assert_eq!(
            render_jsonl(TraceLevel::Bind, &ctx(9), &skip),
            expect_jsonl(
                9,
                "BATCH_SKIP",
                "{\"first_error\":\"KERNEL-0010 constraint violation\",\"rows\":7}"
            )
        );
    }

    #[test]
    fn should_gate_event_kinds_by_level() {
        // 级别阶梯 stmt ⊂ sql ⊂ bind ⊂ all：只放大"事件种类"，字段门另测
        let stmt_only = TraceEvent::SqlExec {
            sql: "SELECT 1".to_string(),
            binds: BindMap::new(),
            elapsed_us: 0,
        };
        for lvl in [
            TraceLevel::Stmt,
            TraceLevel::Sql,
            TraceLevel::Bind,
            TraceLevel::All,
        ] {
            assert!(level_allows(lvl, &stmt_only), "SQL_EXEC at {lvl:?}");
        }
        let cursor = TraceEvent::CursorOpen {
            sql: "SELECT 1".to_string(),
            schema: vec!["qid".to_string()],
        };
        assert!(!level_allows(TraceLevel::Stmt, &cursor));
        assert!(level_allows(TraceLevel::Sql, &cursor));
        let fetch = TraceEvent::Fetch { row: Some(1) };
        assert!(!level_allows(TraceLevel::Stmt, &fetch));
        assert!(level_allows(TraceLevel::Sql, &fetch));
        // BATCH_SKIP 是失败信号，任何级别都不许丢（否则丢行不可观测）
        let skip = TraceEvent::BatchSkip {
            rows: 1,
            first_error: "KERNEL-0010 x".to_string(),
        };
        assert!(level_allows(TraceLevel::Stmt, &skip));
        let udf = TraceEvent::UdfCall {
            name: "fnc_x".to_string(),
        };
        assert!(!level_allows(TraceLevel::Bind, &udf));
        assert!(level_allows(TraceLevel::All, &udf));
        let ir = TraceEvent::IrStmt { variant: "Assign" };
        assert!(!level_allows(TraceLevel::Bind, &ir));
        assert!(level_allows(TraceLevel::All, &ir));
    }

    #[test]
    fn should_text_render_only_the_fields_allowed_by_level() {
        // stmt 只给语句骨架；sql 加 SQL 片段；bind（默认）再加绑定值
        let mut binds = BindMap::new();
        binds.insert("p_date".to_string(), SpinValue::Int(20260105));
        let ev = TraceEvent::SqlExec {
            sql: "SELECT $p_date".to_string(),
            binds,
            elapsed_us: 0,
        };
        assert_eq!(
            render_text(TraceLevel::Stmt, &ctx(1), &ev),
            "[trace] 2026-11-11T02:31:07.412Z 000001 SQL_EXEC     sq-0492c48ec308"
        );
        assert_eq!(
            render_text(TraceLevel::Sql, &ctx(1), &ev),
            "[trace] 2026-11-11T02:31:07.412Z 000001 SQL_EXEC     sq-0492c48ec308  :: SELECT $p_date"
        );
        assert_eq!(
            render_text(TraceLevel::Bind, &ctx(1), &ev),
            "[trace] 2026-11-11T02:31:07.412Z 000001 SQL_EXEC     sq-0492c48ec308  binds=p_date=20260105  :: SELECT $p_date"
        );
    }
    #[test]
    fn should_default_the_trace_shape_to_bind_and_text() {
        // 默认档必须锁在 bind + text：CLI 不传 --trace-level/--trace-format 时
        // 输出的就是已确认的原型形态
        assert_eq!(TraceLevel::default(), TraceLevel::Bind);
        assert_eq!(TraceFormat::default(), TraceFormat::Text);
    }

    #[test]
    fn should_prefix_text_lines_with_the_injected_timestamp() {
        // 行首时间戳：一次 CALL 的行内耗时可由相邻 ts 相减得到
        // （elapsed_us 需要逐点计时，属后续项；ts 由汇注入，渲染器不取时钟）
        let ev = TraceEvent::SqlExec {
            sql: "SELECT 1".to_string(),
            binds: BindMap::new(),
            elapsed_us: 0,
        };
        assert_eq!(
            render_text(TraceLevel::Stmt, &ctx(7), &ev),
            "[trace] 2026-11-11T02:31:07.412Z 000007 SQL_EXEC     sq-7bca63ea84f2"
        );
    }
}
