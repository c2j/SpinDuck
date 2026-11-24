//! §3.5 V3-5：v_par_bond 零异常入口守卫——视图递归全定义证明与
//! 1:1 LEFT JOIN + 计数守卫渲染。处理器不改 INTO 变量（跨行残留），
//! 命中基数由入口守卫在运行期裁决：恰 1 行放行集合 SQL，0/多行整段
//! 退回原解释器（保留 too_many_rows/no_data_found 语义）。

use super::lookup::{parse_total, CorrelationOp, LookupQuery, LookupRender, MissOut};
use super::region::{self, CursorRegion};
use super::schema::{self, Source, Table};
use super::shape::{Name, Tokens};
use super::{BindSrc, SkipReason, VarName};
use crate::error::EngineError;
use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt, TrapEntry};

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
    let Some((cols_source, base, base_table)) = resolve_base(ex, &base, depth)? else {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    };
    let Some(columns) = view_columns(&items, &cols_source, &base, &alias) else {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    };
    Ok(Ok(Some(ViewProof {
        table: Table {
            qualified: name.sql(),
            columns,
        },
        base,
        base_table,
    })))
}

/// 底表解析：普通表直接取结构；视图递归证明。返回 (列解析依据表,
/// 最终底表名, 最终底表结构)；无法证明 = Ok(None)。
fn resolve_base(
    ex: &Source<'_>,
    base: &Name,
    depth: usize,
) -> Result<Option<(Table, Name, Table)>, EngineError> {
    if let Some(t) = ex.load(base)? {
        let bt = t.clone();
        return Ok(Some((t, base.clone(), bt)));
    }
    match prove_view(ex, base, depth + 1)? {
        Ok(Some(p)) => Ok(Some((p.table, p.base, p.base_table))),
        _ => Ok(None),
    }
}

/// 投影项 → 视图列证据：限定符须为底表名或其别名，源列须存在于
/// 列解析依据表；任一不符 = None（调用方报 UnprovenQueryTotalness）。
fn view_columns(
    items: &[ViewItem],
    cols_source: &Table,
    base: &Name,
    alias: &Option<Name>,
) -> Option<Vec<schema::Column>> {
    let mut columns = Vec::with_capacity(items.len());
    for (qual, src, out) in items {
        if let Some(q) = qual {
            if q != base && alias.as_ref() != Some(q) {
                return None;
            }
        }
        let col = cols_source.columns.iter().find(|c| c.name == *src)?;
        columns.push(schema::Column {
            name: out.clone(),
            kind: col.kind.clone(),
            nullable: col.nullable,
            default: None,
        });
    }
    Some(columns)
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
    let KeyParts {
        dims,
        ons,
        key_checks,
    } = key_parts(lq, table, &alias)?;
    let filter = lq.filter.clone();
    let value = match &lq.nvl_default {
        Some(d) => format!("CASE WHEN {alias}.sd_val IS NULL THEN {d} ELSE {alias}.sd_val END"),
        None => format!("{alias}.sd_val"),
    };
    // miss 仿真（证据齐备）：守卫放宽为 dup-only + 混合/均匀守卫，
    // miss 行取值 = INTO 变量初始化常量（经 default_sql 渲染进
    // `CASE WHEN hit THEN value ELSE init END`）；处理器赋值由
    // 批写后标量出口回填。
    let (tie_guard, default_sql, miss_guards, miss_out, exc_guard) = match &lq.miss_emu {
        Some(emu) => {
            let (dup, guards, out, exc) = miss_guards(lq, table, cur_scan, &dims, &filter, emu);
            (dup, emu.init_sql.clone(), guards, out, exc)
        }
        None => (
            Some(guard_sql(lq, table, cur_scan, &dims, &filter)),
            lq.default_sql.clone(),
            Vec::new(),
            None,
            None,
        ),
    };
    Ok(LookupRender {
        cte: None,
        join: Some(join_sql(lq, table, &alias, &dims, &ons, &filter)),
        hit: format!("{alias}.sd_hit = 1"),
        value,
        default_sql,
        key_checks,
        tie_guard,
        guard_binds: lq.filter_binds.clone(),
        miss_guards,
        miss_out,
        exc_guard,
    })
}

/// miss 仿真守卫组 + 标量出口（期望求值均 = 0 / 有 miss 行才取回）：
///   - dup：维度组 >1 行（too_many_rows）整段退回；
///   - mixed：命中与落空并存（carryover 次序无法用初值刻画）退回；
///   - uniform：no_data_found 赋值逐行结果非均匀（"最后失败行胜出"
///     语义无法用任意行代表）退回。
fn miss_guards(
    lq: &LookupQuery,
    table: &Table,
    cur_scan: &str,
    dims: &[String],
    filter: &str,
    emu: &super::lookup::MissEmu,
) -> (
    Option<String>,
    Vec<super::entry::BoundGuard>,
    Option<MissOut>,
    Option<super::entry::BoundGuard>,
) {
    let scan = guard_scan(lq, table, cur_scan, dims, filter);
    let dup = format!("SELECT COUNT(*) {scan} WHERE COALESCE(_b.__cnt, 0) > 1");
    // 混合调度行号版守卫（so-exception-row-hybrid §3）：与 dup 同谓词，
    // 投影违规行 `__sd_rn`（0 基）。行号读自引擎物化的 `sd_cur` 临时表
    // （`__SD_HCUR__` 占位符运行期替换）——编号只在物化时赋一次，
    // 守卫/段批写/重放共用，引擎另做游标流序位置校验（P0-3b）。
    // 仅 miss_emu 路径产出：dup 行 = TOO_MANY_ROWS 异常行，交解释器
    // 单行重放复现 handler 副作用。
    let exc = format!(
        "SELECT _cur.__sd_rn {} WHERE COALESCE(_b.__cnt, 0) > 1",
        guard_scan(
            lq,
            table,
            &format!("SELECT * FROM {}", crate::ir::set_oriented::SD_HCUR_TABLE),
            dims,
            filter
        )
    );
    let mixed = format!(
        "SELECT CAST((COUNT(*) FILTER (WHERE _b.__cnt IS NULL) > 0 AND COUNT(*) FILTER (WHERE _b.__cnt IS NOT NULL) > 0) AS BIGINT) {scan}"
    );
    // 守卫/出口 SQL 内嵌 cur_scan，其 $v_ 参数与 tie_guard 同口径取
    // lq.filter_binds；处理器赋值的帧变量绑定在此之上去重合并。
    let mut guards = vec![(mixed, lq.filter_binds.clone())];
    let mut out = None;
    if !emu.assigns.is_empty() {
        let exprs = emu
            .assigns
            .iter()
            .map(|a| a.sql.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut binds: Vec<(String, BindSrc)> = lq.filter_binds.clone();
        for a in &emu.assigns {
            for b in &a.binds {
                if !binds.iter().any(|(p, _)| p == &b.0) {
                    binds.push(b.clone());
                }
            }
        }
        guards.push((
            format!(
                "SELECT CAST(COUNT(*) > 1 AS BIGINT) FROM (SELECT DISTINCT {exprs} {scan} WHERE _b.__cnt IS NULL) _u"
            ),
            binds.clone(),
        ));
        out = Some(MissOut {
            pc: emu.pc,
            targets: emu.assigns.iter().map(|a| a.var.clone()).collect(),
            sql: format!("SELECT {exprs} {scan} WHERE _b.__cnt IS NULL LIMIT 1"),
            binds,
        });
    }
    (Some(dup), guards, out, Some((exc, lq.filter_binds.clone())))
}

/// 守卫共用的 FROM..ON 尾段：`(cur_scan) _cur LEFT JOIN`（维度分组
/// 计数子查询）`_b ON 关联键`——与 guard_sql 同口径。
fn guard_scan(
    lq: &LookupQuery,
    table: &Table,
    cur_scan: &str,
    dims: &[String],
    filter: &str,
) -> String {
    let group_by = dims.join(", ");
    let g_ons = lq
        .correlations
        .iter()
        .map(|c| format!("_b.{} = _cur.{}", c.dim_column.sql(), c.cursor_field.sql()))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "FROM ({cur_scan}) _cur LEFT JOIN (SELECT {group_by}, COUNT(*) AS __cnt FROM {}{filter} GROUP BY {group_by}) _b ON {g_ons}",
        table.qualified
    )
}

/// 关联键证据：逐键 (维表键列类型, 游标字段) + JOIN/守卫共用的维度列。
struct KeyParts {
    dims: Vec<String>,
    ons: Vec<String>,
    key_checks: Vec<(String, Name)>,
}

fn key_parts(lq: &LookupQuery, table: &Table, alias: &str) -> Result<KeyParts, SkipReason> {
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
    Ok(KeyParts {
        dims,
        ons,
        key_checks,
    })
}

/// 主查询 1:1 LEFT JOIN：已过滤日期的视图投影子查询，无 MAX/HAVING/
/// 外层 __cnt=1 滤行。
fn join_sql(
    lq: &LookupQuery,
    table: &Table,
    alias: &str,
    dims: &[String],
    ons: &[String],
    filter: &str,
) -> String {
    format!(
        "LEFT JOIN (SELECT {}, {} AS sd_val, 1 AS sd_hit FROM {}{}) {} ON {}",
        dims.join(", "),
        lq.column.sql(),
        table.qualified,
        filter,
        alias,
        ons.join(" AND ")
    )
}

/// 入口守卫：子查询先做日期过滤、仅按等值关联键 GROUP BY 计数，
/// 外层 COALESCE(_b.__cnt,0)<>1，期望 COUNT(*)=0（非零即整段退回）。
fn guard_sql(
    lq: &LookupQuery,
    table: &Table,
    cur_scan: &str,
    dims: &[String],
    filter: &str,
) -> String {
    let group_by = dims.join(", ");
    let g_ons = lq
        .correlations
        .iter()
        .map(|c| format!("_b.{} = _cur.{}", c.dim_column.sql(), c.cursor_field.sql()))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "SELECT COUNT(*) FROM ({cur_scan}) _cur LEFT JOIN          (SELECT {group_by}, COUNT(*) AS __cnt FROM {}{filter} GROUP BY {group_by}) _b          ON {g_ons} WHERE COALESCE(_b.__cnt, 0) <> 1",
        table.qualified
    )
}

/// 零异常守卫型处理器结构面（§3.5 V3-5）：保护区恰 1 条 SELECT INTO，
/// 全部处理器体不得写 INTO 变量（异常路径保留跨行残留）、体内跳转仅
/// 允许指向共同续跑点，首个处理器紧随保护区。返回 (查询变量, 续跑 pc)；
/// 形态不符 = None。处理器内容在守卫证明恰 1 行后整体不可达，
/// 其余语句形态由 admit_effects/write_set 兜底裁决。
pub(super) fn carryover_group(
    prog: &IrProg,
    pc: usize,
    entries: &[TrapEntry],
) -> Option<(VarName, usize)> {
    let IrStmt::SqlQuery { into, fn_sites, .. } = prog.code.get(pc + 1)? else {
        return None;
    };
    let [AssignTarget::Var(v)] = into.as_slice() else {
        return None;
    };
    if !fn_sites.is_empty() || entries.is_empty() {
        return None;
    }
    if !matches!(prog.code.get(pc + 2), Some(IrStmt::PopTrap)) {
        return None;
    }
    let join = entries.iter().map(|e| e.body_end).max()? + 1;
    if entries.iter().map(|e| e.body_start).min() != Some(pc + 4)
        || !matches!(prog.code.get(pc + 3), Some(IrStmt::Jump(t)) if *t == join)
    {
        return None;
    }
    for e in entries {
        if e.body_start >= e.body_end
            || !matches!(prog.code.get(e.body_end), Some(IrStmt::Jump(t)) if *t == join)
        {
            return None;
        }
        for bpc in e.body_start..e.body_end {
            match prog.code.get(bpc)? {
                IrStmt::Nop => {}
                IrStmt::Assign {
                    target: AssignTarget::Var(av),
                    ..
                } if av != v => {}
                IrStmt::Jump(t)
                | IrStmt::JumpIf { to: t, .. }
                | IrStmt::JumpIfNot { to: t, .. }
                    if *t == join => {}
                _ => return None,
            }
        }
    }
    Some((v.clone(), join))
}

/// 单语句写出的标量变量（与 liveout::stmt_writes 同口径，含
/// CallRoutine OUT/INOUT 目标写回）。
fn stmt_var_writes(stmt: &IrStmt, out: &mut std::collections::BTreeSet<VarName>) {
    match stmt {
        IrStmt::Assign {
            target: AssignTarget::Var(v),
            ..
        } => {
            out.insert(v.clone());
        }
        IrStmt::Fetch { targets, .. } | IrStmt::SqlQuery { into: targets, .. } => {
            for t in targets {
                if let AssignTarget::Var(v) = t {
                    out.insert(v.clone());
                }
            }
        }
        IrStmt::CallRoutine { out_targets, .. } => {
            for t in out_targets.iter().flatten() {
                if let AssignTarget::Var(v) = t {
                    out.insert(v.clone());
                }
            }
        }
        _ => {}
    }
}

/// INTO 变量初始化常量证明：全程序写点恰为 {循环外 Const Assign,
/// 本查询 INTO（pc+1）}；初始化值渲染为 SQL 字面量。任一其它写点
/// 或区域内重赋值 = None（carryover 语义无法用单值刻画）。
fn into_init_sql(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    into_var: &VarName,
) -> Option<String> {
    let mut init = None;
    for (wpc, stmt) in prog.code.iter().enumerate() {
        let in_region = (region.open_pc..=region.close_pc).contains(&wpc);
        match stmt {
            IrStmt::Assign {
                target: AssignTarget::Var(v),
                expr,
            } if v == into_var => {
                if in_region {
                    return None;
                }
                let IrExpr::Const(c) = expr else {
                    return None;
                };
                if init.is_some() {
                    return None;
                }
                init = crate::sql::render::const_to_sql(c);
            }
            IrStmt::Fetch { targets, .. } => {
                if targets
                    .iter()
                    .any(|t| matches!(t, AssignTarget::Var(v) if v == into_var))
                {
                    return None;
                }
            }
            IrStmt::SqlQuery { into, .. } => {
                if into
                    .iter()
                    .any(|t| matches!(t, AssignTarget::Var(v) if v == into_var))
                    && wpc != pc + 1
                {
                    return None;
                }
            }
            IrStmt::CallRoutine { out_targets, .. }
                if out_targets
                    .iter()
                    .flatten()
                    .any(|t| matches!(t, AssignTarget::Var(v) if v == into_var)) =>
            {
                return None;
            }
            _ => {}
        }
    }
    init
}

/// miss 仿真证据构建（`LookupQuery::miss_emu`）：仅当 no_data_found
/// 处理器存在且全部副作用可批量仿真时返回 Some——
///   1. INTO 变量除本查询外全程序仅一次循环外 Const 初始化；
///   2. no_data_found 体内标量赋值可渲染为逐行 SQL，引用变量均为
///      区域外不变量（形参/前缀局部量），记录字段限本游标记录；
///   3. 赋值目标变量在续跑点（join）之后无任何写（否则逐行覆写
///      次序无法用单值回填刻画）。
///
/// 任一不满足 = None（保持恰 1 行守卫，miss/多行整段退回）。
fn miss_emulation(
    prog: &IrProg,
    pc: usize,
    entries: &[TrapEntry],
    region: &CursorRegion,
    into_var: &VarName,
    join: usize,
) -> Option<super::lookup::MissEmu> {
    let entry = entries
        .iter()
        .find(|e| e.conds.iter().any(|c| c == "no_data_found"))?;
    let init_sql = into_init_sql(prog, pc, region, into_var)?;
    // 区域内行变体变量集：处理器表达式引用即拒绝（逐行值无法绑定）。
    let mut row_vars = std::collections::BTreeSet::new();
    row_vars.insert(into_var.clone());
    for stmt in &prog.code[region.open_pc..=region.close_pc] {
        stmt_var_writes(stmt, &mut row_vars);
    }
    // 续跑点之后的写集：处理器目标变量被覆写则整组仿真放弃。
    // 例外处理器体内的写只在异常路径执行——批写守卫保证成功路径不触发
    // （否则区域整体已回退），不计入 tail。
    let mut trap_pcs = std::collections::BTreeSet::new();
    if join <= region.close_pc {
        for stmt in &prog.code[join..=region.close_pc] {
            if let IrStmt::PushTrap { entries } = stmt {
                for e in entries {
                    trap_pcs.extend(e.body_start..e.body_end);
                }
            }
        }
    }
    let mut tail_vars = std::collections::BTreeSet::new();
    let mut tail_reads = std::collections::BTreeSet::new();
    if join <= region.close_pc {
        for (i, stmt) in prog.code[join..=region.close_pc].iter().enumerate() {
            if !trap_pcs.contains(&(join + i)) {
                stmt_var_writes(stmt, &mut tail_vars);
                region::collect_stmt_reads(stmt, &mut tail_reads);
            }
        }
    }
    let mut assigns = Vec::new();
    for stmt in &prog.code[entry.body_start..entry.body_end] {
        // 处理器体仅允许 Nop 与标量变量赋值（与 writebuf_policy 的
        // handler_stmt_skip_equivalent 同口径）：CallRoutine/SqlExec/
        // 记录字段写/控制流等副作用无法批量仿真，一律拒绝。
        let IrStmt::Assign {
            target: AssignTarget::Var(t),
            expr,
        } = stmt
        else {
            if !matches!(stmt, IrStmt::Nop) {
                return None;
            }
            continue;
        };
        // 目标变量被续跑段写（次序不可回填）或被续跑段读取（批写已
        // 按处理器前值求值，回填太晚）均拒绝。
        if tail_vars.contains(t) || tail_reads.contains(t) {
            return None;
        }
        let (text, ebinds) = crate::sql::render::expr_to_scalar_sql_binds(expr)?;
        let mut repls = Vec::with_capacity(ebinds.len());
        let mut kept = Vec::with_capacity(ebinds.len());
        for (ph, src) in ebinds {
            match src {
                BindSrc::Field { rec, field } if rec == region.record => {
                    repls.push((ph, format!("_cur.{}", Name(field).sql())));
                }
                BindSrc::Var(v) => {
                    if v.0.starts_with("__") || row_vars.contains(&v) {
                        return None;
                    }
                    kept.push((ph, BindSrc::Var(v)));
                }
                _ => return None,
            }
        }
        repls.sort_by_key(|(ph, _)| std::cmp::Reverse(ph.len()));
        let sql = super::substitute_binds(&text, &repls);
        assigns.push(super::lookup::MissAssign {
            var: t.clone(),
            sql,
            binds: kept,
        });
    }
    Some(super::lookup::MissEmu {
        init_sql,
        assigns,
        pc,
    })
}

/// 识别 pc 处的零异常守卫查找（§3.5 V3-5）：保护区恰 1 条 SELECT INTO，
/// 处理器不写 INTO 变量（异常路径保留跨行残留值），续跑落点在处理器块
/// 之后。解析复用 parse_total（兜底常量取 Null——守卫保证恰 1 行命中，
/// 异常分支运行期不可达）。形态不符 = Ok(None)；全定义证明失败 =
/// Err(UnprovenQueryTotalness)；视图/表与区域写集相交 = Err(TargetReadDependency)。
pub(super) fn carryover_group_at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<Option<(LookupQuery, usize)>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let Some((var, resume)) = carryover_group(prog, pc, entries) else {
        return Ok(None);
    };
    let IrStmt::SqlQuery { sql, binds, .. } = &prog.code[pc + 1] else {
        return Ok(None);
    };
    // 查询不可全定义解析时回落 None 交后续识别器（形状放宽后命中面变大，
    // 解析失败不得短路分派——与识别顺序的"首个匹配生效"约定一致）。
    let Ok(mut query) = parse_total(sql, var, "NULL".to_string(), binds, &region.record) else {
        return Ok(None);
    };
    if query.correlations.iter().any(|c| c.op != CorrelationOp::Eq) {
        return Err(SkipReason::UnprovenQueryTotalness);
    }
    query.entry_guard = true;
    // NO_DATA_FOUND miss 仿真：证据齐备时放宽守卫至"全命中或全 miss"，
    // miss 行的 carryover 初值与处理器赋值由渲染/标量出口批量等价。
    let into_var = query.var.clone();
    query.miss_emu = miss_emulation(prog, pc, entries, region, &into_var, resume);
    let writes = region::write_set(prog, region, catalog)?;
    if writes.contains(&query.table.0)
        || query.in_filters.iter().any(|f| writes.contains(&f.table.0))
    {
        return Err(SkipReason::TargetReadDependency);
    }
    Ok(Some((query, resume)))
}
