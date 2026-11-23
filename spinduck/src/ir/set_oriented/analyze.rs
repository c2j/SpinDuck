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
    insert_sites: Vec<super::InsertSite>,
}

/// route 决策点（D7 双臂走查的 divert 输入；§12.3(b)）。
#[derive(Clone, Copy)]
pub(super) enum RouteSite {
    /// branch() 形态：`JumpIfNot` pc 与 ELSE 入口 `to`。
    Branch { pc: usize, to: usize },
    /// trap() `fallback_if_group` 形态：PushTrap pc、ELSE 入口
    /// `else_start`（SqlQuery 组首语句）、汇合后 `resume_pc`。
    TrapGroup {
        push_pc: usize,
        else_start: usize,
        resume_pc: usize,
    },
}

/// 一次区域走查的产物：CopyLoop + 尾部标志 +（D7）route 决策点。
pub(super) struct RegionAnalysis<'a> {
    pub copy: CopyLoop<'a>,
    pub tail_commit: bool,
    pub has_trap: bool,
    /// route 决策点；D7 双臂走查的 divert 输入。
    pub route_site: Option<RouteSite>,
}

pub(super) fn analyze_region<'a>(
    prog: &'a IrProg,
    region: &'a region::CursorRegion,
    mode: SetOrientedMode,
    catalog: &crate::catalog::RoutineCatalog,
    in_params: &std::collections::BTreeSet<String>,
) -> Result<RegionAnalysis<'a>, SkipReason> {
    analyze_walk(prog, region, mode, catalog, in_params, None)
}

/// D7 双臂 THEN 臂走查（§12.3(b)）：同一区域、同一入口状态快照下
/// 独立产出臂计划；失败由调用方决定整体拒绝（单臂失败 = 整体失败）。
pub(super) fn analyze_then_arm<'a>(
    prog: &'a IrProg,
    region: &'a region::CursorRegion,
    mode: SetOrientedMode,
    catalog: &crate::catalog::RoutineCatalog,
    in_params: &std::collections::BTreeSet<String>,
    route_site: RouteSite,
) -> Result<RegionAnalysis<'a>, SkipReason> {
    analyze_walk(prog, region, mode, catalog, in_params, Some(route_site))
}

fn analyze_walk<'a>(
    prog: &'a IrProg,
    region: &'a region::CursorRegion,
    mode: SetOrientedMode,
    catalog: &crate::catalog::RoutineCatalog,
    in_params: &std::collections::BTreeSet<String>,
    route_site: Option<RouteSite>,
) -> Result<RegionAnalysis<'a>, SkipReason> {
    let ctx = Context {
        prog,
        region,
        mode,
        catalog,
        in_params: in_params.clone(),
    };
    let body = match route_site {
        Some(site) => body::Body::walk_then(&ctx, site)?,
        None => body::Body::walk(&ctx)?,
    };
    let route_site = body.route_site;
    let parts = prepare(&ctx, &body, in_params)?;
    let flags = (body.tail_commit, body.has_trap);
    let copy = assemble(&ctx, body, parts, in_params)?;
    Ok(RegionAnalysis {
        copy,
        tail_commit: flags.0,
        has_trap: flags.1,
        route_site,
    })
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
    let insert_sites = admit_insert_sites(ctx, fn_sites)?;
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
        insert_sites,
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
        insert_sites: parts.insert_sites,
        var_types: &ctx.prog.var_types,
        record: &ctx.region.record,
        prog: ctx.prog,
        allow_filter: ctx.mode == SetOrientedMode::BatchAtomic && body.has_trap,
        insert_log: body.insert_log,
        sinks: body.sinks,
        lookups: body.lookups,
        tuples: body.tuples,
        aggs: body.aggs,
        field_writes: body.field_writes,
        route: body.route,
        dual_arm: false,
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

/// INSERT 位 fn_site 准入：例程须 PureExpr（确定性无副作用——批写按
/// DISTINCT 元组去重调用，与解释器逐行调用结果一致）；distinct_sql 须为
/// 无 FROM 实参数列表（批写改写为 distinct-over-sd_cur）；无嵌套 sd_fn
/// 依赖。缓存表名改 `sd_bfN`，避免与游标 site 的 `sd_fnN` 冲突。
fn admit_insert_sites(
    ctx: &Context<'_, '_>,
    sites: &[crate::sql::FnSite],
) -> Result<Vec<super::InsertSite>, SkipReason> {
    if sites_nested(sites) {
        return Err(SkipReason::UnresolvedFnSite);
    }
    sites
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let pure = matches!(
                catalog_summary::summarize(ctx.catalog, &s.name),
                catalog_summary::Summary::PureExpr(_)
            );
            if !pure || !select_list_only(&s.distinct_sql) {
                return Err(SkipReason::UnresolvedFnSite);
            }
            Ok(super::InsertSite {
                orig_table: s.table.clone(),
                table: format!("sd_bf{i}"),
                name: s.name.clone(),
                distinct_sql: s.distinct_sql.clone(),
                binds: s.binds.clone(),
                arity: s.args.len(),
            })
        })
        .collect()
}

/// distinct_sql 仅 SELECT 列表（可选 DISTINCT 关键字）：无 FROM/WHERE/
/// GROUP/ORDER/LIMIT/JOIN/集合操作等子句（深度 0 判定）。
fn select_list_only(sql: &str) -> bool {
    let Some(t) = super::shape::Tokens::new(sql) else {
        return false;
    };
    let w = t.words();
    if w.first().map(String::as_str) != Some("select") {
        return false;
    }
    let mut i = 1;
    if w.get(i).is_some_and(|x| x == "distinct") {
        i += 1;
    }
    let mut depth = 0i32;
    for word in &w[i..] {
        match word.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "from" | "where" | "group" | "order" | "limit" | "join" | "inner" | "left"
            | "right" | "full" | "cross" | "union" | "intersect" | "except" | "having" | "into"
                if depth == 0 =>
            {
                return false
            }
            _ => {}
        }
    }
    true
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
