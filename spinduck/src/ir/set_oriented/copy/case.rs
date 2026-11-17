//! CASE 写汇的字段证明与逐行 DECIMAL 参数绑定语义。

use super::{field_projection, lookup, shape, source_column, Column, SkipReason, Table};

pub(super) fn value(
    case: &lookup::CaseSink,
    operand: &str,
    select: &shape::Select,
    source: &Table,
    sink: (&Column, bool),
) -> Result<String, SkipReason> {
    let (to, allow_filter) = sink;
    let columns = case
        .whens
        .iter()
        .map(|(_, field)| field)
        .chain(std::iter::once(&case.else_field))
        .map(|field| source_column(select, source, field).map(|column| (field, column)))
        .collect::<Result<Vec<_>, _>>()?;
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

fn raw_value(case: &lookup::CaseSink, operand: &str) -> String {
    let whens = case
        .whens
        .iter()
        .map(|(key, field)| format!("WHEN {key} THEN sd_cur.{}", field.sql()))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "CASE {operand} {whens} ELSE sd_cur.{} END",
        case.else_field.sql()
    )
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

fn decimal(kind: &str) -> Option<(u8, u8)> {
    let body = kind.strip_prefix("decimal(")?.strip_suffix(')')?;
    let (precision, scale) = body.split_once(',')?;
    Some((precision.trim().parse().ok()?, scale.trim().parse().ok()?))
}
