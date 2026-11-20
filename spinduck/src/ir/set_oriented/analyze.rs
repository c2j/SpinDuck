//! 区域体分析与读写绑定验证，产出 CopyLoop。

mod body;

use super::{agg, catalog_summary, dead, lookup, region, route, shape, vguard};
use super::{ArithWrite, CopyLoop, SetOrientedMode, SinkOp, SkipReason};
use crate::ir::{AssignTarget, BinOp, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, VarName};

struct Context<'a, 'c> {
    prog: &'a IrProg,
    region: &'a region::CursorRegion,
    mode: SetOrientedMode,
    catalog: &'c crate::catalog::RoutineCatalog,
    /// IN 形参名集（小写）：Lookup 内部绑定须为循环不变实参的证明依据。
    in_params: std::collections::BTreeSet<String>,
}

/// 准备阶段产物：INSERT 语句文本与游标/INSERT 绑定（均为 'a 程序期引用）。
struct Prepared<'a> {
    insert: &'a str,
    binds: &'a [(String, BindSrc)],
    query: &'a str,
    query_binds: &'a [(String, BindSrc)],
    fn_sites: &'a [crate::sql::FnSite],
}

pub(super) fn analyze_region<'a>(
    prog: &'a IrProg,
    region: &'a region::CursorRegion,
    mode: SetOrientedMode,
    catalog: &crate::catalog::RoutineCatalog,
    in_params: &std::collections::BTreeSet<String>,
) -> Result<(CopyLoop<'a>, bool, bool), SkipReason> {
    let ctx = Context {
        prog,
        region,
        mode,
        catalog,
        in_params: in_params.clone(),
    };
    let body = body::Body::walk(&ctx)?;
    let parts = prepare(&ctx, &body, in_params)?;
    let flags = (body.tail_commit, body.has_trap);
    let copy = assemble(&ctx, body, parts, in_params)?;
    Ok((copy, flags.0, flags.1))
}

/// INSERT 形态、游标查询与写完备性检查；产出批写所需的 SQL 片段引用。
fn prepare<'a>(
    ctx: &Context<'a, '_>,
    body: &body::Body<'a>,
    in_params: &std::collections::BTreeSet<String>,
) -> Result<Prepared<'a>, SkipReason> {
    let Some(IrStmt::SqlExec {
        sql: insert,
        binds,
        fn_sites,
        ..
    }) = body.insert_stmt
    else {
        return Err(SkipReason::UnsupportedWrite);
    };
    let (query, query_binds, cursor_sites) = cursor_query(ctx)?;
    if !fn_sites.is_empty() {
        return Err(SkipReason::UnresolvedFnSite);
    }
    if ctx.prog.var_types.contains_key(&ctx.region.record.0) {
        return Err(SkipReason::UnprovenExpression);
    }
    body.validate(ctx, binds)?;
    prove_param_binds(ctx.prog, ctx.region, binds, in_params)?;
    Ok(Prepared {
        insert,
        binds,
        query,
        query_binds,
        fn_sites: cursor_sites,
    })
}

/// Body 分析结果装配为 CopyLoop（写集与 IN 形参名集在此定稿）。
fn assemble<'a>(
    ctx: &Context<'a, '_>,
    body: body::Body<'a>,
    parts: Prepared<'a>,
    in_params: &std::collections::BTreeSet<String>,
) -> Result<CopyLoop<'a>, SkipReason> {
    Ok(CopyLoop {
        query: parts.query,
        insert: parts.insert,
        binds: parts.binds,
        query_binds: parts.query_binds,
        fn_sites: parts.fn_sites,
        var_types: &ctx.prog.var_types,
        record: &ctx.region.record,
        allow_filter: ctx.mode == SetOrientedMode::BatchAtomic && body.has_trap,
        insert_log: body.insert_log,
        sinks: body.sinks,
        lookups: body.lookups,
        tuples: body.tuples,
        aggs: body.aggs,
        field_writes: body.field_writes,
        route: body.route,
        folded: body.folded,
        inlined: body.inlined,
        inline_checks: body.inline_checks,
        varchar_inputs: body.varchar_inputs,
        inline_preconditions: body.inline_preconditions,
        dead: body.dead,
        arith_writes: body.arith_writes,
        writes: region::write_set(ctx.prog, ctx.region, ctx.catalog)?,
        in_params: in_params.clone(),
    })
}

/// §4.2 IN 形参投影证明：INSERT 绑定引用 IN 形参时，该变量必须在区域体内
/// 无任何赋值（Assign/Fetch/SqlQuery INTO），否则逐行取值与形参单值不等价。
fn prove_param_binds(
    prog: &IrProg,
    region: &region::CursorRegion,
    binds: &[(String, BindSrc)],
    in_params: &std::collections::BTreeSet<String>,
) -> Result<(), SkipReason> {
    for (_, src) in binds {
        let BindSrc::Var(v) = src else {
            continue;
        };
        if in_params.contains(&v.0.to_ascii_lowercase())
            && !region::var_invariant_in_region(prog, region, v)
        {
            return Err(SkipReason::UnprovenExpression);
        }
    }
    Ok(())
}

type CursorQuery<'a> = (&'a str, &'a [(String, BindSrc)], &'a [crate::sql::FnSite]);

fn cursor_query<'a>(ctx: &Context<'a, '_>) -> Result<CursorQuery<'a>, SkipReason> {
    let IrStmt::OpenCursor {
        source:
            CursorSource::Static {
                sql,
                binds,
                fn_sites,
            },
        ..
    } = &ctx.prog.code[ctx.region.open_pc]
    else {
        return Err(SkipReason::UnsupportedControlFlow);
    };
    // fn_site 准入：缓存子查询必须整体位于顶层 WHERE 之后（游标过滤
    // 形态，byfund c_get_data 同型）。SELECT 列表计算列形态超出
    // shape::select 承载面，保守拒绝（守护测试口径不变）；distinct_sql
    // 引用其他 sd_fn 缓存表（嵌套下沉）同样拒绝。
    if !fn_sites.is_empty() && (!sites_in_where(sql, fn_sites) || sites_nested(fn_sites)) {
        return Err(SkipReason::UnresolvedFnSite);
    }
    Ok((sql, binds, fn_sites))
}

/// distinct_sql 内出现任何 `sd_fn*` 标识符即视为嵌套物化依赖，拒绝。
fn sites_nested(sites: &[crate::sql::FnSite]) -> bool {
    sites.iter().any(|s| {
        super::shape::Tokens::new(&s.distinct_sql)
            .is_some_and(|t| t.words().iter().any(|w| w.starts_with("sd_fn")))
    })
}

/// 每个 site 缓存表名的全部出现位置都在顶层 `where` 之后，且确实被引用。
fn sites_in_where(sql: &str, sites: &[crate::sql::FnSite]) -> bool {
    let Some(t) = super::shape::Tokens::new(sql) else {
        return false;
    };
    let words = t.words();
    let mut depth = 0i32;
    let mut where_pos = None;
    for (i, w) in words.iter().enumerate() {
        match w.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "where" if depth == 0 => {
                where_pos = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(wp) = where_pos else {
        return false;
    };
    sites.iter().all(|s| {
        let mut seen = false;
        for (i, w) in words.iter().enumerate() {
            if w == &s.table {
                if i <= wp {
                    return false;
                }
                seen = true;
            }
        }
        seen
    })
}

fn invariant_reads_disjoint(
    prog: &IrProg,
    region: &region::CursorRegion,
    sql: &str,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<(), SkipReason> {
    let writes = region::write_set(prog, region, catalog)?;
    if let Some(table) = region::select_from_target(sql) {
        if writes.contains(&table) {
            return Err(SkipReason::TargetReadDependency);
        }
    }
    Ok(())
}

fn freeze_inputs(inputs: Vec<IrExpr>, inlined: &[(VarName, IrExpr)]) -> Vec<IrExpr> {
    let env: Vec<_> = inlined
        .iter()
        .map(|(var, expr)| (var.0.clone(), expr.clone()))
        .collect();
    inputs
        .into_iter()
        .map(|expr| catalog_summary::subst_env(expr, &env))
        .collect()
}

fn cond_reassign(
    prog: &IrProg,
    pc: usize,
    inlined: &[(VarName, IrExpr)],
    catalog: &crate::catalog::RoutineCatalog,
) -> Option<(VarName, IrExpr, catalog_summary::CallChecks)> {
    let IrStmt::JumpIfNot { cond, to } = prog.code.get(pc)? else {
        return None;
    };
    if *to != pc + 3 {
        return None;
    }
    let IrStmt::Assign {
        target: AssignTarget::Var(v),
        expr,
    } = prog.code.get(pc + 1)?
    else {
        return None;
    };
    if !matches!(prog.code.get(pc + 2), Some(IrStmt::Jump(j)) if j == to) {
        return None;
    }
    let (expr, inputs) = match expr {
        IrExpr::SqlFn { .. } => (expr.clone(), catalog_summary::CallChecks::default()),
        IrExpr::FuncCall { name, args, .. } => {
            let call = catalog_summary::inline_pure_call(catalog, name, args)?;
            (call.expr, call.checks)
        }
        _ => return None,
    };
    let base = inlined
        .iter()
        .find(|(iv, _)| iv == v)
        .map(|(_, e)| e.clone())?;
    Some((
        v.clone(),
        IrExpr::Case {
            operand: None,
            whens: vec![(
                catalog_summary::subst_var(cond.clone(), &v.0, &base),
                catalog_summary::subst_var(expr, &v.0, &base),
            )],
            else_expr: Some(Box::new(base)),
        },
        inputs,
    ))
}
