//! explain ↔ trace 对齐（关联形态 A）：日志文本 → sqlid 直方图 → IR 行命中注记。
//!
//! 纯函数层：不碰文件、不碰内核，text 与 jsonl 两种日志形态都吃。指纹与渲染层同源
//! （`render::sql_id` = 折叠空白后的 FNV-1a 48bit），因此 explain 的静态 SQL 与日志行
//! 天然对上，**零契约变更**。
//!
//! 解析口径：一行的身份由 `sq-` 段给出，绑定段取 `binds=` 到 ` :: ` 之间。text 形态的
//! SQL 片段已经 [`flat`] 折叠成单空格，故 `  :: `（双空格 + 冒号 + 空格）只可能出现在
//! 分隔处；唯一例外是绑定值里自带同样的三连空格，此时片段会被提前切断——jsonl 形态无此
//! 歧义，需要逐字段精确判读时按 `--trace-format jsonl` 落盘。

use std::collections::{BTreeMap, BTreeSet};

/// 一个 sqlid 在日志里的命中统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hit {
    /// 命中次数（同一文本每执行一次加一）。
    pub count: usize,
    /// 首次命中的绑定摘要（无绑定为 `-`）。
    pub first: String,
    /// 末次命中的绑定摘要。
    pub last: String,
    /// 日志里出现过的 SQL 开头片段（反向报警时用于认脸）。
    pub head: String,
}

/// 一行的解析结果（只有带 sqlid 的事件才有）。
struct Seen {
    id: String,
    binds: String,
    head: String,
}

/// 日志文本 → `sqlid → 命中统计`；不含 sqlid 的行（IR_STMT/FETCH/…）直接跳过。
pub fn histogram(log: &str) -> BTreeMap<String, Hit> {
    let mut hits: BTreeMap<String, Hit> = BTreeMap::new();
    for line in log.lines() {
        let Some(seen) = parse_line(line) else {
            continue;
        };
        let hit = hits.entry(seen.id).or_default();
        hit.count += 1;
        if hit.count == 1 {
            hit.first = seen.binds.clone();
            hit.head = seen.head;
        }
        hit.last = seen.binds;
    }
    hits
}

/// 一条静态 SQL 在日志里的注记文本：`[trace ×N 绑定 …]` / `[trace ×N 首 … 末 …]`，
/// 无绑定只给次数，未命中给 `[trace ×0]`。
pub fn annotation(id: &str, hits: &BTreeMap<String, Hit>) -> String {
    match hits.get(id) {
        None => "[trace ×0]".to_string(),
        Some(h) if h.first == "-" => format!("[trace ×{}]", h.count),
        Some(h) if h.first == h.last => format!("[trace ×{} 绑定 {}]", h.count, h.first),
        Some(h) => format!("[trace ×{} 首 {} 末 {}]", h.count, h.first, h.last),
    }
}

/// 日志里有、IR 里没有的 sqlid（按 id 升序）：动态 SQL、攒批合并文本、被调例程自己的语句。
pub fn unmatched<'a>(
    hits: &'a BTreeMap<String, Hit>,
    ir_ids: &BTreeSet<String>,
) -> Vec<(&'a String, &'a Hit)> {
    hits.iter()
        .filter(|(id, _)| !ir_ids.contains(*id))
        .collect()
}

/// 单行解析：`{` 开头走 jsonl，其余走 text。
fn parse_line(line: &str) -> Option<Seen> {
    if line.trim_start().starts_with('{') {
        return parse_jsonl(line);
    }
    parse_text(line)
}

/// jsonl 形态：按键取 `detail.sql_id / detail.binds / detail.sql_head`。
/// 级别门槛之下的键整键缺席，因此绑定缺失一律记 `-`（与 text 同口径）。
fn parse_jsonl(line: &str) -> Option<Seen> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let detail = value.get("detail")?.as_object()?;
    let id = detail.get("sql_id")?.as_str()?.to_string();
    let binds = match detail.get("binds").and_then(|b| b.as_object()) {
        None => "-".to_string(),
        Some(map) if map.is_empty() => "-".to_string(),
        Some(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" "),
    };
    let head = detail
        .get("sql_head")
        .and_then(|h| h.as_str())
        .unwrap_or_default()
        .to_string();
    Some(Seen { id, binds, head })
}

/// text 形态：`sq-` 段即身份，`binds=` 到 ` :: ` 之间为绑定摘要，` :: ` 之后为片段。
fn parse_text(line: &str) -> Option<Seen> {
    let id = line.split_whitespace().find(|t| is_sql_id(t))?.to_string();
    let (before, head) = match line.split_once("  :: ") {
        Some((b, h)) => (b, h.to_string()),
        None => (line, String::new()),
    };
    let binds = match before.split_once("binds=") {
        Some((_, b)) => b.trim_end().to_string(),
        None => "-".to_string(),
    };
    Some(Seen { id, binds, head })
}

/// sqlid 的文本形状：`sq-` + 12 位小写十六进制（与 [`sql_id`] 的渲染一致）。
fn is_sql_id(token: &str) -> bool {
    let Some(hex) = token.strip_prefix("sq-") else {
        return false;
    };
    hex.len() == 12
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::{annotation, histogram, is_sql_id, unmatched};
    use crate::trace::{sql_id, Hit};
    use std::collections::BTreeSet;

    const SQL: &str =
        "update par_fund_partner set status = 1 where partner_code = $v_r__partner_code";

    fn text_line(seq: u32, binds: &str) -> String {
        format!(
            "[trace] 2026-11-11T04:34:22.52{}Z {seq:06} SQL_EXEC     {}  binds={binds}  :: {SQL}",
            seq % 10,
            sql_id(SQL)
        )
    }

    fn hit(hits: &std::collections::BTreeMap<String, Hit>) -> &Hit {
        hits.get(&sql_id(SQL)).expect("该 sqlid 必须入图")
    }

    #[test]
    fn should_count_repeated_text_lines_and_keep_first_and_last_binds() {
        let log = [
            text_line(1, "$v_r__partner_code=A0001"),
            "[trace] 2026-11-11T04:34:22.600Z 000002 IR_STMT      Assign".to_string(),
            text_line(3, "$v_r__partner_code=A2000"),
        ]
        .join("\n");
        let hits = histogram(&log);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            hit(&hits),
            &Hit {
                count: 2,
                first: "$v_r__partner_code=A0001".to_string(),
                last: "$v_r__partner_code=A2000".to_string(),
                head: SQL.to_string(),
            }
        );
    }

    #[test]
    fn should_read_the_same_hits_from_jsonl_lines() {
        let line = format!(
            "{{\"ch\":\"trace\",\"cid\":\"c-000001\",\"detail\":{{\"binds\":\
            {{\"$v_p_date\":\"20260105\"}},\"sql_chars\":97,\"sql_head\":\"{SQL}\",\
            \"sql_id\":\"{}\"}},\"event\":\"SQL_EXEC\",\"lvl\":\"INFO\",\
            \"proc\":\"prc_demo\",\"seq\":1}}",
            sql_id(SQL)
        );
        let hits = histogram(&line);
        let h = hit(&hits);
        assert_eq!(h.count, 1, "{hits:?}");
        assert_eq!(h.first, "$v_p_date=20260105");
        assert_eq!(h.head, SQL);
    }

    #[test]
    fn should_count_cursor_open_lines_without_binds_as_a_hit_with_dash() {
        // 游标语句在 text 形态里没有 binds=，注记必须是 `-` 而不是空串
        let line = format!(
            "[trace] 2026-11-11T04:34:22.526Z 000002 CURSOR_OPEN  {}  cols=a,b  :: {SQL}",
            sql_id(SQL)
        );
        let hits = histogram(&line);
        let h = hit(&hits);
        assert_eq!((h.first.as_str(), h.last.as_str()), ("-", "-"));
    }

    #[test]
    fn should_annotate_a_hit_line_with_count_and_bind_span() {
        let log = (1..=3)
            .map(|n| text_line(n, &format!("k={n}")))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            annotation(&sql_id(SQL), &histogram(&log)),
            "[trace ×3 首 k=1 末 k=3]"
        );
    }

    #[test]
    fn should_annotate_a_single_bind_value_without_a_span() {
        assert_eq!(
            annotation(&sql_id(SQL), &histogram(&text_line(1, "k=1"))),
            "[trace ×1 绑定 k=1]"
        );
    }

    #[test]
    fn should_annotate_a_hit_without_binds_with_the_count_only() {
        // 无绑定的语句（`SELECT 1`）首末都是 `-`：注记只给次数，不重复绑定
        let log = [text_line(1, "-"), text_line(2, "-")].join("\n");
        assert_eq!(annotation(&sql_id(SQL), &histogram(&log)), "[trace ×2]");
    }

    #[test]
    fn should_annotate_an_ir_line_that_never_ran() {
        assert_eq!(annotation(&sql_id(SQL), &histogram("")), "[trace ×0]");
    }

    #[test]
    fn should_report_log_sqlids_that_no_ir_line_contains() {
        // 反向报警：日志里跑了、但本次 explain 的 IR 里没有的语句（动态 SQL/子树语句）
        let hits = histogram(&text_line(1, "-"));
        let mut ids = BTreeSet::new();
        ids.insert(sql_id("select 1 from dual"));
        let orphans = unmatched(&hits, &ids);
        assert_eq!(orphans.len(), 1, "{orphans:?}");
        assert_eq!(orphans[0].0, &sql_id(SQL));
        assert_eq!(orphans[0].1.count, 1);
        assert_eq!(orphans[0].1.head, SQL);
        // 同一 id 在 IR 里存在时不报警
        ids.insert(sql_id(SQL));
        assert!(unmatched(&hits, &ids).is_empty());
        // 形状判定只认渲染层给出的形态
        assert!(is_sql_id(&sql_id(SQL)));
        assert!(!is_sql_id("sq-zzzzzzzzzzzz"));
        assert!(!is_sql_id("select"));
    }
}
