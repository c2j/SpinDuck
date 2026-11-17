//! 查找聚合渲染：单值门控与相关键分组。

use super::super::schema::Table;
use super::{LookupQuery, LookupRender, Name, SkipReason};

pub(super) fn case_gate(lq: &LookupQuery, table: &Table) -> LookupRender {
    LookupRender {
        cte: None,
        join: None,
        hit: format!("(SELECT COUNT(*) FROM {}) = 1", table.qualified),
        value: String::new(),
        default_sql: lq.default_sql.clone(),
        key_checks: Vec::new(),
        tie_guard: None,
        guard_binds: Vec::new(),
    }
}

pub(super) fn scalar(lq: &LookupQuery, table: &Table, filter: &str) -> LookupRender {
    LookupRender {
        cte: None,
        join: None,
        hit: format!("(SELECT COUNT(*) FROM {}{filter}) = 1", table.qualified),
        value: nvl(
            lq,
            format!(
                "(SELECT MAX({}) FROM {}{filter})",
                lq.column.sql(),
                table.qualified
            ),
        ),
        default_sql: lq.default_sql.clone(),
        key_checks: Vec::new(),
        tie_guard: None,
        guard_binds: Vec::new(),
    }
}

pub(super) fn grouped(
    lq: &LookupQuery,
    table: &Table,
    filter: &str,
    index: usize,
) -> Result<LookupRender, SkipReason> {
    let key_checks = key_checks(lq, table)?;
    let alias = format!("sd_lk{index}");
    let single = lq.correlations.len() == 1;
    let key_alias = |i: usize| {
        if single {
            "sd_key".to_string()
        } else {
            format!("sd_key{i}")
        }
    };
    let select_keys = lq
        .correlations
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{} AS {}", c.dim_column.sql(), key_alias(i)))
        .collect::<Vec<_>>()
        .join(", ");
    let group_by = lq
        .correlations
        .iter()
        .map(|c| c.dim_column.sql())
        .collect::<Vec<_>>()
        .join(", ");
    let join_on = lq
        .correlations
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{alias}.{} = sd_cur.{}", key_alias(i), c.cursor_field.sql()))
        .collect::<Vec<_>>()
        .join(" AND ");
    Ok(LookupRender {
        cte: Some(format!(
            "{alias} AS (SELECT {select_keys}, MAX({}) AS sd_val, COUNT(*) AS sd_cnt \
             FROM {}{filter} GROUP BY {group_by})",
            lq.column.sql(),
            table.qualified
        )),
        join: Some(format!("LEFT JOIN {alias} ON {join_on}")),
        hit: format!("{alias}.sd_cnt = 1"),
        value: nvl(lq, format!("{alias}.sd_val")),
        default_sql: lq.default_sql.clone(),
        key_checks,
        tie_guard: None,
        guard_binds: Vec::new(),
    })
}

fn key_checks(lq: &LookupQuery, table: &Table) -> Result<Vec<(String, Name)>, SkipReason> {
    let mut checks = Vec::new();
    for corr in &lq.correlations {
        let Some(key) = table.columns.iter().find(|c| c.name == corr.dim_column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        checks.push((key.kind.clone(), corr.cursor_field.clone()));
    }
    Ok(checks)
}

fn nvl(lq: &LookupQuery, value: String) -> String {
    match &lq.nvl_default {
        Some(d) => format!("CASE WHEN {value} IS NULL THEN {d} ELSE {value} END"),
        None => value,
    }
}
