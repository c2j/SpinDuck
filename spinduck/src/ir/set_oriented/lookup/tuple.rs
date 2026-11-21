//! LookupTuple 渲染（§3.2）：单条 GROUP BY 派生表携带全部投影列与
//! `COUNT(*) AS sd_cnt`，共享 `sd_cnt = 1` 命中门控；每个调用方变量
//! 一条 `CASE WHEN hit THEN <列_i> ELSE <兜底常量_i> END`。
//! 仅 Eq 相关键形态；不变量/范围键/IN 子查询保守拒绝（交回解释器）。

use super::super::schema::Table;
use super::parse::fallback_sql;
use super::{
    CorrelationOp, Name, ResolvedTuple, ResolvedTupleSlot, SkipReason, TupleQuery, TupleSlot,
    TupleSource,
};
use crate::value::SpinValue;

/// 解析产物 → 渲染产物：派生表 CTE + LEFT JOIN + 共享 hit + 槽位值。
pub(crate) fn render_tuple(
    tq: &TupleQuery,
    table: &Table,
    index: usize,
) -> Result<ResolvedTuple, SkipReason> {
    if tq.correlations.is_empty() || tq.correlations.iter().any(|c| c.op != CorrelationOp::Eq) {
        return Err(SkipReason::UnprovenQueryTotalness);
    }
    let alias = format!("sd_lk{index}");
    let (key_selects, group_by, join_on, values) = fragments(tq, &alias);
    let mut slots = Vec::with_capacity(tq.slots.len());
    for slot in &tq.slots {
        slots.push(resolve_slot(slot, tq, table, &alias)?);
    }
    Ok(ResolvedTuple {
        cte: format!(
            "{alias} AS (SELECT {key_selects}, {values}, COUNT(*) AS sd_cnt \
             FROM {}{} GROUP BY {group_by})",
            table.qualified, tq.filter
        ),
        join: format!("LEFT JOIN {alias} ON {join_on}"),
        hit: format!("{alias}.sd_cnt = 1"),
        slots,
        key_checks: key_checks(tq, table)?,
    })
}

/// CTE 片段：相关键投影/分组键/JOIN 条件/值列 MAX 聚合。
fn fragments(tq: &TupleQuery, alias: &str) -> (String, String, String, String) {
    let single = tq.correlations.len() == 1;
    let key_alias = |i: usize| {
        if single {
            "sd_key".to_string()
        } else {
            format!("sd_key{i}")
        }
    };
    let select_keys = tq
        .correlations
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{} AS {}", c.dim_column.sql(), key_alias(i)))
        .collect::<Vec<_>>()
        .join(", ");
    let group_by = tq
        .correlations
        .iter()
        .map(|c| c.dim_column.sql())
        .collect::<Vec<_>>()
        .join(", ");
    let join_on = tq
        .correlations
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{alias}.{} = sd_cur.{}", key_alias(i), c.cursor_field.sql()))
        .collect::<Vec<_>>()
        .join(" AND ");
    let values = tq
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("MAX({}) AS sd_v{i}", c.sql()))
        .collect::<Vec<_>>()
        .join(", ");
    (select_keys, group_by, join_on, values)
}

fn resolve_slot(
    slot: &TupleSlot,
    tq: &TupleQuery,
    table: &Table,
    alias: &str,
) -> Result<ResolvedTupleSlot, SkipReason> {
    let (value, kind, nullable) = match &slot.source {
        TupleSource::Col(i) => {
            let col = tq
                .columns
                .get(*i)
                .and_then(|n| table.columns.iter().find(|c| c.name == *n))
                .ok_or(SkipReason::UnknownObjectDependency)?;
            (
                format!("{alias}.sd_v{i}"),
                Some(col.kind.clone()),
                col.nullable,
            )
        }
        TupleSource::Const(v) => (fallback_sql(v)?, const_kind(v), const_nullable(v)),
    };
    Ok(ResolvedTupleSlot {
        var: slot.var.clone(),
        value,
        default_sql: fallback_sql(&slot.default)?,
        kind,
        nullable,
    })
}

/// 常量来源槽位的类型证据：Str→VARCHAR、Int→BIGINT、Null 不做列对齐。
fn const_kind(v: &SpinValue) -> Option<String> {
    match v {
        SpinValue::Str(_) => Some("VARCHAR".to_string()),
        SpinValue::Int(_) => Some("BIGINT".to_string()),
        _ => None,
    }
}

fn const_nullable(v: &SpinValue) -> bool {
    matches!(v, SpinValue::Null)
}

fn key_checks(tq: &TupleQuery, table: &Table) -> Result<Vec<(String, Name)>, SkipReason> {
    let mut checks = Vec::new();
    for corr in &tq.correlations {
        let Some(key) = table.columns.iter().find(|c| c.name == corr.dim_column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        checks.push((key.kind.clone(), corr.cursor_field.clone()));
    }
    Ok(checks)
}
