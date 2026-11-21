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
) -> Result<(String, String), SkipReason> {
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
        let (v, k, _) = decimal_value(case, operand, &columns)?;
        return Ok((v, k));
    }
    let mut target = if allow_filter {
        columns[0].1.clone()
    } else {
        to.clone()
    };
    target.nullable |= allow_filter;
    for (field, _) in &columns {
        // 值域守卫无法经 raw_value 透传——含内核域守卫的字段证明拒绝。
        if !field_projection(select, source, &target, field)?
            .kernel_guard
            .is_empty()
        {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
    }
    Ok((raw_value(case, operand), String::new()))
}

/// CASE 写汇操作数 → SQL：查找变量按自身组命中门控取值，
/// 内联例程返回值经 checked_value 渲染。
fn operand_sql(
    region: &CopyLoop<'_>,
    cs: &lookup::CaseSink,
    env: &mut ProjEnv<'_>,
) -> Result<String, SkipReason> {
    match env.lookups.iter().find(|l| l.var == cs.operand) {
        Some(operand) => {
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
            Ok(lookup::typed_case(
                &ohits,
                &operand.render.value,
                &operand.render.default_sql,
                &operand.kind,
            ))
        }
        None => {
            let Some((_, expr)) = region.inlined.iter().find(|(v, _)| *v == cs.operand) else {
                return Err(SkipReason::UnprovenExpression);
            };
            super::inline::checked_value(region, (&cs.operand, expr), env)
        }
    }
}

/// CASE 写汇的**值级**渲染（供 ArithWrite 基值等操作数绑定）：
/// 仅 DECIMAL 全定义路径——分支经 decimal_value 对齐到公共
/// DECIMAL 类型（混合标度时分支 CAST/round），返回 (值 SQL,
/// 内核域守卫, 公共标度)。非 DECIMAL 分支集无法证明值域，拒绝。
pub(super) fn sink_value(
    region: &CopyLoop<'_>,
    cs: &lookup::CaseSink,
    env: &mut ProjEnv<'_>,
) -> Result<(String, String, u8), SkipReason> {
    let operand_sql = operand_sql(region, cs, env)?;
    let mut columns = cs
        .whens
        .iter()
        .map(|(_, field)| field)
        .map(|field| source_column(env.select, env.source, field).map(|c| (field, c)))
        .collect::<Result<Vec<_>, _>>()?;
    if let CaseElse::Field(field) = &cs.else_value {
        columns.push((field, source_column(env.select, env.source, field)?));
    }
    if !region.allow_filter || !columns.iter().all(|(_, c)| decimal(&c.kind).is_some()) {
        return Err(SkipReason::UnprovenExpression);
    }
    decimal_value(cs, &operand_sql, &columns)
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
    let operand_sql = operand_sql(region, cs, env)?;
    let value = value(
        cs,
        &operand_sql,
        env.select,
        env.source,
        (to, region.allow_filter),
    )?;
    let (value, kernel_guard) = value;
    if region.allow_filter {
        let mut g = super::guarded_choice(&hits, value, &lq.render.default_sql, to);
        g.kernel_guard = kernel_guard;
        return Ok(g);
    }
    Ok(expr::GuardedExpr {
        expr: format!(
            "CASE WHEN {hits} THEN {value} ELSE {} END",
            lq.render.default_sql
        ),
        guard: String::new(),
        kernel_guard,
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
) -> Result<(String, String, u8), SkipReason> {
    let mut scales = std::collections::BTreeMap::<u8, Vec<String>>::new();
    let mut integer_digits = 0;
    for (field, column) in columns {
        let Some((precision, scale)) = decimal(&column.kind) else {
            return Err(SkipReason::UnprovenExpression);
        };
        integer_digits = integer_digits.max(precision - scale);
        scales
            .entry(scale)
            .or_default()
            .push(format!("sd_cur.{} IS NOT NULL", field.sql()));
    }
    let value = raw_value(case, operand);
    if scales.len() == 1 {
        let scale = *scales.keys().next().unwrap_or(&0);
        return Ok((value, String::new(), scale));
    }
    let max_scale = *scales
        .last_key_value()
        .ok_or(SkipReason::UnprovenExpression)?
        .0;
    // CASE 分支合并是值域并集而非算术：精度 = 最大整数位 + 最大标度，
    // 不加 combine 的 +1 进位余量（与 Domain::unify 同口径）。
    let width = integer_digits + max_scale;
    // 静态宽度超 38 时内核结果类型封顶 DECIMAL(38, max_scale)：整数位
    // 容量 = 38-max_scale。目标写汇 TRY_CAST 已把有效值域限制在远小于
    // 该容量的范围；仍对全部分支列发幅值准入守卫（DOUBLE 域求值永不
    // 抛错），越界行非零即整体回退复现解释器语义。
    let (ty, kernel_guard) = if width > 38 {
        let int_capacity = 38 - i64::from(max_scale);
        if int_capacity < 1 {
            return Err(SkipReason::UnprovenExpression);
        }
        let preds = columns
            .iter()
            .map(|(field, _)| {
                let col = format!("sd_cur.{}", field.sql());
                format!(
                    "(({col}) IS NULL) OR (abs(CAST(({col}) AS DOUBLE)) < 1e{})",
                    int_capacity - 1
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        (format!("DECIMAL(38,{max_scale})"), preds)
    } else {
        (format!("DECIMAL({width},{max_scale})"), String::new())
    };
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
    Ok((
        format!("CASE {whens} ELSE CAST(({value}) AS {ty}) END"),
        kernel_guard,
        max_scale,
    ))
}

pub(super) fn decimal(kind: &str) -> Option<(u8, u8)> {
    let body = kind.strip_prefix("decimal(")?.strip_suffix(')')?;
    let (precision, scale) = body.split_once(',')?;
    Some((precision.trim().parse().ok()?, scale.trim().parse().ok()?))
}
