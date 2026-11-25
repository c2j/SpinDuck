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
pub(in crate::ir::set_oriented) use badrow::sd_cur_rows;
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
    // rn = 混合调度（so-exception-row-hybrid）：sd_cur 附 `__sd_rn`
    // 0 基行号列，外层 WHERE 追加 `BETWEEN __SD_LO__/__SD_HI__` 段谓词
    // 占位符（引擎按段替换）。
    rn: bool,
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
        kernel_guards: Vec::new(),
    };
    inputs::prove(region, &env)?;
    for l in lookups.iter().filter(|l| l.filter_on_miss) {
        guards.push(group_hits(&[l], select, source)?);
        register_lookup(l, lookups, env.ctes, env.joins);
    }
    for (var, value) in &region.inline_checks {
        inline::check_evaluation(region, (var, value), &mut env)?;
    }
    let (projections, write_guards, kernel_guards) =
        write_projections(region, insert, target, &mut env)?;
    let fn_preludes = fn_distincts(region, select, source, &mut env)?;
    // 内核域守卫必须先于写守卫坏行计数：COUNT 求值顺序保证溢出行
    // 先触发整体回退，写守卫 COUNT 不会对含溢出算术的行求值。
    badrow::kernel_row_guard(
        &kernel_guards,
        select,
        source,
        (&ctes, &joins),
        &mut expr_guards,
    );
    if region.insert_log || !kernel_guards.is_empty() {
        badrow::bad_row_guard(
            &write_guards,
            select,
            source,
            (&ctes, &joins),
            &mut expr_guards,
        );
    }
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
        rn,
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
            let with = std::iter::once(sd_cur_cte(select, source, false))
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

/// 写投影产物：SELECT 列 / 行级守卫 / 内核域守卫。
type WriteProjections = (Vec<String>, Vec<String>, Vec<String>);

fn write_projections(
    region: &CopyLoop<'_>,
    insert: &shape::Insert,
    target: &Table,
    env: &mut ProjEnv<'_>,
) -> Result<WriteProjections, SkipReason> {
    let mut projections = Vec::new();
    let mut guards = Vec::new();
    let mut kernel_guards = Vec::new();
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
        if !guarded.kernel_guard.is_empty() {
            kernel_guards.push(guarded.kernel_guard);
        }
        if !guarded.guard.is_empty() {
            guards.push(guarded.guard);
        }
    }
    kernel_guards.append(&mut env.kernel_guards);
    Ok((projections, guards, kernel_guards))
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
            return field_projection(env.select, env.source, to, &field);
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
        // D7 THEN 臂活常量折叠：字面量逐行同值，写汇 TRY_CAST 守卫同口径。
        if let Some((_, c)) = region.folded.iter().find(|(fv, _)| fv == v) {
            let lit = crate::sql::render::const_to_sql(c).ok_or(SkipReason::UnprovenExpression)?;
            return Ok(expr::at_write_sink(lit, to));
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
    if let Some((_, value)) = region.inlined.iter().find(|(v, _)| v == var) {
        return inline::numeric_projection(region, (var, value), env);
    }
    let Some(lq) = env.lookups.iter().find(|l| &l.var == var) else {
        return Err(bad);
    };
    // ArithWrite 覆写变量：值 = CASE WHEN cond THEN arith ELSE 原值，
    // 须先于查找分支判定（同名变量同时存在于 lookups）。
    if let Some(write) = region.arith_writes.iter().find(|w| w.var == *var) {
        return arith::value_binding(region, write, env);
    }
    // 兜底（非跳行）查找：CASE 操作数/比较等非数值位置的值来源——
    // 命中取值/落空兜底自渲染，Other 域使算术合并自然拒绝误用。
    if !lq.filter_on_miss && lq.case_sink.is_none() {
        let members: Vec<_> = env.lookups.iter().filter(|l| l.group == lq.group).collect();
        let hits = group_hits(&members, env.select, env.source)?;
        for member in &members {
            register_lookup(member, env.lookups, env.ctes, env.joins);
        }
        return Ok(expr::BoundExpr {
            value: expr::GuardedExpr {
                expr: lookup::typed_case(&hits, &lq.render.value, &lq.render.default_sql, &lq.kind),
                guard: String::new(),
                kernel_guard: String::new(),
            },
            domain: expr::Domain::Other,
            dbl: None,
            safe: true,
        });
    }
    if region.var_types.get(&var.0) == Some(&crate::types::SpinType::Varchar) {
        return Err(bad);
    }
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
            kernel_guard: String::new(),
        },
        domain: expr::Domain::Numeric,
        dbl: Some(format!("CAST(({}) AS DOUBLE)", lq.render.value)),
        safe: true,
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
        expr: lookup::typed_case(&hits, &lq.render.value, &lq.render.default_sql, &lq.kind),
        guard: String::new(),
        kernel_guard: String::new(),
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
        kernel_guard: String::new(),
    }
}

fn render_copy(
    select: &shape::Select,
    source: &Table,
    sink: (&shape::Insert, &Table),
    projections: Vec<String>,
    parts: (Vec<String>, Vec<String>, Vec<String>),
    rn: bool,
) -> String {
    let (insert, target) = sink;
    let (guards, ctes, joins) = parts;
    let mut filter = if guards.is_empty() {
        String::new()
    } else {
        format!(" WHERE ({})", guards.join(") AND ("))
    };
    if rn {
        // 混合段谓词：占位符由引擎按段替换为字面量；NULL = 无界
        // （exc 空集时普通 SO 路径以 NULL,NULL 覆盖全行，行为不变）。
        // __SD_HI__ 为排他上界（0 基半开区间）。
        let pred = "(sd_cur.__sd_rn >= COALESCE(__SD_LO__, -1)) AND (sd_cur.__sd_rn < COALESCE(__SD_HI__, 9223372036854775807))";
        filter = if filter.is_empty() {
            format!(" WHERE {pred}")
        } else {
            format!("{filter} AND {pred}")
        };
    }
    let sd_cur = sd_cur_cte(select, source, rn);
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
) -> Result<expr::GuardedExpr, SkipReason> {
    let from = source_column(select, source, field)?;
    let expr = format!("sd_cur.{}", field.sql());
    if from.kind == to.kind && !(from.nullable && !to.nullable) {
        return Ok(expr::GuardedExpr {
            expr,
            guard: String::new(),
            kernel_guard: String::new(),
        });
    }
    // DECIMAL→DECIMAL 窄化/可空性收紧：批写隐式 cast 与解释器同内核语义，
    // 越界或 NULL 违例行由内核域守卫计数、非零整体回退复现解释器错误。
    if from.kind.starts_with("decimal(") && to.kind.starts_with("decimal(") {
        let sink = expr::at_write_sink(expr.clone(), to);
        return Ok(expr::GuardedExpr {
            expr,
            guard: String::new(),
            kernel_guard: sink.guard,
        });
    }
    Err(SkipReason::UnprovenTargetConstraints)
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
        kernel_guard: String::new(),
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
