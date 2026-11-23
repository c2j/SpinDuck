//! 相关聚合 LATERAL 渲染（§4.4 B1）：相关兜底查找的 IN 子查询本身相关
//! （引用游标字段）时，独立 GROUP BY 派生表无法引用 sd_cur，改逐行
//! LEFT JOIN LATERAL 聚合（MAX 取值 + COUNT 判恰 1 行），保留 EXISTS
//! 半连接语义与计数口径。非 Top-1：多行 → COUNT>1 → hit false → 兜底。

use super::super::schema::Table;
use super::{InFilter, LookupQuery, LookupRender, Name, SkipReason};

/// 相关查找含相关 IN 子查询的逐行 LATERAL 聚合渲染。外层相关键与内层
/// 半连接相关键均代入 sd_cur.字段；形参 BETWEEN 绑定经 guard_binds 携带。
pub(super) fn render_correlated_lateral(
    lq: &LookupQuery,
    table: &Table,
    inner_tables: &[(Name, Table)],
    index: usize,
) -> Result<LookupRender, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if lq.correlations.is_empty() {
        return Err(bad);
    }
    let mut key_checks = Vec::new();
    for corr in &lq.correlations {
        let Some(key) = table.columns.iter().find(|c| c.name == corr.dim_column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        key_checks.push((key.kind.clone(), corr.cursor_field.clone()));
    }
    let d = "sd_d";
    let mut preds: Vec<String> = Vec::new();
    for corr in &lq.correlations {
        preds.push(format!(
            "{d}.{} {} sd_cur.{}",
            corr.dim_column.sql(),
            corr.op.sql(),
            corr.cursor_field.sql()
        ));
    }
    if !lq.filter.is_empty() {
        preds.push(lq.filter.trim_start_matches(" WHERE ").to_string());
    }
    for f in &lq.in_filters {
        preds.push(in_semi_pred(f, d, table, inner_tables, &mut key_checks)?);
    }
    let where_sql = format!(" WHERE {}", preds.join(" AND "));
    let alias = format!("sd_lk{index}");
    let col = lq.column.sql();
    let join = format!(
        "LEFT JOIN LATERAL (SELECT MAX({d}.{col}) AS sd_val, COUNT(*) AS sd_cnt \
         FROM {} {d}{where_sql}) {alias} ON TRUE",
        table.qualified
    );
    let value = match &lq.nvl_default {
        Some(def) => format!("CASE WHEN {alias}.sd_val IS NULL THEN {def} ELSE {alias}.sd_val END"),
        None => format!("{alias}.sd_val"),
    };
    Ok(LookupRender {
        cte: None,
        join: Some(join),
        hit: format!("{alias}.sd_cnt = 1"),
        value,
        default_sql: lq.default_sql.clone(),
        key_checks,
        tie_guard: None,
        guard_binds: lq.filter_binds.clone(),
        miss_guards: Vec::new(),
        miss_out: None,
        exc_guard: None,
    })
}

/// 单个 IN 半连接谓词：`d.col IN (SELECT inner_col FROM inner WHERE ...)`。
/// 内层相关键代入 sd_cur.字段并纳入 key_checks；半连接两侧列严格同型
/// （杜绝隐式 CAST 报错点）；内层静态过滤（含 BETWEEN $v_ 占位）原样携带。
fn in_semi_pred(
    f: &InFilter,
    d: &str,
    table: &Table,
    inner_tables: &[(Name, Table)],
    key_checks: &mut Vec<(String, Name)>,
) -> Result<String, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let Some((_, inner)) = inner_tables.iter().find(|(n, _)| *n == f.table) else {
        return Err(SkipReason::UnknownObjectDependency);
    };
    let Some(oc) = table.columns.iter().find(|c| c.name == f.column) else {
        return Err(SkipReason::UnknownObjectDependency);
    };
    let Some(ic) = inner.columns.iter().find(|c| c.name == f.inner_column) else {
        return Err(SkipReason::UnknownObjectDependency);
    };
    if oc.kind != ic.kind {
        return Err(bad);
    }
    let mut ip: Vec<String> = Vec::new();
    for corr in &f.correlations {
        let Some(k) = inner.columns.iter().find(|c| c.name == corr.dim_column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        key_checks.push((k.kind.clone(), corr.cursor_field.clone()));
        ip.push(format!(
            "{} = sd_cur.{}",
            corr.dim_column.sql(),
            corr.cursor_field.sql()
        ));
    }
    if !f.filter.is_empty() {
        ip.push(f.filter.trim_start_matches(" WHERE ").to_string());
    }
    let inner_where = if ip.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", ip.join(" AND "))
    };
    Ok(format!(
        "{d}.{} IN (SELECT {} FROM {}{inner_where})",
        f.column.sql(),
        f.inner_column.sql(),
        inner.qualified
    ))
}
