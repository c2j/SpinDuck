//! 内联表达式的绑定替换与入口无抛错守卫。

use super::{expr, group_hits, lookup, register_lookup, sd_cur_scan, shape, substitute_binds};
use super::{BindSrc, Column, CopyLoop, Name, SkipReason, Table};

pub(super) struct ProjEnv<'a> {
    pub select: &'a shape::Select,
    pub source: &'a Table,
    pub lookups: &'a [lookup::ResolvedLookup],
    pub ctes: &'a mut Vec<String>,
    pub joins: &'a mut Vec<String>,
    pub expr_guards: &'a mut Vec<String>,
}

pub(super) fn numeric_projection(
    region: &CopyLoop<'_>,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
) -> Result<expr::BoundExpr, SkipReason> {
    if !matches!(value.1, crate::ir::IrExpr::SqlFn { name, .. }
        if name.trim_start_matches('@').eq_ignore_ascii_case("intervaltonum"))
    {
        return Err(SkipReason::UnprovenWriteCompleteness);
    }
    let column = Column {
        name: Name("sd_inline".into()),
        kind: "bigint".into(),
        nullable: true,
        default: None,
    };
    Ok(expr::BoundExpr {
        value: inline_projection(region, &column, value, env)?,
        domain: expr::Domain::Numeric,
    })
}

pub(super) fn inline_projection(
    region: &CopyLoop<'_>,
    to: &Column,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
) -> Result<expr::GuardedExpr, SkipReason> {
    Ok(expr::at_write_sink(checked_value(region, value, env)?, to))
}

pub(super) fn checked_value(
    region: &CopyLoop<'_>,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
) -> Result<String, SkipReason> {
    checked_value_at(region, value, env, "")
}

pub(super) fn check_evaluation(
    region: &CopyLoop<'_>,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
) -> Result<(), SkipReason> {
    check_evaluation_at(region, value, env, "")
}

fn check_evaluation_at(
    region: &CopyLoop<'_>,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
    reachable: &str,
) -> Result<(), SkipReason> {
    use crate::ir::IrExpr;
    if super::inputs::total(value.1, region, env) {
        return Ok(());
    }
    if let IrExpr::Case {
        operand: None,
        whens,
        else_expr: None,
    } = value.1
    {
        if let [(cond, expr)] = whens.as_slice() {
            let cond = checked_value_at(region, (value.0, cond), env, reachable)?;
            let reachable = conjunction(reachable, &total_sql(&cond)?);
            return check_evaluation_at(region, (value.0, expr), env, &reachable);
        }
    }
    checked_value_at(region, value, env, reachable)?;
    Ok(())
}

fn total_sql(sql: &str) -> Result<String, SkipReason> {
    use ogsql_parser::token::tokenizer::Tokenizer;
    let tokens = Tokenizer::new(sql)
        .tokenize()
        .map_err(|_| SkipReason::UnprovenExpression)?;
    let mut result = String::new();
    let mut copied = 0;
    for pair in tokens.windows(2) {
        let span = &pair[0].span;
        let word = &sql[span.start..span.end];
        let next = &pair[1].span;
        if &sql[next.start..next.end] != "(" {
            continue;
        }
        let replacement = if word.eq_ignore_ascii_case("cast") {
            "TRY_CAST"
        } else if word.eq_ignore_ascii_case("strptime") {
            "TRY_STRPTIME"
        } else {
            continue;
        };
        result.push_str(&sql[copied..span.start]);
        result.push_str(replacement);
        copied = span.end;
    }
    result.push_str(&sql[copied..]);
    Ok(result)
}

fn conjunction(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_string(),
        (_, true) => left.to_string(),
        (false, false) => format!("({left}) AND ({right})"),
    }
}

fn checked_value_at(
    region: &CopyLoop<'_>,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
    reachable: &str,
) -> Result<String, SkipReason> {
    let (var, expr) = value;
    if !region.allow_filter {
        return Err(SkipReason::UnsupportedWrite);
    }
    if !inline_safe(expr) {
        return Err(SkipReason::UnprovenExpression);
    }
    if let crate::ir::IrExpr::IsNull(inner) | crate::ir::IrExpr::Not(inner) = expr {
        check_evaluation_at(region, (var, inner), env, reachable)?;
    }
    if let crate::ir::IrExpr::Bin { l, r, .. } = expr {
        check_evaluation_at(region, (var, l), env, reachable)?;
        check_evaluation_at(region, (var, r), env, reachable)?;
    }
    check_case_evaluations(region, value, env, reachable)?;
    let (text, binds) =
        crate::sql::render::expr_to_scalar_sql_binds(expr).ok_or(SkipReason::UnprovenExpression)?;
    let mut guard_ctes: Vec<String> = Vec::new();
    let mut guard_joins: Vec<String> = Vec::new();
    // 长占位符优先，避免 `$v_x` 误伤 `$v_x__f`；单遍引号感知替换，
    // 字面量内的同名占位符文本不被污染（§4.6 值等价）。
    let mut ordered: Vec<_> = binds.iter().collect();
    ordered.sort_by_key(|x| std::cmp::Reverse(x.0.len()));
    let mut repls = Vec::with_capacity(ordered.len());
    for (ph, src) in ordered {
        let repl = inline_binding(region, src, env, (&mut guard_ctes, &mut guard_joins))?;
        repls.push((ph.clone(), repl));
    }
    let mut out = substitute_binds(&text, &repls);
    let hits = reachable_lookups(region, var, env, (&mut guard_ctes, &mut guard_joins))?;
    let hits = conjunction(&hits, reachable);
    let total = total_sql(&out)?;
    if total != out {
        append_entry_guard(&total, env, guard_ctes, guard_joins, &hits);
    }
    if !hits.is_empty() {
        out = format!("CASE WHEN {hits} THEN ({out}) ELSE NULL END");
    }
    Ok(out)
}

fn check_case_evaluations(
    region: &CopyLoop<'_>,
    value: (&super::super::VarName, &crate::ir::IrExpr),
    env: &mut ProjEnv<'_>,
    reachable: &str,
) -> Result<(), SkipReason> {
    use crate::ir::{BinOp, IrExpr};
    let IrExpr::Case {
        operand,
        whens,
        else_expr,
    } = value.1
    else {
        return Ok(());
    };
    if let Some(operand) = operand {
        check_evaluation_at(region, (value.0, operand), env, reachable)?;
    }
    let mut pending = reachable.to_string();
    for (condition, result) in whens {
        let condition = match operand {
            Some(operand) => IrExpr::Bin {
                op: BinOp::Eq,
                l: operand.clone(),
                r: Box::new(condition.clone()),
            },
            None => condition.clone(),
        };
        let condition = checked_value_at(region, (value.0, &condition), env, &pending)?;
        let hit = total_sql(&condition)?;
        let selected = conjunction(&pending, &hit);
        check_evaluation_at(region, (value.0, result), env, &selected)?;
        pending = conjunction(&pending, &format!("({hit}) IS NOT TRUE"));
    }
    if let Some(result) = else_expr {
        check_evaluation_at(region, (value.0, result), env, &pending)?;
    }
    Ok(())
}

fn reachable_lookups(
    region: &CopyLoop<'_>,
    var: &super::super::VarName,
    env: &mut ProjEnv<'_>,
    guard: (&mut Vec<String>, &mut Vec<String>),
) -> Result<String, SkipReason> {
    let required = region.inline_preconditions.get(&var.0);
    let prerequisites: Vec<_> = env
        .lookups
        .iter()
        .filter(|l| required.is_some_and(|vars| vars.contains(&l.var)))
        .collect();
    let hits = group_hits(&prerequisites, env.select, env.source)?;
    for l in prerequisites {
        register_lookup(l, env.lookups, env.ctes, env.joins);
        register_lookup(l, env.lookups, guard.0, guard.1);
    }
    Ok(hits)
}

fn append_entry_guard(
    total: &str,
    env: &mut ProjEnv<'_>,
    ctes: Vec<String>,
    joins: Vec<String>,
    hits: &str,
) {
    let with = std::iter::once(format!(
        "sd_cur AS ({})",
        sd_cur_scan(env.select, env.source)
    ))
    .chain(ctes)
    .collect::<Vec<_>>()
    .join(", ");
    let join_sql = if joins.is_empty() {
        String::new()
    } else {
        format!(" {}", joins.join(" "))
    };
    let reachable = if hits.is_empty() {
        String::new()
    } else {
        format!("({hits}) AND ")
    };
    env.expr_guards.push(format!(
        "WITH {with} SELECT COUNT(*) FROM sd_cur{join_sql} WHERE {reachable}({total}) IS NULL"
    ));
}

fn inline_binding(
    region: &CopyLoop<'_>,
    src: &BindSrc,
    env: &mut ProjEnv<'_>,
    guard: (&mut Vec<String>, &mut Vec<String>),
) -> Result<String, SkipReason> {
    match src {
        BindSrc::Field { rec, field } => {
            if rec != region.record {
                return Err(SkipReason::UnprovenExpression);
            }
            let name = Name(field.clone());
            super::source_column(env.select, env.source, &name)?;
            Ok(format!("sd_cur.{}", name.sql()))
        }
        BindSrc::Var(v) => {
            if let Some(l) = env.lookups.iter().find(|l| &l.var == v) {
                group_hits(&[l], env.select, env.source)?;
                register_lookup(l, env.lookups, env.ctes, env.joins);
                register_lookup(l, env.lookups, guard.0, guard.1);
                Ok(format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    l.render.hit, l.render.value, l.render.default_sql
                ))
            } else {
                let Some((_, c)) = region.folded.iter().find(|(fv, _)| fv == v) else {
                    return Err(SkipReason::UnprovenExpression);
                };
                crate::sql::render::const_to_sql(c).ok_or(SkipReason::UnprovenExpression)
            }
        }
    }
}

fn inline_safe(e: &crate::ir::IrExpr) -> bool {
    use crate::ir::{BinOp, IrExpr};
    match e {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => true,
        IrExpr::Bin { op, l, r } => match op {
            BinOp::Concat | BinOp::And | BinOp::Or => inline_safe(l) && inline_safe(r),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                inline_safe(l) && inline_safe(r)
            }
            BinOp::Add | BinOp::Sub => {
                (date_expr(l) && num_leaf(r)) || (*op == BinOp::Add && num_leaf(l) && date_expr(r))
            }
            BinOp::Mul | BinOp::Div => false,
        },
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_deref().is_none_or(inline_safe)
                && whens.iter().all(|(c, v)| inline_safe(c) && inline_safe(v))
                && else_expr.as_deref().is_none_or(inline_safe)
        }
        IrExpr::Not(x) | IrExpr::IsNull(x) => inline_safe(x),
        IrExpr::SqlFn {
            name,
            args,
            prepared,
        } => {
            prepared.is_ok()
                && if name
                    .trim_start_matches('@')
                    .eq_ignore_ascii_case("intervaltonum")
                {
                    matches!(args.as_slice(), [IrExpr::Bin { op: BinOp::Sub, l, r }]
                        if date_diff_operand(l) && date_diff_operand(r))
                } else {
                    is_date_fn(name) && args.iter().all(inline_safe)
                }
        }
        IrExpr::Lookup { args, default, .. } => {
            args.iter().all(|(_, a)| inline_safe(a)) && inline_safe(default)
        }
        IrExpr::FuncCall { .. } | IrExpr::CursorAttr { .. } => false,
    }
}

fn date_diff_operand(e: &crate::ir::IrExpr) -> bool {
    matches!(e, crate::ir::IrExpr::SqlFn { name, .. }
        if name.trim_start_matches('@').eq_ignore_ascii_case("to_date") && inline_safe(e))
}

fn date_expr(e: &crate::ir::IrExpr) -> bool {
    match e {
        crate::ir::IrExpr::SqlFn { name, .. } => is_date_fn(name),
        _ => false,
    }
}

fn is_date_fn(name: &str) -> bool {
    let bare = name.trim_start_matches('@');
    ["to_date", "to_char", "intervaltonum"]
        .iter()
        .any(|n| bare.eq_ignore_ascii_case(n))
}

fn num_leaf(e: &crate::ir::IrExpr) -> bool {
    matches!(
        e,
        crate::ir::IrExpr::Const(crate::value::SpinValue::Int(_))
            | crate::ir::IrExpr::Var(_)
            | crate::ir::IrExpr::Field { .. }
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn should_totalize_only_fallible_function_tokens() {
        let sql = "CAST(x AS BIGINT) + TRY_CAST(y AS BIGINT) || 'CAST(' || 'strptime('";
        let expected = "TRY_CAST(x AS BIGINT) + TRY_CAST(y AS BIGINT) || 'CAST(' || 'strptime('";
        assert_eq!(super::total_sql(sql).unwrap(), expected);
    }

    #[test]
    fn should_keep_totalization_idempotent() {
        let sql = "cast(x AS BIGINT) + TRY_CAST(y AS BIGINT) || strptime(d, '%Y%m%d')";
        let expected = "TRY_CAST(x AS BIGINT) + TRY_CAST(y AS BIGINT) || TRY_STRPTIME(d, '%Y%m%d')";
        let once = super::total_sql(sql).unwrap();
        assert_eq!(once, expected);
        assert_eq!(super::total_sql(&once).unwrap(), expected);
    }
}
