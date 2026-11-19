//! 集合化计划生成：结构证据、内联提升、快照复核与批写绑定。

use super::{catalog_summary, copy, entry, filter, lookup, region, resolve, route, schema, shape};
use super::{BindSrc, CopyLoop, EngineError, OptimizeCtx, SkipReason};

struct Inputs {
    select: shape::Select,
    insert: shape::Insert,
    source: schema::Table,
    target: schema::Table,
}

pub(super) fn generate(
    copy: &mut CopyLoop<'_>,
    ex: &schema::Source<'_>,
    ctx: &OptimizeCtx<'_>,
    region: &region::CursorRegion,
    tail_commit: bool,
) -> Result<Result<entry::ConversionPlan, SkipReason>, EngineError> {
    let inputs = match load_inputs(copy, ex)? {
        Ok(inputs) => inputs,
        Err(reason) => return Ok(Err(reason)),
    };
    let cur_scan = copy::sd_cur_scan(&inputs.select, &inputs.source);
    let mut resolved = match resolve::resolve_lookups(ex, &copy.lookups, &cur_scan)? {
        Ok(r) => r,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Err(reason) = hoist(copy, ex, &mut resolved)? {
        return Ok(Err(reason));
    }
    if let Err(reason) = resolve::substitute_lookup_refs(&mut resolved) {
        return Ok(Err(reason));
    }
    let resolved_tuples = match resolve::resolve_tuples(ex, &copy.tuples, resolved.len() + 1)? {
        Ok(t) => t,
        Err(reason) => return Ok(Err(reason)),
    };
    let resolved_aggs =
        match resolve::resolve_aggs(ex, &copy.aggs, resolved.len() + resolved_tuples.len() + 1)? {
            Ok(a) => a,
            Err(reason) => return Ok(Err(reason)),
        };
    if !matches_snapshot(
        ex,
        ctx,
        &inputs,
        &resolved,
        &resolved_tuples,
        &resolved_aggs,
    )? {
        return Ok(Err(SkipReason::UnprovenTargetConstraints));
    }
    Ok(finish(
        copy,
        inputs,
        resolved,
        resolved_tuples,
        resolved_aggs,
        region,
        tail_commit,
    ))
}

fn load_inputs(
    copy: &CopyLoop<'_>,
    ex: &schema::Source<'_>,
) -> Result<Result<Inputs, SkipReason>, EngineError> {
    let shape = shape::select(copy.query)
        .and_then(|select| shape::insert(copy.insert).map(|insert| (select, insert)));
    let (select, insert) = match shape {
        Ok(shape) => shape,
        Err(reason) => return Ok(Err(reason)),
    };
    if select.table == insert.table {
        return Ok(Err(SkipReason::TargetReadDependency));
    }
    let Some(source) = ex.load(&select.table)? else {
        return Ok(Err(SkipReason::UnknownObjectDependency));
    };
    if let Some(f) = &select.filter {
        if let Err(reason) = filter::prove(
            f,
            &source,
            &select.table,
            &select.alias,
            copy.query_binds,
            copy.var_types,
        ) {
            return Ok(Err(reason));
        }
    }
    let Some(target) = ex.load(&insert.table)? else {
        return Ok(Err(SkipReason::UnprovenTargetConstraints));
    };
    Ok(Ok(Inputs {
        select,
        insert,
        source,
        target,
    }))
}

fn hoist(
    copy: &mut CopyLoop<'_>,
    ex: &schema::Source<'_>,
    resolved: &mut resolve::HoistedLookups,
) -> Result<Result<(), SkipReason>, EngineError> {
    catalog_summary::resolve_locals(&mut copy.inlined);
    let record = copy.record;
    let mut next_index = resolved.len() + 1;
    for expr in copy
        .inlined
        .iter_mut()
        .chain(copy.inline_checks.iter_mut())
        .map(|(_, expr)| expr)
        .chain(copy.varchar_inputs.iter_mut())
    {
        match resolve::hoist_inline(ex, expr, record, next_index)? {
            Ok((new_expr, mut acc)) => {
                *expr = new_expr;
                next_index += acc.len();
                resolved.append(&mut acc);
            }
            Err(reason) => return Ok(Err(reason)),
        }
    }
    Ok(Ok(()))
}

fn matches_snapshot(
    ex: &schema::Source<'_>,
    ctx: &OptimizeCtx<'_>,
    inputs: &Inputs,
    resolved: &resolve::HoistedLookups,
    tuples: &[(lookup::ResolvedTuple, resolve::LookupTables)],
    aggs: &[(super::agg::ResolvedAgg, resolve::LookupTables)],
) -> Result<bool, EngineError> {
    if let Some(snapshot) = ctx.snapshot {
        if !ex.matches_snapshot(snapshot, &inputs.select.table, ctx.rules)?
            || !ex.matches_snapshot(snapshot, &inputs.insert.table, ctx.rules)?
        {
            return Ok(false);
        }
        for tables in resolved
            .iter()
            .map(|(_, t)| t)
            .chain(tuples.iter().map(|(_, t)| t))
            .chain(aggs.iter().map(|(_, t)| t))
        {
            for (name, _) in tables {
                if !ex.matches_snapshot(snapshot, name, ctx.rules)? {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn finish(
    copy: &CopyLoop<'_>,
    inputs: Inputs,
    resolved: resolve::HoistedLookups,
    resolved_tuples: Vec<(lookup::ResolvedTuple, resolve::LookupTables)>,
    resolved_aggs: Vec<(super::agg::ResolvedAgg, resolve::LookupTables)>,
    region: &region::CursorRegion,
    tail_commit: bool,
) -> Result<entry::ConversionPlan, SkipReason> {
    let projections: Vec<lookup::ResolvedLookup> =
        resolved.iter().map(|(l, _)| l.clone()).collect();
    let top1_guards = resolved
        .iter()
        .filter_map(|(l, _)| {
            l.render
                .tie_guard
                .clone()
                .map(|g| (g, l.render.guard_binds.clone()))
        })
        .collect();
    let route_guards = route_guards(copy, &resolved)?;
    let tuple_views: Vec<lookup::ResolvedTuple> =
        resolved_tuples.iter().map(|(t, _)| t.clone()).collect();
    let agg_views: Vec<super::agg::ResolvedAgg> =
        resolved_aggs.iter().map(|(a, _)| a.clone()).collect();
    let generated = copy::copy_sql(
        copy,
        (&inputs.select, &inputs.insert),
        (&inputs.source, &inputs.target),
        &projections,
        &tuple_views,
        &agg_views,
    )?;
    let binds = query_binds(copy, &resolved);
    let expr_guards = entry::bind_guards(generated.expr_guards, &binds)?;
    Ok(entry::ConversionPlan {
        sql: generated.sql,
        binds,
        tail_commit,
        open_pc: region.open_pc,
        close_pc: region.close_pc,
        source: inputs.select.table,
        target: inputs.insert.table,
        source_table: inputs.source,
        target_table: inputs.target,
        lookup_tables: resolved
            .into_iter()
            .flat_map(|(_, ts)| ts)
            .chain(resolved_tuples.into_iter().flat_map(|(_, ts)| ts))
            .chain(resolved_aggs.into_iter().flat_map(|(_, ts)| ts))
            .collect(),
        top1_guards,
        route_guards,
        expr_guards,
    })
}

fn route_guards(
    copy: &CopyLoop<'_>,
    resolved: &resolve::HoistedLookups,
) -> Result<Vec<String>, SkipReason> {
    let mut guards = Vec::new();
    if let Some((v, lit)) = &copy.route {
        let Some((l, _)) = resolved.iter().find(|(l, _)| &l.var == v) else {
            return Err(SkipReason::UnsupportedControlFlow);
        };
        guards.push(route::guard(l, lit)?);
    }
    Ok(guards)
}

fn query_binds(copy: &CopyLoop<'_>, resolved: &resolve::HoistedLookups) -> Vec<(String, BindSrc)> {
    let mut binds = query_binds_inner(copy, resolved);
    for (ph, src) in copy.aggs.iter().flat_map(|a| &a.static_binds) {
        if !binds.iter().any(|(p, _)| p == ph) {
            binds.push((ph.clone(), src.clone()));
        }
    }
    binds
}

fn query_binds_inner(
    copy: &CopyLoop<'_>,
    resolved: &resolve::HoistedLookups,
) -> Vec<(String, BindSrc)> {
    let mut binds = copy.query_binds.to_vec();
    for binds_src in copy
        .lookups
        .iter()
        .map(|l| &l.filter_binds)
        .chain(copy.tuples.iter().map(|t| &t.filter_binds))
    {
        for (ph, src) in binds_src {
            // 查找结果已逐行代入投影，不能再从尚未 FETCH 的帧取值。
            if let BindSrc::Var(v) = src {
                if resolved.iter().any(|(l, _)| l.var == *v) {
                    continue;
                }
            }
            if !binds.iter().any(|(p, _)| p == ph) {
                binds.push((ph.clone(), src.clone()));
            }
        }
    }
    binds
}
