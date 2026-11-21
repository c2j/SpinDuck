//! 集合化计划生成：结构证据、内联提升、快照复核与批写绑定。

use super::{catalog_summary, copy, entry, filter, lookup, region, resolve, route, schema, shape};
use super::{BindSrc, CopyLoop, EngineError, OptimizeCtx, SkipReason, VarName};

struct Inputs {
    select: shape::Select,
    insert: shape::Insert,
    /// 基表结构证据：WHERE 全定义证明、入口漂移复核对象。
    source: schema::Table,
    /// 投影证据表：普通游标 = 基表；verbatim 游标 = DESCRIBE 探测的
    /// sd_cur 输出 schema（输出名即列名，kind 为内核实测类型）。
    cur: schema::Table,
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
    let cur_scan = copy::cur_scan(&inputs.select, &inputs.cur);
    let mut resolved = match resolve::resolve_lookups(ex, ctx.catalog, &copy.lookups, &cur_scan)? {
        Ok(r) => r,
        Err(reason) => {
            return Ok(Err(reason));
        }
    };
    if let Err(reason) = hoist(copy, ex, ctx.catalog, region, &cur_scan, &mut resolved)? {
        return Ok(Err(reason));
    }
    if let Err(reason) = resolve::substitute_lookup_refs(&mut resolved) {
        return Ok(Err(reason));
    }
    // §3.5：视图底表 Reads∩Writes(Region) 必须为空（视图名本身已在
    // carryover_group_at 复核；此处补最终底表）。
    for (l, _) in resolved.iter() {
        if let Some(base) = &l.guard_base {
            if copy.writes.contains(&base.0) {
                return Ok(Err(SkipReason::TargetReadDependency));
            }
        }
        if l.view_bases.iter().any(|b| copy.writes.contains(&b.0)) {
            return Ok(Err(SkipReason::TargetReadDependency));
        }
    }
    let resolved_tuples = match resolve::resolve_tuples(ex, &copy.tuples, resolved.len() + 1)? {
        Ok(t) => t,
        Err(reason) => {
            return Ok(Err(reason));
        }
    };
    let resolved_aggs =
        match resolve::resolve_aggs(ex, &copy.aggs, resolved.len() + resolved_tuples.len() + 1)? {
            Ok(a) => a,
            Err(reason) => {
                return Ok(Err(reason));
            }
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
    match finish(
        copy,
        inputs,
        resolved,
        resolved_tuples,
        resolved_aggs,
        region,
        tail_commit,
    ) {
        Ok(p) => Ok(Ok(p)),
        Err(reason) => Ok(Err(reason)),
    }
}

fn load_inputs(
    copy: &CopyLoop<'_>,
    ex: &schema::Source<'_>,
) -> Result<Result<Inputs, SkipReason>, EngineError> {
    // 普通游标优先；SELECT 列表含表达式/聚合或 WHERE 残留尾部子句时
    // 走 verbatim 原文内嵌（输出列 schema 另经 DESCRIBE 探测）。
    let select = match shape::select(copy.query) {
        Ok(s) if !s.filter.as_deref().is_some_and(shape::filter_has_tail) => s,
        _ => match shape::select_verbatim(copy.query) {
            Ok(s) => s,
            Err(reason) => return Ok(Err(reason)),
        },
    };
    let shape = shape::insert(copy.insert).map(|insert| (select, insert));
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
        let fn_tables: Vec<String> = copy.fn_sites.iter().map(|s| s.table.clone()).collect();
        if let Err(reason) = filter::prove(
            f,
            &source,
            &select.table,
            &select.alias,
            copy.query_binds,
            copy.var_types,
            &fn_tables,
        ) {
            return Ok(Err(reason));
        }
    }
    // verbatim 游标：DESCRIBE 探测输出列 schema，列名须与解析输出名
    // 逐一对齐（顺序即 SELECT 项序）；任一不符按不可证回退。
    let cur = match &select.verbatim {
        Some(v) => {
            let columns = ex.probe_columns(v)?.filter(|cols| {
                cols.len() == select.columns.len()
                    && cols.iter().map(|c| &c.name).eq(select.columns.iter())
            });
            match columns {
                Some(columns) => schema::Table {
                    qualified: "sd_cur".to_string(),
                    columns,
                },
                None => return Ok(Err(SkipReason::UnprovenExpression)),
            }
        }
        None => source.clone(),
    };
    let Some(target) = ex.load(&insert.table)? else {
        return Ok(Err(SkipReason::UnprovenTargetConstraints));
    };
    Ok(Ok(Inputs {
        select,
        insert,
        source,
        cur,
        target,
    }))
}

fn hoist(
    copy: &mut CopyLoop<'_>,
    ex: &schema::Source<'_>,
    catalog: &crate::catalog::RoutineCatalog,
    region: &region::CursorRegion,
    cur_scan: &str,
    resolved: &mut resolve::HoistedLookups,
) -> Result<Result<(), SkipReason>, EngineError> {
    catalog_summary::resolve_locals(&mut copy.inlined);
    // Var 实参安全集：查找结果变量由 substitute_lookup_refs 逐行代入
    // CASE；其余变量须区域内无赋值（帧单值绑定，§4.4 B1 同口径）。
    let mut safe_vars: std::collections::BTreeSet<VarName> =
        resolved.iter().map(|(l, _)| l.var.clone()).collect();
    for name in copy.prog.var_types.keys() {
        let v = VarName(name.clone());
        if region::var_invariant_in_region(copy.prog, region, &v) {
            safe_vars.insert(v);
        }
    }
    let lookup_vars: std::collections::BTreeSet<VarName> =
        resolved.iter().map(|(l, _)| l.var.clone()).collect();
    let hctx = resolve::HoistCtx {
        ex,
        catalog,
        record: copy.record,
        safe_vars: &safe_vars,
        lookup_vars: &lookup_vars,
        cur_scan,
    };
    let mut next_index = resolved.len() + 1;
    for expr in copy
        .inlined
        .iter_mut()
        .chain(copy.inline_checks.iter_mut())
        .map(|(_, expr)| expr)
        .chain(copy.varchar_inputs.iter_mut())
        .chain(
            copy.arith_writes
                .iter_mut()
                .flat_map(|w| [&mut w.cond, &mut w.expr]),
        )
    {
        match resolve::hoist_inline(&hctx, expr, next_index)? {
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
    let top1_guards: Vec<entry::BoundGuard> = resolved
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
        (&inputs.cur, &inputs.target),
        &projections,
        &tuple_views,
        &agg_views,
    )?;
    let binds = with_param_binds(copy, &generated, query_binds(copy, &resolved));
    // verbatim 游标的 cur_scan 原文含 $v_ 绑定：守卫 SQL 按 token 出现
    // 情况从全量绑定集补齐（guard_binds 只覆盖查找自身过滤绑定）。
    let top1_guards: Vec<entry::BoundGuard> = top1_guards
        .into_iter()
        .map(|(sql, mut gbinds)| {
            if let Some(t) = shape::Tokens::new(&sql) {
                for (ph, src) in &binds {
                    if t.words().iter().any(|w| w == ph) && !gbinds.iter().any(|(p, _)| p == ph) {
                        gbinds.push((ph.clone(), src.clone()));
                    }
                }
            }
            (sql, gbinds)
        })
        .collect();
    let expr_guards = entry::bind_guards(generated.expr_guards, &binds)?;
    // fn_site 物化前导：游标 WHERE site 的 distinct_sql 为编译期原文
    // （含源表与过滤），INSERT 位 site 为 copy_sql 生成的
    // distinct-over-sd_cur 查询（needs_sd_cur，依赖游标表先行物化）。
    let mut fn_preludes: Vec<entry::FnPrelude> = copy
        .fn_sites
        .iter()
        .map(|s| entry::FnPrelude {
            table: s.table.clone(),
            name: s.name.clone(),
            distinct_sql: s.distinct_sql.clone(),
            binds: s.binds.clone(),
            arity: s.args.len(),
            needs_sd_cur: false,
        })
        .collect();
    fn_preludes.extend(generated.fn_preludes);
    // 视图物化前导：普通查找落在视图上时由 viewdep 产出（每查找一张
    // sd_vwN 临时表）；先于入口守卫执行（守卫可内嵌 sd_vwN 派生表）。
    let view_preludes: Vec<entry::ViewPrelude> = projections
        .iter()
        .filter_map(|l| l.view_prelude.clone())
        .collect();
    // 守卫 SQL（verbatim cur_scan / 写守卫缓存子查询）引用 sd_fn*/sd_bf*
    // 临时表时，物化前导必须先于入口守卫执行，否则守卫查询打到不存在的表。
    // 视图前导存在时 fn 前导同样提前：fn 例程调用可回滚会话本地建表，
    // 必须先于视图物化完成，否则 sd_vwN 会被其回滚撤销。
    let preludes_first = {
        let tables: Vec<&str> = fn_preludes.iter().map(|p| p.table.as_str()).collect();
        top1_guards
            .iter()
            .map(|(s, _)| s.as_str())
            .chain(route_guards.iter().map(String::as_str))
            .chain(expr_guards.iter().map(|(s, _)| s.as_str()))
            .any(|sql| {
                shape::Tokens::new(sql)
                    .is_some_and(|t| t.words().iter().any(|w| tables.contains(&w.as_str())))
            })
    } || !view_preludes.is_empty();
    Ok(entry::ConversionPlan {
        sql: generated.sql,
        binds,
        fn_preludes,
        view_preludes,
        preludes_first,
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

/// §4.2：把批写 SQL/守卫实际引用的 IN 形参占位符并入绑定集（去重）。
fn with_param_binds(
    copy: &CopyLoop<'_>,
    generated: &copy::CopySql,
    mut binds: Vec<(String, BindSrc)>,
) -> Vec<(String, BindSrc)> {
    for (ph, src) in param_binds(copy, generated) {
        if !binds.iter().any(|(p, _)| p == &ph) {
            binds.push((ph, src));
        }
    }
    binds
}

/// §4.2：批写 SQL 与行级守卫中实际引用的 IN 形参占位符，运行期按名绑定；
/// 未被引用的形参不携带（避免对未用形参做帧解析）。
fn param_binds(copy: &CopyLoop<'_>, generated: &copy::CopySql) -> Vec<(String, BindSrc)> {
    copy.binds
        .iter()
        .filter(|(ph, src)| {
            matches!(src, BindSrc::Var(v) if copy.in_params.contains(&v.0.to_ascii_lowercase()))
                && (generated.sql.contains(ph.as_str())
                    || generated
                        .expr_guards
                        .iter()
                        .any(|g| g.contains(ph.as_str())))
        })
        .cloned()
        .collect()
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
        // 内联提升查找（__sd_lkN）的静态过滤绑定同样须随批写 SQL 解析；
        // 其占位符文本在派生表 WHERE 中，与过程级查找同口径去重。
        .chain(resolved.iter().map(|(l, _)| &l.filter_binds))
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
