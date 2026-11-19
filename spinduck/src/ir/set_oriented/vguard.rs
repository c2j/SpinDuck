//! §3.5 V3-5：v_par_bond 零异常入口守卫——视图递归全定义证明与
//! 1:1 LEFT JOIN + 计数守卫渲染。处理器不改 INTO 变量（跨行残留），
//! 命中基数由入口守卫在运行期裁决：恰 1 行放行集合 SQL，0/多行整段
//! 退回原解释器（保留 too_many_rows/no_data_found 语义）。

use super::lookup::{parse_total, CorrelationOp, LookupQuery, LookupRender};
use super::region::{self, CursorRegion};
use super::schema::{self, Source, Table};
use super::shape::{Name, Tokens};
use super::{SkipReason, VarName};
use crate::error::EngineError;
use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt, TrapEntry};
use crate::value::SpinValue;

/// 视图证明产物：投影列证据（视图形状）+ 底表名与底表结构
/// （写集相交复核与入口漂移复核用）。
pub(super) struct ViewProof {
    pub table: Table,
    pub base: Name,
    pub base_table: Table,
}

/// 递归展开视图：SELECT 列表仅允许裸列引用（可 `AS`/隐式别名、可限定
/// 符），FROM 单表（可选别名）；底表为视图时递归（深度上限防环）。
/// WHERE/GROUP BY/函数/CAST/连接一律 UnprovenQueryTotalness。
/// 非视图名返回 Ok(None)，交回调用方按普通表缺失处理。
pub(super) fn prove_view(
    ex: &Source<'_>,
    name: &Name,
    depth: usize,
) -> Result<Result<Option<ViewProof>, SkipReason>, EngineError> {
    if depth > 8 {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    }
    let Some(sql) = ex.view_sql(name)? else {
        return Ok(Ok(None));
    };
    let Some(shape) = view_shape(&sql) else {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    };
    let ViewShape {
        items,
        base,
        alias,
        has_where,
    } = shape;
    if has_where {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    }
    // cols_source：外层列解析依据（底表本身或内层视图投影）；
    // (base, base_table)：最终底表名与结构（写集相交/入口漂移复核）。
    let (cols_source, base, base_table) = match ex.load(&base)? {
        Some(t) => {
            let bt = t.clone();
            (t, base.clone(), bt)
        }
        None => match prove_view(ex, &base, depth + 1)? {
            Ok(Some(p)) => (p.table, p.base, p.base_table),
            _ => return Ok(Err(SkipReason::UnprovenQueryTotalness)),
        },
    };
    let mut columns = Vec::with_capacity(items.len());
    for (qual, src, out) in items {
        if let Some(q) = qual {
            if q != base && alias.as_ref() != Some(&q) {
                return Ok(Err(SkipReason::UnprovenQueryTotalness));
            }
        }
        let Some(col) = cols_source.columns.iter().find(|c| c.name == src) else {
            return Ok(Err(SkipReason::UnprovenQueryTotalness));
        };
        columns.push(schema::Column {
            name: out,
            kind: col.kind.clone(),
            nullable: col.nullable,
            default: None,
        });
    }
    Ok(Ok(Some(ViewProof {
        table: Table {
            qualified: name.sql(),
            columns,
        },
        base,
        base_table,
    })))
}

/// 视图投影项：(限定符, 源列, 输出名)。
type ViewItem = (Option<Name>, Name, Name);

/// 视图 DDL 形状解析产物。
struct ViewShape {
    items: Vec<ViewItem>,
    base: Name,
    alias: Option<Name>,
    has_where: bool,
}

/// 视图 DDL 形状：`CREATE [OR REPLACE] VIEW <名> AS SELECT <裸列项>
/// FROM <表> [别名]`；WHERE/GROUP BY/函数/CAST/连接等其余形态 = None。
fn view_shape(sql: &str) -> Option<ViewShape> {
    let mut t = Tokens::new(sql)?;
    if !t.take("create") {
        return None;
    }
    if t.take("or") {
        if !(t.take("replace") && t.take("view")) {
            return None;
        }
    } else if !t.take("view") {
        return None;
    }
    t.name()?;
    if !t.take("as") || !t.take("select") {
        return None;
    }
    let mut items = Vec::new();
    loop {
        let first = t.name()?;
        let (qual, src) = if t.take(".") {
            (Some(first), t.name()?)
        } else {
            (None, first)
        };
        let out = if t.take("as") || t.peek().is_some_and(|w| w != "," && w != "from") {
            t.name()?
        } else {
            src.clone()
        };
        items.push((qual, src, out));
        if !t.take(",") {
            break;
        }
    }
    if !t.take("from") {
        return None;
    }
    let base = t.name()?;
    let alias = if t.peek().is_some_and(|w| w != "where") {
        t.name()
    } else {
        None
    };
    let has_where = t.take("where") || !t.done();
    Some(ViewShape {
        items,
        base,
        alias,
        has_where,
    })
}

/// 零异常守卫渲染：主查询 1:1 LEFT JOIN 已过滤日期的视图投影
/// （不用 MAX/HAVING/外层 __cnt=1 滤行）；入口守卫先在子查询做日期
/// 过滤、仅按等值关联键 GROUP BY 计数，外层 COALESCE(__cnt,0)<>1，
/// 期望 COUNT(*)=0（复用 top1_guards 槽位，非零即整段退回）。
pub(super) fn render(
    lq: &LookupQuery,
    table: &Table,
    index: usize,
    cur_scan: &str,
) -> Result<LookupRender, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if cur_scan.is_empty()
        || lq.correlations.is_empty()
        || !lq.in_filters.is_empty()
        || !lq.in_lists.is_empty()
        || lq.top1.is_some()
        || lq.case_sink.is_some()
    {
        return Err(bad);
    }
    let alias = format!("sd_lk{index}");
    let mut dims = Vec::new();
    let mut ons = Vec::new();
    let mut key_checks = Vec::new();
    for c in &lq.correlations {
        let Some(key_col) = table.columns.iter().find(|col| col.name == c.dim_column) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        key_checks.push((key_col.kind.clone(), c.cursor_field.clone()));
        dims.push(c.dim_column.sql());
        ons.push(format!(
            "{alias}.{} = sd_cur.{}",
            c.dim_column.sql(),
            c.cursor_field.sql()
        ));
    }
    let filter = lq.filter.clone();
    let col = lq.column.sql();
    let join = format!(
        "LEFT JOIN (SELECT {}, {} AS sd_val, 1 AS sd_hit FROM {}{}) {alias} ON {}",
        dims.join(", "),
        col,
        table.qualified,
        filter,
        ons.join(" AND ")
    );
    let group_by = dims.join(", ");
    let g_ons = lq
        .correlations
        .iter()
        .map(|c| format!("_b.{} = _cur.{}", c.dim_column.sql(), c.cursor_field.sql()))
        .collect::<Vec<_>>()
        .join(" AND ");
    let tie_guard = format!(
        "SELECT COUNT(*) FROM ({cur_scan}) _cur LEFT JOIN \
         (SELECT {group_by}, COUNT(*) AS __cnt FROM {}{filter} GROUP BY {group_by}) _b \
         ON {g_ons} WHERE COALESCE(_b.__cnt, 0) <> 1",
        table.qualified
    );
    let value = match &lq.nvl_default {
        Some(d) => format!("CASE WHEN {alias}.sd_val IS NULL THEN {d} ELSE {alias}.sd_val END"),
        None => format!("{alias}.sd_val"),
    };
    Ok(LookupRender {
        cte: None,
        join: Some(join),
        hit: format!("{alias}.sd_hit = 1"),
        value,
        default_sql: lq.default_sql.clone(),
        key_checks,
        tie_guard: Some(tie_guard),
        guard_binds: lq.filter_binds.clone(),
    })
}

/// 零异常守卫型处理器结构面（§3.5 V3-5）：同一 WHEN OTHERS 内恰 1 条
/// SELECT INTO，处理器体为常量赋值（不得触碰 INTO 变量——异常路径保留
/// 上一行残留）+ 同目标续跑 Jump。返回 (查询变量, 续跑 pc)；形态不符 = None。
pub(super) fn carryover_group(
    prog: &IrProg,
    pc: usize,
    entry: &TrapEntry,
) -> Option<(VarName, usize)> {
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return None;
    }
    let IrStmt::SqlQuery { into, fn_sites, .. } = prog.code.get(pc + 1)? else {
        return None;
    };
    let [AssignTarget::Var(v)] = into.as_slice() else {
        return None;
    };
    if !fn_sites.is_empty() {
        return None;
    }
    if !matches!(prog.code.get(pc + 2), Some(IrStmt::PopTrap))
        || !matches!(prog.code.get(pc + 3), Some(IrStmt::Jump(t)) if *t == entry.body_end + 1)
        || !matches!(prog.code.get(entry.body_end), Some(IrStmt::Jump(t)) if *t == entry.body_end + 1)
    {
        return None;
    }
    if entry.body_start != pc + 4 || entry.body_end <= entry.body_start {
        return None;
    }
    for bpc in entry.body_start..entry.body_end {
        let IrStmt::Assign {
            target: AssignTarget::Var(av),
            expr: IrExpr::Const(_),
        } = prog.code.get(bpc)?
        else {
            return None;
        };
        if av == v {
            return None;
        }
    }
    Some((v.clone(), entry.body_end + 1))
}

/// 识别 pc 处的零异常守卫查找（§3.5 V3-5）：PushTrap(WHEN OTHERS) 内
/// 恰 1 条 SELECT INTO，处理器只写其它变量常量、不改 INTO 变量（异常
/// 路径保留跨行残留值），续跑落点在处理器块之后。解析复用 parse_total
/// （兜底常量取 Null——守卫保证恰 1 行命中，ELSE 分支运行期不可达）。
/// 形态不符 = Ok(None)；全定义证明失败 = Err(UnprovenQueryTotalness)；
/// 视图/表与区域写集相交 = Err(TargetReadDependency)。
pub(super) fn carryover_group_at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<Option<(LookupQuery, usize)>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let [entry] = entries.as_slice() else {
        return Ok(None);
    };
    let Some((var, resume)) = carryover_group(prog, pc, entry) else {
        return Ok(None);
    };
    let IrStmt::SqlQuery { sql, binds, .. } = &prog.code[pc + 1] else {
        return Ok(None);
    };
    let mut query = parse_total(sql, var, SpinValue::Null, binds, &region.record)?;
    if query.correlations.iter().any(|c| c.op != CorrelationOp::Eq) {
        return Err(SkipReason::UnprovenQueryTotalness);
    }
    query.entry_guard = true;
    let writes = region::write_set(prog, region, catalog)?;
    if writes.contains(&query.table.0)
        || query.in_filters.iter().any(|f| writes.contains(&f.table.0))
    {
        return Err(SkipReason::TargetReadDependency);
    }
    Ok(Some((query, resume)))
}
