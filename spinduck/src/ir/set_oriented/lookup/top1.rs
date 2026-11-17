//! 有序 Top-1 渲染（§4.4 B1）：独立窗口 CTE（单相关键、静态过滤）与
//! 游标字段 IN-list 的逐行 LATERAL 两形态；入口并列一致性守卫均以
//! RANK + FIRST_VALUE + IS DISTINCT FROM 判定（严禁 COUNT(DISTINCT)/MIN=MAX）。

use super::super::schema::Table;
use super::{InListItem, LookupQuery, LookupRender, Name, SkipReason, Top1};

fn validate_order_columns(top1: &Top1, table: &Table) -> Result<(), SkipReason> {
    if top1.order_by.is_empty()
        || top1
            .order_by
            .iter()
            .any(|(c, _)| !table.columns.iter().any(|tc| tc.name == *c))
    {
        return Err(SkipReason::UnknownObjectDependency);
    }
    Ok(())
}

fn lateral_key_checks(lq: &LookupQuery, table: &Table) -> Result<Vec<(String, Name)>, SkipReason> {
    let mut checks = Vec::new();
    for corr in &lq.correlations {
        let Some(key) = table.columns.iter().find(|c| c.name == corr.dim_column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        checks.push((key.kind.clone(), corr.cursor_field.clone()));
    }
    for il in &lq.in_lists {
        let Some(ic) = table.columns.iter().find(|c| c.name == il.column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        for it in &il.items {
            match it {
                InListItem::Field(f) => checks.push((ic.kind.clone(), f.clone())),
                InListItem::Expr { base_field, .. } => {
                    if ic.kind != "varchar" {
                        return Err(SkipReason::UnprovenQueryTotalness);
                    }
                    checks.push((ic.kind.clone(), base_field.clone()));
                }
                InListItem::Lit(_) => {}
            }
        }
    }
    Ok(checks)
}

fn lateral_filter(lq: &LookupQuery, d: &str) -> String {
    let mut preds = Vec::new();
    for corr in &lq.correlations {
        preds.push(format!(
            "{d}.{} = sd_cur.{}",
            corr.dim_column.sql(),
            corr.cursor_field.sql()
        ));
    }
    for il in &lq.in_lists {
        let items = il
            .items
            .iter()
            .map(|it| match it {
                InListItem::Field(f) => format!("sd_cur.{}", f.sql()),
                InListItem::Lit(s) => s.clone(),
                InListItem::Expr { sql, .. } => sql.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        preds.push(format!("{d}.{} IN ({items})", il.column.sql()));
    }
    if !lq.filter.is_empty() {
        preds.push(lq.filter.trim_start_matches(" WHERE ").to_string());
    }
    format!(" WHERE {}", preds.join(" AND "))
}

/// 有序 Top-1 渲染：按组 row_number 首行派生表（sd_hit 标记区分
/// 命中 NULL 与无匹配）；入口守卫用 RANK 识别全部并列首名，
/// FIRST_VALUE 取首行值（NULL 安全），IS DISTINCT FROM 逐组校验
/// 异值并列——严禁 COUNT(DISTINCT)/MIN=MAX（漏检 {'1', NULL}）。
pub(super) fn render_top1(
    lq: &LookupQuery,
    table: &Table,
    filter: &str,
    index: usize,
) -> Result<LookupRender, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let [corr] = lq.correlations.as_slice() else {
        return Err(bad);
    };
    let top1 = lq.top1.as_ref().ok_or(bad)?;
    let Some(key) = table.columns.iter().find(|c| c.name == corr.dim_column) else {
        return Err(SkipReason::UnknownObjectDependency);
    };
    validate_order_columns(top1, table)?;
    let alias = format!("sd_lk{index}");
    let dim = corr.dim_column.sql();
    let col = lq.column.sql();
    let ord = top1
        .order_by
        .iter()
        .map(|(c, desc)| format!("{} {}", c.sql(), if *desc { "DESC" } else { "ASC" }))
        .collect::<Vec<_>>()
        .join(", ");
    let window = format!("PARTITION BY {dim} ORDER BY {ord}");
    let cte = format!(
        "{alias} AS (SELECT sd_key, sd_val, 1 AS sd_hit FROM \
         (SELECT {dim} AS sd_key, {col} AS sd_val, ROW_NUMBER() OVER ({window}) AS sd_rn \
         FROM {}{filter}) sd_ranked WHERE sd_rn = 1)",
        table.qualified
    );
    let tie_guard = format!(
        "SELECT COUNT(*) FROM (SELECT sd_key FROM \
         (SELECT {dim} AS sd_key, {col} AS sd_val, RANK() OVER ({window}) AS sd_rnk, \
         FIRST_VALUE({col}) OVER ({window}) AS sd_top1 FROM {}{filter}) sd_ties \
         GROUP BY sd_key HAVING COUNT(*) FILTER \
         (WHERE sd_rnk = 1 AND sd_val IS DISTINCT FROM sd_top1) > 0)",
        table.qualified
    );
    Ok(LookupRender {
        cte: Some(cte),
        join: Some(format!(
            "LEFT JOIN {alias} ON {alias}.sd_key = sd_cur.{}",
            corr.cursor_field.sql()
        )),
        hit: format!("{alias}.sd_hit = 1"),
        value: match &lq.nvl_default {
            Some(d) => format!("CASE WHEN {alias}.sd_val IS NULL THEN {d} ELSE {alias}.sd_val END"),
            None => format!("{alias}.sd_val"),
        },
        default_sql: lq.default_sql.clone(),
        key_checks: vec![(key.kind.clone(), corr.cursor_field.clone())],
        tie_guard: Some(tie_guard),
        guard_binds: lq.filter_binds.clone(),
    })
}

/// 游标字段 IN-list 有序 Top-1 的 LATERAL 渲染：逐行 LEFT JOIN LATERAL
/// 取首行（sd_hit 标记区分命中 NULL 与无匹配）；入口并列守卫亦按行
/// LATERAL 聚合（RANK + FIRST_VALUE + IS DISTINCT FROM，严禁
/// COUNT(DISTINCT)/MIN=MAX）。cur_scan 为游标扫描投影（守卫自带 sd_cur，
/// 空串 = 内联提升上下文无游标形状 → 保守拒绝）。仅相关 Top-1，禁 IN
/// 子查询/形参绑定混用（守卫以空绑定执行，须自包含）。
pub(super) fn render_top1_lateral(
    lq: &LookupQuery,
    table: &Table,
    cur_scan: &str,
    index: usize,
) -> Result<LookupRender, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if cur_scan.is_empty() || !lq.in_filters.is_empty() {
        return Err(bad);
    }
    let top1 = lq.top1.as_ref().ok_or(bad)?;
    if lq.correlations.is_empty() || top1.order_by.is_empty() {
        return Err(bad);
    }
    let key_checks = lateral_key_checks(lq, table)?;
    validate_order_columns(top1, table)?;
    let alias = format!("sd_lk{index}");
    let d = "sd_d";
    let col = lq.column.sql();
    let where_sql = lateral_filter(lq, d);
    let ord = top1
        .order_by
        .iter()
        .map(|(c, desc)| format!("{d}.{} {}", c.sql(), if *desc { "DESC" } else { "ASC" }))
        .collect::<Vec<_>>()
        .join(", ");
    let join = format!(
        "LEFT JOIN LATERAL (SELECT {d}.{col} AS sd_val, 1 AS sd_hit \
         FROM {} {d}{where_sql} ORDER BY {ord} LIMIT 1) {alias} ON TRUE",
        table.qualified
    );
    let window = format!("ORDER BY {ord}");
    let tie_guard = format!(
        "WITH sd_cur AS ({cur_scan}) SELECT COUNT(*) FROM sd_cur LEFT JOIN LATERAL \
         (SELECT COUNT(*) AS sd_bad FROM (SELECT {d}.{col} AS sd_val, \
         RANK() OVER ({window}) AS sd_rnk, FIRST_VALUE({d}.{col}) OVER ({window}) AS sd_top1 \
         FROM {} {d}{where_sql}) sd_t WHERE sd_rnk = 1 AND sd_val IS DISTINCT FROM sd_top1) \
         sd_g ON TRUE WHERE sd_g.sd_bad > 0",
        table.qualified
    );
    Ok(LookupRender {
        cte: None,
        join: Some(join),
        hit: format!("{alias}.sd_hit = 1"),
        value: match &lq.nvl_default {
            Some(def) => {
                format!("CASE WHEN {alias}.sd_val IS NULL THEN {def} ELSE {alias}.sd_val END")
            }
            None => format!("{alias}.sd_val"),
        },
        default_sql: lq.default_sql.clone(),
        key_checks,
        tie_guard: Some(tie_guard),
        guard_binds: lq.filter_binds.clone(),
    })
}
