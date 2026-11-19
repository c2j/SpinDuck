//! §3.4 聚合字段覆写投影：外层 IF 门 CASE → 内层门 CASE →
//! `COALESCE(sd_agN.sd_v0, 默认)`；未命中/门假走外层默认，IF 假走原字段。

use super::super::schema::Column;
use super::super::shape::Name;
use super::super::{agg, expr, CopyLoop, SkipReason};
use super::inline::ProjEnv;
use super::{field_projection, source_column};

/// §3.4 聚合字段覆写投影：外层 IF 门 CASE → 内层门 CASE →
/// `COALESCE(sd_agN.sd_v0, 默认)`；未命中/门假走外层默认，IF 假走原字段。
pub(super) fn agg_projection(
    region: &CopyLoop<'_>,
    to: &Column,
    field: &Name,
    env: &mut ProjEnv<'_>,
) -> Result<Option<expr::GuardedExpr>, SkipReason> {
    let Some(fw) = region.field_writes.iter().find(|w| w.field == *field) else {
        return Ok(None);
    };
    let Some(ra) = env.aggs.iter().find(|a| a.var == fw.var) else {
        return Err(SkipReason::UnsupportedControlFlow);
    };
    for f in &ra.cursor_fields {
        source_column(env.select, env.source, f)?;
    }
    for (key_kind, cursor_field) in &ra.key_checks {
        let from = source_column(env.select, env.source, cursor_field)?;
        if &from.kind != key_kind {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
    }
    // ELSE 分支 = 原字段直通（含类型/可空对齐证明）。
    let else_sql = field_projection(env.select, env.source, to, field)?;
    if let Some(kind) = &ra.value_kind {
        if *kind != to.kind || (ra.value_nullable && !to.nullable) {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
    } else if ra.value_nullable && !to.nullable {
        return Err(SkipReason::UnprovenTargetConstraints);
    }
    if !env.ctes.contains(&ra.cte) {
        env.ctes.push(ra.cte.clone());
    }
    if !env.joins.contains(&ra.join) {
        env.joins.push(ra.join.clone());
    }
    let inner = if ra.gates.is_empty() {
        ra.value.clone()
    } else {
        format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            ra.gates, ra.value, ra.outer_default
        )
    };
    let expr_sql = match &fw.gate {
        Some(g) => {
            let gs = agg::gate_sql(g, region.record, env.select, env.source)?;
            format!("CASE WHEN {gs} THEN ({inner}) ELSE {else_sql} END")
        }
        None => inner,
    };
    Ok(Some(expr::GuardedExpr {
        expr: expr_sql,
        guard: String::new(),
    }))
}
