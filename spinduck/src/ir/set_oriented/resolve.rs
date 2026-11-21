//! 查找表结构解析与渲染调度：schema 证据填充 + IN 两侧类型对齐。

use super::lookup::{self, LookupQuery};
use super::shape;
use super::substitute_binds;
use super::{schema, viewdep, SkipReason, VarName};
use crate::catalog::RoutineCatalog;
use crate::error::EngineError;
use crate::ir::IrExpr;

/// 查找涉案表（主表 + IN 子查询读表），入口漂移复核同口径。
pub(super) type LookupTables = Vec<(shape::Name, schema::Table)>;
pub(super) type LookupResolution = Result<Vec<(lookup::ResolvedLookup, LookupTables)>, SkipReason>;
/// 提升产物：替换后的表达式 + 各查找的解析结果与涉案表。
pub(super) type HoistedLookups = Vec<(lookup::ResolvedLookup, LookupTables)>;
pub(super) type HoistResult = Result<Result<(IrExpr, HoistedLookups), SkipReason>, EngineError>;

/// 查找表结构解析 + 渲染（不变量聚合标量 / 相关 GROUP BY 派生表）；
/// IN 子查询读表一并解析，纳入漂移复核与类型对齐证据。
pub(super) fn resolve_lookups(
    ex: &schema::Source<'_>,
    catalog: &RoutineCatalog,
    lookups: &[LookupQuery],
    cur_scan: &str,
) -> Result<LookupResolution, EngineError> {
    let mut out = Vec::new();
    for (i, lq) in lookups.iter().enumerate() {
        match resolve_one(ex, catalog, lq, i + 1, cur_scan)? {
            Ok(item) => out.push(item),
            Err(reason) => return Ok(Err(reason)),
        }
    }
    Ok(Ok(out))
}

/// 单条查找解析（显式渲染序号，供函数内联 hoist 全局编号避免 JOIN 别名冲突）。
/// `cur_scan` 为游标扫描投影（LATERAL 相关 Top-1 的入口守卫自带 sd_cur）；
/// 内联提升上下文无游标形状，传空串 → LATERAL 形态保守拒绝。
pub(super) fn resolve_one(
    ex: &schema::Source<'_>,
    catalog: &RoutineCatalog,
    lq: &LookupQuery,
    index: usize,
    cur_scan: &str,
) -> Result<Result<(lookup::ResolvedLookup, LookupTables), SkipReason>, EngineError> {
    let system_case = lq.case_sink.is_some() && lq.table.0 == "sys_dummy";
    // §3.5：入口守卫查找的表可为视图——递归全定义证明失败即
    // UnprovenQueryTotalness（不发守卫 SQL）；证明产物给出投影列证据
    // 与最终底表（漂移/写集复核对象）。
    let mut proof = None;
    let mut view_dep = None;
    let table = if system_case {
        // 哑表不读任何列，入口单行守卫已涵盖全部依赖，不要求用户 DDL 声明。
        schema::Table {
            qualified: lq.table.sql(),
            columns: Vec::new(),
        }
    } else if let Some(table) = ex.load(&lq.table)? {
        table
    } else if lq.entry_guard {
        match super::vguard::prove_view(ex, &lq.table, 0)? {
            Ok(Some(p)) => {
                let t = p.table.clone();
                proof = Some(p);
                t
            }
            // 非视图（view_sql 缺失）= 对象缺失；视图存在但全定义
            // 证明失败 → 回退视图物化前导（确定性/依赖独立再证，
            // vguard 渲染只依赖 qualified/columns，sd_vwN 等价）。
            Ok(None) => return Ok(Err(SkipReason::UnknownObjectDependency)),
            Err(_) => match viewdep::materialize(ex, catalog, &lq.table, index)? {
                Ok(Some(dep)) => {
                    let t = dep.table.clone();
                    view_dep = Some(dep);
                    t
                }
                Ok(None) => return Ok(Err(SkipReason::UnknownObjectDependency)),
                Err(reason) => return Ok(Err(reason)),
            },
        }
    } else {
        // 视图物化前导：DESCRIBE 探测输出列 + 底表依赖登记，渲染改
        // 引用 sd_vwN 临时表；非视图/不可证 = UnknownObjectDependency。
        match viewdep::materialize(ex, catalog, &lq.table, index)? {
            Ok(Some(dep)) => {
                let t = dep.table.clone();
                view_dep = Some(dep);
                t
            }
            Ok(None) => return Ok(Err(SkipReason::UnknownObjectDependency)),
            Err(reason) => return Ok(Err(reason)),
        }
    };
    if lq.case_sink.is_some() {
        // CASE 写汇 + 视图物化暂不支持（渲染证据面不同），保守拒绝。
        if view_dep.is_some() {
            return Ok(Err(SkipReason::UnprovenQueryTotalness));
        }
        return Ok(resolve_case(lq, table, index, cur_scan));
    }
    let Some(col) = table.columns.iter().find(|c| c.name == lq.column) else {
        return Ok(Err(SkipReason::UnknownObjectDependency));
    };
    let (kind, nullable) = (col.kind.clone(), col.nullable);
    // 渲染证据用视图/探测投影形状；涉案表登记：入口守卫视图→最终底表，
    // 物化视图→全部底表（视图名本身不可 load，登记会恒判漂移）。
    let render_table = table.clone();
    let seed = match &view_dep {
        Some(dep) => dep.bases.clone(),
        None => vec![(lq.table.clone(), table.clone())],
    };
    let mut tables = match load_in_tables(ex, lq, &render_table, seed)? {
        Ok(tables) => tables,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Some(p) = &proof {
        tables[0] = (p.base.clone(), p.base_table.clone());
    }
    let render = match lookup::render(lq, &render_table, &tables[1..], index, cur_scan) {
        Ok(r) => r,
        Err(reason) => return Ok(Err(reason)),
    };
    Ok(Ok((
        lookup::ResolvedLookup {
            var: lq.var.clone(),
            kind,
            nullable,
            render,
            group: lq.group,
            filter_on_miss: lq.filter_on_miss,
            case_sink: None,
            deps: Vec::new(),
            guard_base: proof.map(|p| p.base),
            view_prelude: view_dep.as_ref().map(|d| d.prelude.clone()),
            view_bases: view_dep.map(|d| d.base_names).unwrap_or_default(),
            filter_binds: lq.filter_binds.clone(),
            default_var: lq.default_var.clone(),
        },
        tables,
    )))
}

fn resolve_case(
    lq: &LookupQuery,
    table: schema::Table,
    index: usize,
    cur_scan: &str,
) -> Result<(lookup::ResolvedLookup, LookupTables), SkipReason> {
    let render = lookup::render(lq, &table, &[], index, cur_scan)?;
    Ok((
        lookup::ResolvedLookup {
            var: lq.var.clone(),
            kind: String::new(),
            nullable: false,
            render,
            group: lq.group,
            filter_on_miss: false,
            case_sink: lq.case_sink.clone(),
            deps: Vec::new(),
            guard_base: None,
            view_prelude: None,
            view_bases: Vec::new(),
            filter_binds: lq.filter_binds.clone(),
            default_var: lq.default_var.clone(),
        },
        if lq.table.0 == "sys_dummy" {
            Vec::new()
        } else {
            vec![(lq.table.clone(), table)]
        },
    ))
}

/// `kind_src` 为 IN 两侧类型对齐的列证据源（渲染表：普通表/入口守卫
/// 视图投影/物化视图探测列）；`seed` 为涉案表证据初值（物化视图为
/// 全部底表，普通查找为主表本身）。
fn load_in_tables(
    ex: &schema::Source<'_>,
    lq: &LookupQuery,
    kind_src: &schema::Table,
    seed: Vec<(shape::Name, schema::Table)>,
) -> Result<Result<LookupTables, SkipReason>, EngineError> {
    let mut tables = seed;
    for f in &lq.in_filters {
        let Some(inner) = ex.load(&f.table)? else {
            return Ok(Err(SkipReason::UnknownObjectDependency));
        };
        let outer_kind = kind_src
            .columns
            .iter()
            .find(|c| c.name == f.column)
            .map(|c| c.kind.clone());
        let inner_kind = inner
            .columns
            .iter()
            .find(|c| c.name == f.inner_column)
            .map(|c| c.kind.clone());
        match (outer_kind, inner_kind) {
            (Some(a), Some(b)) if a == b => {}
            _ => return Ok(Err(SkipReason::UnprovenQueryTotalness)),
        }
        tables.push((f.table.clone(), inner));
    }
    Ok(Ok(tables))
}

/// LookupTuple 解析（§3.2）：加载涉案表 + 渲染单条 GROUP BY 派生表；
/// `base_index` 接续查找编号避免 JOIN 别名冲突。
pub(super) fn resolve_tuples(
    ex: &schema::Source<'_>,
    tuples: &[lookup::TupleQuery],
    base_index: usize,
) -> Result<Result<Vec<(lookup::ResolvedTuple, LookupTables)>, SkipReason>, EngineError> {
    let mut out = Vec::new();
    for (i, tq) in tuples.iter().enumerate() {
        let Some(table) = ex.load(&tq.table)? else {
            return Ok(Err(SkipReason::UnknownObjectDependency));
        };
        match lookup::render_tuple(tq, &table, base_index + i) {
            Ok(rt) => out.push((rt, vec![(tq.table.clone(), table)])),
            Err(reason) => return Ok(Err(reason)),
        }
    }
    Ok(Ok(out))
}

/// §3.4 聚合查表解析：加载维表 + 渲染 GROUP BY 派生表/LEFT JOIN；
/// `base_index` 接续查找/元组编号避免 JOIN 别名冲突。
pub(super) fn resolve_aggs(
    ex: &schema::Source<'_>,
    aggs: &[super::agg::AggQuery],
    base_index: usize,
) -> Result<Result<Vec<(super::agg::ResolvedAgg, LookupTables)>, SkipReason>, EngineError> {
    let mut out = Vec::new();
    for (i, aq) in aggs.iter().enumerate() {
        let Some(table) = ex.load(&aq.table)? else {
            return Ok(Err(SkipReason::UnknownObjectDependency));
        };
        match super::agg::render(aq, &table, base_index + i) {
            Ok(ra) => out.push((ra, vec![(aq.table.clone(), table)])),
            Err(reason) => return Ok(Err(reason)),
        }
    }
    Ok(Ok(out))
}

/// 内联提升上下文：schema 源、例程目录、游标记录与 Var 实参安全集
/// （查找结果变量 ∪ 循环不变变量；其余 Var 实参保守拒绝）。
pub(super) struct HoistCtx<'a, 'b> {
    pub ex: &'a schema::Source<'b>,
    pub catalog: &'a RoutineCatalog,
    pub record: &'a VarName,
    pub safe_vars: &'a std::collections::BTreeSet<VarName>,
    /// 已解析查找的结果变量集（default 为查找变量时允许 $v_ 占位引用）。
    pub lookup_vars: &'a std::collections::BTreeSet<VarName>,
    /// 区域游标扫描 SQL（verbatim 原文/普通投影）：LATERAL Top-1
    /// 并列守卫的 sd_cur 来源；空串 = 无游标形状（该类查找拒绝）。
    pub cur_scan: &'a str,
}

/// 内联表达式中的函数内查找（§4.3 A7）关系化提升：把每个 `IrExpr::Lookup`
/// 替换为合成变量 `__sd_lkN`，并以 GROUP BY 派生表 LEFT JOIN 渲染（相关
/// 聚合标量子查询深嵌套会触发内核规划栈溢出，关系化后栈深恒定）。
/// 合成变量在批写投影里替换为 `CASE WHEN hit THEN value ELSE default END`。
/// `base_index` 接续过程级查找编号，避免 JOIN 别名冲突。
pub(super) fn hoist_inline(
    ctx: &HoistCtx<'_, '_>,
    expr: &IrExpr,
    base_index: usize,
) -> HoistResult {
    let mut acc = Vec::new();
    let mut next = base_index;
    match hoist_walk(ctx, expr, &mut next, &mut acc) {
        Ok(e) => Ok(Ok((e, acc))),
        Err(HoistFail::Skip(r)) => Ok(Err(r)),
        Err(HoistFail::Eng(e)) => Err(e),
    }
}

/// 提升失败的统一错误：保守拒绝（Skip）与内核错误（Eng）分流，
/// 使递归走查可用 `?` 直传，外层再还原为 generate 的双层 Result。
enum HoistFail {
    Skip(SkipReason),
    Eng(EngineError),
}

impl From<SkipReason> for HoistFail {
    fn from(r: SkipReason) -> Self {
        HoistFail::Skip(r)
    }
}

impl From<EngineError> for HoistFail {
    fn from(e: EngineError) -> Self {
        HoistFail::Eng(e)
    }
}

fn hoist_walk(
    ctx: &HoistCtx<'_, '_>,
    e: &IrExpr,
    next: &mut usize,
    acc: &mut HoistedLookups,
) -> Result<IrExpr, HoistFail> {
    Ok(match e {
        IrExpr::Lookup {
            query,
            args,
            default,
        } => {
            // 兜底表达式可能内嵌 Lookup（内联例程的 WHEN OTHERS 分支）：
            // 先递归提升为 __sd_lkN，外层 default 以 $v_ 占位引用，
            // 由 substitute_lookup_refs 逐行代入内层 CASE。
            let default = hoist_walk(ctx, default, next, acc)?;
            let mut lookup_vars = ctx.lookup_vars.clone();
            lookup_vars.extend(acc.iter().map(|(l, _)| l.var.clone()));
            let ictx = lookup::InlineCtx {
                record: ctx.record,
                safe_vars: ctx.safe_vars,
                lookup_vars: &lookup_vars,
            };
            let lq = match lookup::parse_inline(query, args, &default, &ictx) {
                Ok(lq) => lq,
                Err(r) => {
                    return Err(HoistFail::Skip(r));
                }
            };
            let idx = *next;
            *next += 1;
            let inner = resolve_one(ctx.ex, ctx.catalog, &lq, idx, ctx.cur_scan)?;
            let (mut resolved, tables) = match inner {
                Ok(x) => x,
                Err(r) => {
                    return Err(HoistFail::Skip(r));
                }
            };
            let var = VarName(format!("__sd_lk{idx}"));
            resolved.var = var.clone();
            resolved.group = idx;
            acc.push((resolved, tables));
            IrExpr::Var(var)
        }
        IrExpr::Bin { op, l, r } => IrExpr::Bin {
            op: *op,
            l: Box::new(hoist_walk(ctx, l, next, acc)?),
            r: Box::new(hoist_walk(ctx, r, next, acc)?),
        },
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => hoist_case(ctx, (operand, whens, else_expr), next, acc)?,
        IrExpr::Not(x) => IrExpr::Not(Box::new(hoist_walk(ctx, x, next, acc)?)),
        IrExpr::IsNull(x) => IrExpr::IsNull(Box::new(hoist_walk(ctx, x, next, acc)?)),
        IrExpr::SqlFn {
            name,
            args,
            prepared,
        } => {
            let args = args
                .iter()
                .map(|a| hoist_walk(ctx, a, next, acc))
                .collect::<Result<Vec<_>, _>>()?;
            IrExpr::SqlFn {
                name: name.clone(),
                args,
                prepared: prepared.clone(),
            }
        }
        _ => e.clone(),
    })
}

type CaseParts<'a> = (
    &'a Option<Box<IrExpr>>,
    &'a [(IrExpr, IrExpr)],
    &'a Option<Box<IrExpr>>,
);

fn hoist_case(
    ctx: &HoistCtx<'_, '_>,
    case: CaseParts<'_>,
    next: &mut usize,
    acc: &mut HoistedLookups,
) -> Result<IrExpr, HoistFail> {
    let (operand, whens, else_expr) = case;
    let operand = match operand {
        Some(o) => Some(Box::new(hoist_walk(ctx, o, next, acc)?)),
        None => None,
    };
    let mut ws = Vec::with_capacity(whens.len());
    for (c, v) in whens {
        ws.push((
            hoist_walk(ctx, c, next, acc)?,
            hoist_walk(ctx, v, next, acc)?,
        ));
    }
    let else_expr = match else_expr {
        Some(o) => Some(Box::new(hoist_walk(ctx, o, next, acc)?)),
        None => None,
    };
    Ok(IrExpr::Case {
        operand,
        whens: ws,
        else_expr,
    })
}

/// 跨查找渲染替换（§4.4 B1，语料 s.fund_code = v_fund_code）：查找 L 的静态
/// 过滤引用另一逐行查找 X 的结果变量（`$v_<X>`，循环可变非常量帧值）时，以
/// X 的批写投影 CASE 逐行代入 L 的 join/hit/value/tie_guard；tie_guard 自包含
/// 注入 X 及传递依赖的派生表/连接（守卫以空帧绑定独立求值）；记录 deps 供批写
/// 按拓扑登记连接（L 的 LATERAL 引用 X 别名，X 连接须在前）；移除该帧绑定。
/// 依赖成环 = 保守拒绝（UnprovenQueryTotalness）。
pub(super) fn substitute_lookup_refs(
    resolved: &mut [(lookup::ResolvedLookup, LookupTables)],
) -> Result<(), SkipReason> {
    let vars: Vec<VarName> = resolved.iter().map(|(l, _)| l.var.clone()).collect();
    let orig: Vec<lookup::LookupRender> = resolved.iter().map(|(l, _)| l.render.clone()).collect();
    let n = orig.len();
    let order = topo_order(&orig, &vars)?;
    let mut renders = orig.clone();
    let mut ctes: Vec<Vec<String>> = vec![Vec::new(); n];
    let mut joins: Vec<Vec<String>> = vec![Vec::new(); n];
    let mut direct: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in order {
        let deps = render_refs(&orig[i], &vars);
        direct[i] = deps.clone();
        for &d in &deps {
            let dc = ctes[d].clone();
            let dj = joins[d].clone();
            merge_unique(&mut ctes[i], &dc);
            merge_unique(&mut joins[i], &dj);
            if let Some(c) = &renders[d].cte {
                push_unique(&mut ctes[i], c);
            }
            if let Some(j) = &renders[d].join {
                push_unique(&mut joins[i], j);
            }
        }
        let mut r = orig[i].clone();
        // 先收集全部 (占位符, CASE) 替换对再单遍引号感知替换：字面量内
        // 的同名占位符文本不被污染，替换文本也不被后续占位符重扫（§4.6）。
        let mut repls: Vec<(String, String)> = Vec::with_capacity(deps.len());
        for &d in &deps {
            let ph = format!("$v_{}", vars[d].0);
            repls.push((
                ph.clone(),
                format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    renders[d].hit, renders[d].value, renders[d].default_sql
                ),
            ));
            r.guard_binds.retain(|(p, _)| p != &ph);
        }
        repls.sort_by_key(|(ph, _)| std::cmp::Reverse(ph.len()));
        r.join = r.join.map(|s| substitute_binds(&s, &repls));
        r.hit = substitute_binds(&r.hit, &repls);
        r.value = substitute_binds(&r.value, &repls);
        // 内联 Lookup 的兜底可为查找变量（$v_<var> 占位）：同样逐行代入
        // 内层 CASE（hit→value 否则内层自身兜底），依赖经 render_refs
        // 计入拓扑序。
        r.default_sql = substitute_binds(&r.default_sql, &repls);
        if let Some(tg) = &r.tie_guard {
            r.tie_guard = Some(inject_tie_guard(
                &substitute_binds(tg, &repls),
                &ctes[i],
                &joins[i],
            ));
        }
        renders[i] = r;
    }
    for (i, slot) in resolved.iter_mut().enumerate() {
        slot.0.render = renders[i].clone();
        slot.0.deps = direct[i].iter().map(|&d| vars[d].clone()).collect();
    }
    Ok(())
}

/// 依赖拓扑后序（被引用者先于引用者）；visiting 重入 = 成环拒绝。
fn topo_order(orig: &[lookup::LookupRender], vars: &[VarName]) -> Result<Vec<usize>, SkipReason> {
    let mut state = vec![0u8; orig.len()];
    let mut order = Vec::new();
    for i in 0..orig.len() {
        topo_dfs(i, orig, vars, &mut state, &mut order)?;
    }
    Ok(order)
}

fn topo_dfs(
    i: usize,
    orig: &[lookup::LookupRender],
    vars: &[VarName],
    state: &mut [u8],
    order: &mut Vec<usize>,
) -> Result<(), SkipReason> {
    match state[i] {
        2 => return Ok(()),
        1 => return Err(SkipReason::UnprovenQueryTotalness),
        _ => {}
    }
    state[i] = 1;
    for d in render_refs(&orig[i], vars) {
        topo_dfs(d, orig, vars, state, order)?;
    }
    state[i] = 2;
    order.push(i);
    Ok(())
}

/// 渲染文本（join/hit/value/tie_guard）引用的查找变量下标：精确提取 `$v_<ident>`
/// 占位符并与查找变量名求交（形参绑定/游标字段不在 vars 中，自然忽略）。
fn render_refs(r: &lookup::LookupRender, vars: &[VarName]) -> Vec<usize> {
    // 字段间以空格分隔：占位符标识符止于非标识字符，防跨字段粘连
    // （如 default_sql 末尾的 $v_<var> 与 join 起始的 LEFT 合并为
    // "__sd_lkNLEFT" 而漏识别依赖）。
    let mut text = r.hit.clone();
    text.push(' ');
    text.push_str(&r.value);
    text.push(' ');
    text.push_str(&r.default_sql);
    if let Some(j) = &r.join {
        text.push(' ');
        text.push_str(j);
    }
    if let Some(t) = &r.tie_guard {
        text.push(' ');
        text.push_str(t);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = text[i..].find("$v_") {
        let start = i + p + 3;
        let mut end = start;
        while end < text.len()
            && (text.as_bytes()[end].is_ascii_alphanumeric() || text.as_bytes()[end] == b'_')
        {
            end += 1;
        }
        if let Some(idx) = vars.iter().position(|v| v.0 == text[start..end]) {
            if !out.contains(&idx) {
                out.push(idx);
            }
        }
        i = start.max(end);
    }
    out
}

/// 入口并列守卫自包含：把依赖派生表注入 WITH（sd_cur CTE 之后）、依赖连接
/// 注入 `FROM sd_cur` 之后（tie LATERAL 之前）。守卫恒以
/// `WITH sd_cur AS (…) SELECT COUNT(*) FROM sd_cur LEFT JOIN LATERAL` 起首。
fn inject_tie_guard(tg: &str, ctes: &[String], joins: &[String]) -> String {
    if ctes.is_empty() && joins.is_empty() {
        return tg.to_string();
    }
    let Some(pos) = tg.find("SELECT COUNT(*) FROM sd_cur") else {
        return tg.to_string();
    };
    let head = tg[..pos].trim_end();
    let tail = &tg[pos..];
    let head = if ctes.is_empty() {
        head.to_string()
    } else {
        format!("{head}, {}", ctes.join(", "))
    };
    let tail = if joins.is_empty() {
        tail.to_string()
    } else {
        tail.replacen(
            "FROM sd_cur",
            &format!("FROM sd_cur {}", joins.join(" ")),
            1,
        )
    };
    format!("{head} {tail}")
}

fn merge_unique(dst: &mut Vec<String>, src: &[String]) {
    for s in src {
        push_unique(dst, s);
    }
}

fn push_unique(dst: &mut Vec<String>, s: &str) {
    if !dst.iter().any(|x| x == s) {
        dst.push(s.to_string());
    }
}
