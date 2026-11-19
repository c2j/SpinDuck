//! CASE 写汇的字段证明与逐行 DECIMAL 参数绑定语义。

use super::inline::ProjEnv;
use super::lookup::CaseElse;
use super::{
    expr, field_projection, group_hits, lookup, register_lookup, shape, source_column, Column,
    CopyLoop, SkipReason, Table,
};

pub(super) fn value(
    case: &lookup::CaseSink,
    operand: &str,
    select: &shape::Select,
    source: &Table,
    sink: (&Column, bool),
) -> Result<String, SkipReason> {
    let (to, allow_filter) = sink;
    let mut columns = case
        .whens
        .iter()
        .map(|(_, field)| field)
        .map(|field| source_column(select, source, field).map(|column| (field, column)))
        .collect::<Result<Vec<_>, _>>()?;
    if let CaseElse::Field(field) = &case.else_value {
        columns.push((field, source_column(select, source, field)?));
    }
    if allow_filter
        && decimal(&to.kind).is_some()
        && columns.iter().all(|(_, c)| decimal(&c.kind).is_some())
    {
        return decimal_value(case, operand, &columns);
    }
    let mut target = if allow_filter {
        columns[0].1.clone()
    } else {
        to.clone()
    };
    target.nullable |= allow_filter;
    for (field, _) in &columns {
        field_projection(select, source, &target, field)?;
    }
    Ok(raw_value(case, operand))
}

/// CASE 写汇投影：操作数查找按自身组命中门控取值（落空取兜底常量），
/// 注册其组派生表/连接；裸 render.value 在落空时为 NULL 会漂移。
pub(super) fn sink(
    region: &CopyLoop<'_>,
    lq: &lookup::ResolvedLookup,
    cs: &lookup::CaseSink,
    hits: String,
    to: &Column,
    env: &mut ProjEnv<'_>,
) -> Result<expr::GuardedExpr, SkipReason> {
    let operand = env
        .lookups
        .iter()
        .find(|l| l.var == cs.operand)
        .ok_or(SkipReason::UnprovenExpression)?;
    if operand.filter_on_miss || operand.case_sink.is_some() {
        return Err(SkipReason::UnprovenExpression);
    }
    let omembers: Vec<_> = env
        .lookups
        .iter()
        .filter(|l| l.group == operand.group)
        .collect();
    let ohits = group_hits(&omembers, env.select, env.source)?;
    for member in &omembers {
        register_lookup(member, env.lookups, env.ctes, env.joins);
    }
    let operand_sql = format!(
        "CASE WHEN {ohits} THEN {} ELSE {} END",
        operand.render.value, operand.render.default_sql
    );
    let value = value(
        cs,
        &operand_sql,
        env.select,
        env.source,
        (to, region.allow_filter),
    )?;
    if region.allow_filter {
        return Ok(super::guarded_choice(
            &hits,
            value,
            &lq.render.default_sql,
            to,
        ));
    }
    Ok(expr::GuardedExpr {
        expr: format!(
            "CASE WHEN {hits} THEN {value} ELSE {} END",
            lq.render.default_sql
        ),
        guard: String::new(),
    })
}

fn raw_value(case: &lookup::CaseSink, operand: &str) -> String {
    let whens = case
        .whens
        .iter()
        .map(|(key, field)| format!("WHEN {key} THEN sd_cur.{}", field.sql()))
        .collect::<Vec<_>>()
        .join(" ");
    let els = match &case.else_value {
        CaseElse::Field(field) => format!("sd_cur.{}", field.sql()),
        CaseElse::Lit(lit) => lit.clone(),
    };
    format!("CASE {operand} {whens} ELSE {els} END")
}

fn decimal_value(
    case: &lookup::CaseSink,
    operand: &str,
    columns: &[(&shape::Name, &Column)],
) -> Result<String, SkipReason> {
    let mut scales = std::collections::BTreeMap::<u8, Vec<String>>::new();
    let mut integer_digits = 0;
    for (field, column) in columns {
        let (precision, scale) = decimal(&column.kind).ok_or(SkipReason::UnprovenExpression)?;
        integer_digits = integer_digits.max(precision - scale);
        scales
            .entry(scale)
            .or_default()
            .push(format!("sd_cur.{} IS NOT NULL", field.sql()));
    }
    let value = raw_value(case, operand);
    if scales.len() == 1 {
        return Ok(value);
    }
    let max_scale = *scales
        .last_key_value()
        .ok_or(SkipReason::UnprovenExpression)?
        .0;
    let width = integer_digits + max_scale + 1;
    if width > 38 {
        return Err(SkipReason::UnprovenExpression);
    }
    let ty = format!("DECIMAL({width},{max_scale})");
    // Off 将非 NULL DECIMAL 绑定为精度 38，CASE 公共类型取其中最小 scale。
    let whens = scales
        .iter()
        .filter(|(scale, _)| **scale != max_scale)
        .map(|(scale, present)| {
            format!(
                "WHEN {} THEN CAST(round(({value}), {scale}) AS {ty})",
                present.join(" OR ")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!("CASE {whens} ELSE CAST(({value}) AS {ty}) END"))
}

pub(super) fn decimal(kind: &str) -> Option<(u8, u8)> {
    let body = kind.strip_prefix("decimal(")?.strip_suffix(')')?;
    let (precision, scale) = body.split_once(',')?;
    Some((precision.trim().parse().ok()?, scale.trim().parse().ok()?))
}
