//! 语句片段区间切分与调用点就地下沉（自 `sqlfn` 拆分，AGENTS §3）。
//!
//! 下沉需要"在同一条语句文本里，把某个调用点原地换成它的缓存子查询"，而实参、
//! WHERE 合取项都可能自带别的调用点。本模块只做**字节区间**层面的切分与替换：
//! 一切判定基于调用点的绝对区间（而非名称字符串），因此不受 AST 重排产生的
//! 空白差异影响（历史上按 `name(` 字面匹配会漏判）。

use crate::error::EngineError;
use crate::sql::sqlfn::{CallPlan, Occurrence};
use ogsql_parser::token::{Token, TokenWithSpan};

/// 调用括号内按顶层逗号切分的实参绝对区间（空实参不产出区间）。
pub(super) fn arg_spans(
    sql_text: &str,
    tokens: &[TokenWithSpan],
    open_idx: usize,
    close_idx: usize,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut depth = 0i32;
    let mut seg = tokens[open_idx].span.end;
    for t in tokens[open_idx + 1..close_idx].iter() {
        match t.token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Comma if depth == 0 => {
                push_span(sql_text, seg, t.span.start, &mut spans);
                seg = t.span.end;
            }
            _ => {}
        }
    }
    push_span(sql_text, seg, tokens[close_idx].span.start, &mut spans);
    spans
}

fn push_span(sql_text: &str, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    let raw = &sql_text[from..to];
    let body = raw.trim();
    if body.is_empty() {
        return;
    }
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    out.push((from + lead, to - trail));
}

/// `[from, to)` 内按顶层 `AND` 切出的合取项绝对区间（引号、括号内不切）。
pub(super) fn top_level_and_ranges(sql_text: &str, from: usize, to: usize) -> Vec<(usize, usize)> {
    let bytes = sql_text.as_bytes();
    let mut ranges = Vec::new();
    let mut depth = 0i32;
    let mut quote = false;
    let mut seg = from;
    let mut i = from;
    while i < to {
        let ch = bytes[i];
        if quote {
            if ch == b'\'' {
                quote = false;
            }
            i += 1;
            continue;
        }
        match ch {
            b'\'' => quote = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'A' | b'a' if depth == 0 && is_and(bytes, i, to) => {
                ranges.push((seg, i));
                i += 3;
                seg = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    ranges.push((seg, to));
    ranges
}

fn is_and(bytes: &[u8], i: usize, to: usize) -> bool {
    i + 3 <= to
        && bytes[i..i + 3].eq_ignore_ascii_case(b"AND")
        && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        && (i + 3 >= to || !bytes[i + 3].is_ascii_alphanumeric())
}

/// 片段 `[from, to)` 内把**其他**调用点替换为其缓存子查询（按拓扑序已算好）。
///
/// 只替换最外层调用点：更深层的调用随外层文本一并下沉；本站点自身的调用点
/// 保持原文（元组查询不得自引用）。
pub(super) fn lower_spans(
    sql_text: &str,
    from: usize,
    to: usize,
    site: usize,
    plans: &[CallPlan],
    occs: &[Occurrence],
) -> Result<String, EngineError> {
    let mut inside: Vec<&Occurrence> = occs
        .iter()
        .filter(|o| o.plan != site && o.start >= from && o.end <= to)
        .collect();
    // 只留最外层：同起点时长者在前，被前者区间覆盖的即更深一层
    inside.sort_unstable_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut outer: Vec<&Occurrence> = Vec::with_capacity(inside.len());
    for o in inside {
        match outer.last() {
            Some(p) if p.start <= o.start && o.end <= p.end => {}
            _ => outer.push(o),
        }
    }
    let mut out = String::new();
    let mut cursor = from;
    for o in outer {
        let sub = &plans[o.plan].sub_orig;
        if sub.is_empty() {
            // 拓扑序被破坏：依赖计划尚未算出缓存子查询
            return Err(EngineError::invariant(
                47,
                format!(
                    "下沉顺序错误：{} 的依赖 {} 未先物化",
                    plans[site].name, plans[o.plan].name
                ),
            ));
        }
        out.push_str(&sql_text[cursor..o.start]);
        out.push_str(sub);
        cursor = o.end;
    }
    out.push_str(&sql_text[cursor..to]);
    Ok(out)
}
