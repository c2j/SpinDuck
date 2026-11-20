//! 已证明 CopyLoop 的批写 SQL 生成与写入边界守卫。

mod agg_proj;
mod arith;
mod badrow;
mod case;
mod inline;
mod inputs;

use super::schema::{Column, Default, Table};
use super::shape::{self, Name};
use super::{agg, entry, expr, lookup, substitute_binds, BindSrc, CopyLoop, SinkOp, SkipReason};
use badrow::sd_cur_cte;
use inline::{inline_projection, ProjEnv};

pub(super) struct CopySql {
    pub sql: String,
    pub expr_guards: Vec<String>,
    /// INSERT 位 fn_site 的 distinct-over-sd_cur 前导查询（needs_sd_cur）。
    pub fn_preludes: Vec<entry::FnPrelude>,
}

pub(super) fn copy_sql(
    region: &CopyLoop<'_>,
    shapes: (&shape::Select, &shape::Insert),
    tables: (&Table, &Table),
    lookups: &[lookup::ResolvedLookup],
    tuples: &[lookup::ResolvedTuple],
    aggs: &[agg::ResolvedAgg],
) -> Result<CopySql, SkipReason> {
    let (select, insert) = shapes;
    let (source, target) = tables;
    prove_omitted_columns(insert, target)?;
    let mut guards = Vec::new();
    let mut expr_guards = Vec::new();
    let mut ctes = Vec::new();
    let mut joins = Vec::new();
    let mut env = ProjEnv {
        select,
        source,
        lookups,
        tuples,
        aggs,
        ctes: &mut ctes,
        joins: &mut joins,
        expr_guards: &mut expr_guards,
    };
    inputs::prove(region, &env)?;
    for l in lookups.iter().filter(|l| l.filter_on_miss) {
        guards.push(group_hits(&[l], select, source)?);
        register_lookup(l, lookups, env.ctes, env.joins);
    }
    for (var, value) in &region.inline_checks {
        inline::check_evaluation(region, (var, value), &mut env)?;
    }
    let (projections, write_guards) = write_projections(region, insert, target, &mut env)?;
    let fn_preludes = fn_distincts(region, select, source, &mut env)?;
    badrow::push_bad_row_guard(
        region,
        (&write_guards, &ctes, &joins),
        select,
        source,
        &mut expr_guards,
    );
    guards.extend(write_guards);
    if select
        .sources
        .iter()
        .any(|n| !source.columns.iter().any(|c| c.name == *n))
    {
        return Err(SkipReason::UnknownObjectDependency);
    }
    let sql = render_copy(
        select,
        source,
        (insert, target),
        projections,
        (guards, ctes, joins),
    );
    Ok(CopySql {
        sql,
        expr_guards,
        fn_preludes,
    })
}

/// INSERT 位 fn_site 的 distinct-over-sd_cur 前导查询：实参按投影
/// 环境渲染（`$v_` → sd_cur 列 / 行级表达式 / IN 形参占位符），CTE/join
/// 与主批写同口径；渲染后仍残留的 `$v_` 占位符按 site.binds 回填，
/// 运行期由物化前导从帧解析。
fn fn_distincts(
    region: &CopyLoop<'_>,
    select: &shape::Select,
    source: &Table,
    env: &mut ProjEnv<'_>,
) -> Result<Vec<entry::FnPrelude>, SkipReason> {
    region
        .insert_sites
        .iter()
        .map(|site| {
            let args = distinct_args(&site.distinct_sql)?;
            if args.len() != site.arity {
                return Err(SkipReason::UnresolvedFnSite);
            }
            let mut rendered = Vec::with_capacity(args.len());
            for arg in &args {
                rendered.push(expr::render_arg(
                    arg,
                    region,
                    env.select,
                    env.source,
                    &mut |v| expression_var(region, v, env),
                )?);
            }
            let with = std::iter::once(sd_cur_cte(select, source))
                .chain(env.ctes.iter().cloned())
                .collect::<Vec<_>>()
                .join(", ");
            let join_sql = if env.joins.is_empty() {
                String::new()
            } else {
                format!(" {}", env.joins.join(" "))
            };
            let sql = format!(
                "WITH {with} SELECT DISTINCT {} FROM sd_cur{join_sql}",
                rendered.join(", ")
            );
            let tokens = shape::Tokens::new(&sql).ok_or(SkipReason::UnprovenWriteCompleteness)?;
            let words = tokens.words();
            let binds = site
                .binds
                .iter()
                .filter(|(ph, _)| words.iter().any(|w| w == ph))
                .cloned()
                .collect();
            Ok(entry::FnPrelude {
                table: site.table.clone(),
                name: site.name.clone(),
                distinct_sql: sql,
                binds,
                arity: site.arity,
                needs_sd_cur: true,
            })
        })
        .collect()
}

/// distinct_sql 的 SELECT 列表项：`select [distinct] item (, item)*`，
/// 顶层逗号切分，词法 token 以空格重组（analyze 已证无 FROM 子句）。
fn distinct_args(sql: &str) -> Result<Vec<String>, SkipReason> {
    let bad = SkipReason::UnresolvedFnSite;
    let t = shape::Tokens::new(sql).ok_or(bad)?;
    let w = t.words();
    if w.first().map(String::as_str) != Some("select") {
        return Err(bad);
    }
    let mut i = 1;
    if w.get(i).is_some_and(|x| x == "distinct") {
        i += 1;
    }
    let mut items = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    for word in &w[i..] {
        match word.as_str() {
            "(" => {
                depth += 1;
                cur.push(word);
            }
            ")" => {
                depth -= 1;
                cur.push(word);
            }
            "," if depth == 0 => {
                items.push(cur.join(" "));
                cur = Vec::new();
            }
            _ => cur.push(word),
        }
    }
    if !cur.is_empty() {
        items.push(cur.join(" "));
    }
    if items.is_empty() {
        return Err(bad);
    }
    Ok(items)
}

fn write_projections(
    region: &CopyLoop<'_>,
    insert: &shape::Insert,
    target: &Table,
    env: &mut ProjEnv<'_>,
) -> Result<(Vec<String>, Vec<String>), SkipReason> {
    let mut projections = Vec::new();
    let mut guards = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (name, parameter) in insert.columns.iter().zip(&insert.parameters) {
        if !seen.insert(&name.0) {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
        let to = target
            .columns
            .iter()
            .find(|c| c.name == *name)
            .ok_or(SkipReason::UnknownObjectDependency)?;
        let guarded = projection(region, parameter, to, env)?;
        projections.push(guarded.expr);
        if !guarded.guard.is_empty() {
            guards.push(guarded.guard);
        }
    }
    Ok((projections, guards))
}

fn prove_omitted_columns(insert: &shape::Insert, target: &Table) -> Result<(), SkipReason> {
    if insert.parameters.len() != insert.columns.len() {
        return Err(SkipReason::UnprovenTargetConstraints);
    }
    for column in &target.columns {
        if insert.columns.contains(&column.name) {
            continue;
        }
        match &column.default {
            Some(Default::PureConst(_)) => {}
            Some(Default::Opaque(_)) => return Err(SkipReason::NonDeterministicDefault),
            None if column.nullable => {}
            None => return Err(SkipReason::UnprovenTargetConstraints),
        }
    }
    Ok(())
}

fn projection(
    region: &CopyLoop<'_>,
    parameter: &str,
    to: &Column,
    env: &mut ProjEnv<'_>,
) -> Result<expr::GuardedExpr, SkipReason> {
    if let Some(field) = single_field(region, parameter) {
        if !region.allow_filter {
            return Ok(expr::GuardedExpr {
                expr: field_projection(env.select, env.source, to, &field)?,
                guard: String::new(),
            });
        }
    } else if let Some((_, BindSrc::Var(v))) = region.binds.iter().find(|(p, _)| p == parameter) {
        if let Some(w) = region.arith_writes.iter().find(|w| w.var == *v) {
            return arith::projection(region, w, to, env);
        }
        if let Some(lq) = env.lookups.iter().find(|l| &l.var == v) {
            return lookup_projection(region, lq, to, env);
        }
        if let Some((_, ie)) = region.inlined.iter().find(|(iv, _)| iv == v) {
            return inline_projection(region, to, (v, ie), env);
        }
        if let Some(g) = tuple_projection(to, v, env)? {
            return Ok(g);
        }
        // §4.2：循环不变 IN 形参按占位符投影，值域守卫与写汇同口径。
        if region.in_params.contains(&v.0.to_ascii_lowercase()) {
            return Ok(expr::at_write_sink(parameter.to_string(), to));
        }
        return sink_projection(region, env.select, env.source, to, v);
    }
    if !region.allow_filter {
        return Err(SkipReason::UnsupportedWrite);
    }
    if let Some(field) = single_field(region, parameter) {
        if let Some(g) = agg_proj::agg_projection(region, to, &field, env)? {
            return Ok(g);
        }
    }
    expr::prove(parameter, region, (env.select, env.source), to, &mut |v| {
        expression_var(region, v, env)
    })
}

fn expression_var(
    region: &CopyLoop<'_>,
    var: &super::VarName,
    env: &mut ProjEnv<'_>,
) -> Result<expr::BoundExpr, SkipReason> {
    let bad = SkipReason::UnprovenWriteCompleteness;
    if region.var_types.get(&var.0) == Some(&crate::types::SpinType::Varchar) {
        return Err(bad);
    }
    if let Some((_, value)) = region.inlined.iter().find(|(v, _)| v == var) {
        return inline::numeric_projection(region, (var, value), env);
    }
    let lq = env.lookups.iter().find(|l| &l.var == var).ok_or(bad)?;
    if !lq.filter_on_miss
        || lq.case_sink.is_some()
        || !expr::Domain::of(&lq.kind).is_numeric()
        || region.lookups.iter().any(|q| {
            &q.var == var
                && q.nvl_default
                    .as_ref()
                    .is_some_and(|n| n.parse::<i64>().is_err())
        })
    {
        return Err(bad);
    }
    let members: Vec<_> = env.lookups.iter().filter(|l| l.group == lq.group).collect();
    let hits = group_hits(&members, env.select, env.source)?;
    for member in &members {
        register_lookup(member, env.lookups, env.ctes, env.joins);
    }
    Ok(expr::BoundExpr {
        value: expr::GuardedExpr {
            expr: lq.render.value.clone(),
            guard: hits,
        },
        domain: expr::Domain::Numeric,
    })
}

fn lookup_projection(
    region: &CopyLoop<'_>,
    lq: &lookup::ResolvedLookup,
    to: &Column,
    env: &mut ProjEnv<'_>,
) -> Result<expr::GuardedExpr, SkipReason> {
    if lq.case_sink.is_none()
        && !(lq.filter_on_miss && region.allow_filter)
        && (lq.kind != to.kind || (lq.nullable && !to.nullable))
    {
        return Err(SkipReason::UnprovenTargetConstraints);
    }
    let members: Vec<_> = env.lookups.iter().filter(|l| l.group == lq.group).collect();
    let hits = group_hits(&members, env.select, env.source)?;
    for member in &members {
        register_lookup(member, env.lookups, env.ctes, env.joins);
    }
    if let Some(cs) = &lq.case_sink {
        return case::sink(region, lq, cs, hits, to, env);
    }
    if lq.filter_on_miss {
        if !region.allow_filter {
            return Err(SkipReason::UnsupportedWrite);
        }
        let mut value = expr::at_write_sink(lq.render.value.clone(), to);
        value.guard = format!("({hits}) AND ({})", value.guard);
        return Ok(value);
    }
    Ok(expr::GuardedExpr {
        expr: format!(
            "CASE WHEN {hits} THEN {} ELSE {} END",
            lq.render.value, lq.render.default_sql
        ),
        guard: String::new(),
    })
}

fn guarded_choice(hits: &str, value: String, default: &str, to: &Column) -> expr::GuardedExpr {
    let selected = expr::at_write_sink(value, to);
    let fallback = expr::at_write_sink(default.to_string(), to);
    let ty = &to.kind;
    expr::GuardedExpr {
        expr: format!(
            "CASE WHEN {hits} THEN TRY_CAST(({}) AS {ty}) ELSE TRY_CAST(({}) AS {ty}) END",
            selected.expr, fallback.expr
        ),
        guard: format!(
            "CASE WHEN {hits} THEN ({}) ELSE ({}) END",
            selected.guard, fallback.guard
        ),
    }
}

fn render_copy(
    select: &shape::Select,
    source: &Table,
    sink: (&shape::Insert, &Table),
    projections: Vec<String>,
    parts: (Vec<String>, Vec<String>, Vec<String>),
) -> String {
    let (insert, target) = sink;
    let (guards, ctes, joins) = parts;
    let filter = if guards.is_empty() {
        String::new()
    } else {
        format!(" WHERE ({})", guards.join(") AND ("))
    };
    let sd_cur = sd_cur_cte(select, source);
    let with = std::iter::once(sd_cur)
        .chain(ctes)
        .collect::<Vec<_>>()
        .join(", ");
    let join_sql = if joins.is_empty() {
        String::new()
    } else {
        format!(" {}", joins.join(" "))
    };
    format!(
        "WITH {with} INSERT INTO {} ({}) SELECT {} FROM sd_cur{join_sql}{filter}",
        target.qualified,
        render(&insert.columns),
        projections.join(", ")
    )
}

/// 守卫省略形参 WHERE，以全源行超集保守预筛。
pub(super) fn sd_cur_scan(select: &shape::Select, source: &Table) -> String {
    let proj = select
        .columns
        .iter()
        .zip(&select.sources)
        .map(|(out, src)| {
            if out == src {
                src.sql()
            } else {
                format!("{} AS {}", src.sql(), out.sql())
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let from_clause = match &select.alias {
        Some(a) => format!("{} {}", source.qualified, a.sql()),
        None => source.qualified.clone(),
    };
    format!("SELECT {proj} FROM {from_clause}")
}

/// 守卫内嵌的游标行集：verbatim 游标即原文（含 WHERE/GROUP BY——守卫
/// 针对解释器实际 FETCH 的行集）；普通游标为省略 WHERE 的全源扫描。
pub(super) fn cur_scan(select: &shape::Select, cur: &Table) -> String {
    match &select.verbatim {
        Some(v) => v.clone(),
        None => sd_cur_scan(select, cur),
    }
}

fn register_lookup(
    l: &lookup::ResolvedLookup,
    lookups: &[lookup::ResolvedLookup],
    ctes: &mut Vec<String>,
    joins: &mut Vec<String>,
) {
    let mut seen = Vec::new();
    register_rec(l, lookups, ctes, joins, &mut seen);
}

fn register_rec(
    l: &lookup::ResolvedLookup,
    lookups: &[lookup::ResolvedLookup],
    ctes: &mut Vec<String>,
    joins: &mut Vec<String>,
    seen: &mut Vec<super::VarName>,
) {
    if seen.contains(&l.var) {
        return;
    }
    seen.push(l.var.clone());
    for dep in &l.deps {
        if let Some(d) = lookups.iter().find(|x| &x.var == dep) {
            register_rec(d, lookups, ctes, joins, seen);
        }
    }
    if let Some(cte) = &l.render.cte {
        if !ctes.contains(cte) {
            ctes.push(cte.clone());
        }
    }
    if let Some(join) = &l.render.join {
        if !joins.contains(join) {
            joins.push(join.clone());
        }
    }
}

fn group_hits(
    members: &[&lookup::ResolvedLookup],
    select: &shape::Select,
    source: &Table,
) -> Result<String, SkipReason> {
    let mut hits = Vec::new();
    for m in members {
        for (key_kind, cursor_field) in &m.render.key_checks {
            let from = source_column(select, source, cursor_field)?;
            if &from.kind != key_kind {
                return Err(SkipReason::UnprovenTargetConstraints);
            }
        }
        hits.push(m.render.hit.clone());
    }
    Ok(hits.join(" AND "))
}

fn field_projection(
    select: &shape::Select,
    source: &Table,
    to: &Column,
    field: &Name,
) -> Result<String, SkipReason> {
    let from = source_column(select, source, field)?;
    if from.kind != to.kind || (from.nullable && !to.nullable) {
        return Err(SkipReason::UnprovenTargetConstraints);
    }
    Ok(format!("sd_cur.{}", field.sql()))
}

pub(super) fn source_column<'a>(
    select: &shape::Select,
    source: &'a Table,
    field: &Name,
) -> Result<&'a Column, SkipReason> {
    if select.columns.iter().filter(|c| *c == field).count() != 1 {
        return Err(SkipReason::UnprovenExpression);
    }
    let src = select
        .source_of(field)
        .ok_or(SkipReason::UnknownObjectDependency)?;
    source
        .columns
        .iter()
        .find(|c| c.name == *src)
        .ok_or(SkipReason::UnknownObjectDependency)
}

/// LookupTuple 槽位投影（§3.2）：`CASE WHEN sd_cnt = 1 THEN <列> ELSE <兜底>`；
/// 共享派生表/连接按别名去重登记，相关键类型对齐与查找同口径。
fn tuple_projection(
    to: &Column,
    v: &super::VarName,
    env: &mut ProjEnv<'_>,
) -> Result<Option<expr::GuardedExpr>, SkipReason> {
    let Some((t, slot)) = env
        .tuples
        .iter()
        .find_map(|t| t.slots.iter().find(|s| s.var == *v).map(|s| (t, s)))
    else {
        return Ok(None);
    };
    for (key_kind, cursor_field) in &t.key_checks {
        let from = source_column(env.select, env.source, cursor_field)?;
        if &from.kind != key_kind {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
    }
    if let Some(kind) = &slot.kind {
        if *kind != to.kind || (slot.nullable && !to.nullable) {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
    } else if slot.nullable && !to.nullable {
        return Err(SkipReason::UnprovenTargetConstraints);
    }
    if !env.ctes.contains(&t.cte) {
        env.ctes.push(t.cte.clone());
    }
    if !env.joins.contains(&t.join) {
        env.joins.push(t.join.clone());
    }
    Ok(Some(expr::GuardedExpr {
        expr: format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            t.hit, slot.value, slot.default_sql
        ),
        guard: String::new(),
    }))
}

fn sink_projection(
    region: &CopyLoop<'_>,
    select: &shape::Select,
    source: &Table,
    to: &Column,
    v: &super::VarName,
) -> Result<expr::GuardedExpr, SkipReason> {
    if !region.allow_filter {
        return Err(SkipReason::UnsupportedWrite);
    }
    let Some((_, op, lhs, rhs)) = region.sinks.iter().find(|(sv, _, _, _)| sv == v) else {
        return Err(SkipReason::UnprovenExpression);
    };
    for f in [lhs, rhs] {
        let name = Name(f.clone());
        let src = select
            .source_of(&name)
            .ok_or(SkipReason::UnprovenWriteCompleteness)?;
        let from = source
            .columns
            .iter()
            .find(|c| c.name == *src)
            .ok_or(SkipReason::UnprovenWriteCompleteness)?;
        let kind_ok = match op {
            SinkOp::Div => expr::integer_kind(&from.kind),
            SinkOp::Concat => expr::concat_kind(&from.kind),
        };
        if select.columns.iter().filter(|c| **c == name).count() != 1 || !kind_ok {
            return Err(SkipReason::UnprovenWriteCompleteness);
        }
    }
    Ok(match op {
        SinkOp::Div => expr::pl_division(lhs, rhs, to),
        SinkOp::Concat => expr::pl_concat(lhs, rhs, to),
    })
}

fn single_field(region: &CopyLoop<'_>, parameter: &str) -> Option<Name> {
    let (_, BindSrc::Field { rec, field }) = region.binds.iter().find(|(p, _)| p == parameter)?
    else {
        return None;
    };
    (rec == region.record).then(|| Name(field.clone()))
}

fn render(names: &[Name]) -> String {
    names.iter().map(Name::sql).collect::<Vec<_>>().join(", ")
}
