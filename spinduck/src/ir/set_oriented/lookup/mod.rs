//! 兜底型维表查找（§4.4 B1）：PushTrap(WHEN OTHERS → 常量) 保护的
//! SELECT INTO 组，关系化为全定义渲染——循环不变量 = 聚合标量子查询，
//! 相关键等值 = GROUP BY 派生表 LEFT JOIN，有序 Top-1 = 按组首行派生表。
//! 恰 1 行命中判定永不抛 NO_DATA_FOUND/TOO_MANY_ROWS，命中 NULL 保留
//! （不得 COALESCE）；同组任一查询抛错 → 全部变量落兜底（异常优先级）。

mod aggregate;
mod lateral;
mod mixed;
mod parse;
mod top1;
mod tuple;
pub(super) use mixed::mixed_fallback_group_at;
pub(crate) use parse::parse_total;
pub(crate) use parse::parse_tuple_total;
pub(super) use tuple::render_tuple;

use super::region::{self, CursorRegion};
use super::shape::Name;
use super::{BindSrc, SkipReason, VarName};
use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt};
use crate::value::SpinValue;

/// 维表列与游标字段的关联比较。
pub(super) struct Correlation {
    pub dim_column: Name,
    pub cursor_field: Name,
    pub op: CorrelationOp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CorrelationOp {
    Eq,
    Le,
    Ge,
}

impl CorrelationOp {
    fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Le => "<=",
            Self::Ge => ">=",
        }
    }
}

/// 有序 Top-1：ORDER BY 多列 + LIMIT 1（编译期 ROWNUM=1 改写产物）。
/// 并列一致性按完整排序键判定（首列并列由次列打破不算歧义）。
pub(super) struct Top1 {
    /// (排序列, 是否降序)，按 ORDER BY 出现顺序。
    pub order_by: Vec<(Name, bool)>,
}

/// 异构 CASE 写汇（§4.4 B1）：`SELECT CASE operand WHEN lit THEN r.field
/// ... ELSE r.field END INTO var FROM sys_dummy`。operand 为同组查找变量，
/// 分支取游标字段；作为兜底组成员由 copy 以 group_hits 门控组装计算 CASE。
#[derive(Clone)]
pub(super) struct CaseSink {
    /// 操作数查找变量（同组成员，命中值含 NULL→ELSE）。
    pub operand: VarName,
    /// (WHEN 字面量 SQL, 游标字段)，按出现顺序。
    pub whens: Vec<(String, Name)>,
    pub else_value: CaseElse,
}

/// CASE ELSE 分支：游标字段或数值字面量（`ELSE 0` 是业务分支，非异常兜底）。
#[derive(Clone)]
pub(super) enum CaseElse {
    Field(Name),
    Lit(String),
}

/// IN 子查询过滤：单表投影；读表纳入传递读写不交与入口漂移复核。
/// 内层谓词可为静态字面量、形参 BETWEEN（绑定并入 filter_binds）或
/// 相关键等值（correlations，引用游标字段 → 须 LATERAL 渲染）。
pub(super) struct InFilter {
    pub column: Name,
    pub table: Name,
    pub inner_column: Name,
    /// 渲染后的内层静态 WHERE（空串 = 无过滤；含 BETWEEN 的 $v_ 占位）。
    pub filter: String,
    /// 内层相关键等值（维表列 = 游标字段）；空 = 静态半连接。
    pub correlations: Vec<Correlation>,
}

/// 游标字段 IN-list（§4.4 B1 相关 Top-1）：`col IN (项, ...)`，项为游标
/// 字段绑定或字面量。逐行相关 → 独立窗口 CTE 无法引用 sd_cur，须 LATERAL
/// 渲染（深嵌套相关标量子查询会触发内核规划栈溢出，LATERAL 连接栈深恒定）。
pub(super) struct InList {
    pub column: Name,
    pub items: Vec<InListItem>,
}

/// IN-list 项：游标字段（渲染为 sd_cur.字段）、字面量（原样），或
/// concat/substring 字符串表达式（IR 规范形，已代入 sd_cur.字段；
/// base_field 供类型对齐）。
pub(super) enum InListItem {
    Field(Name),
    Lit(String),
    Expr { sql: String, base_field: Name },
}

/// 已识别的兜底查找：变量、单表投影、白名单过滤、兜底常量。
pub(super) struct LookupQuery {
    pub var: VarName,
    pub table: Name,
    pub column: Name,
    /// 投影 `nvl(col, 常量)` 的兜底常量 SQL（源 SQL 显式 coalesce）：
    /// 命中 NULL → 取常量；落空仍按 hit 判定（跳行/兜底），二者不同。
    pub nvl_default: Option<String>,
    /// 渲染后的静态 WHERE 谓词（空串 = 无过滤；相关键与 IN 子查询不在其中）。
    pub filter: String,
    pub default_sql: String,
    /// 相关键等值（可多键，如 sd_partner 的 partner/sub_partner/market）；
    /// 空 = 循环不变量。GROUP BY/JOIN 按全部键派生。
    pub correlations: Vec<Correlation>,
    pub in_filters: Vec<InFilter>,
    /// 游标字段 IN-list（相关 Top-1 专用，LATERAL 渲染）。
    pub in_lists: Vec<InList>,
    pub top1: Option<Top1>,
    /// 异构 CASE 写汇（Some 时 table 为 sys_dummy 哑表，column 无意义）。
    pub case_sink: Option<CaseSink>,
    /// 同一 WHEN OTHERS 闭包组编号（analyze_region 赋值）；
    /// 投影命中条件须合取同组全部成员的 hit。
    pub group: usize,
    /// 跳行 trap 查找（§5 步骤 4）：落空抛错 → 整行过滤（区别于常量兜底
    /// 保留行）。仅 BatchAtomic × trap 授权；投影直接取命中值，批写 WHERE
    /// 合取 hit 过滤落空行。
    pub filter_on_miss: bool,
    /// 静态过滤引用的过程形参绑定（如 BETWEEN 区间的 p_i_date）：须并入
    /// 批写 SQL 的 binds 在运行期解析（相关键 Field 绑定不在此列）。
    pub filter_binds: Vec<(String, BindSrc)>,
    /// §3.5 零异常入口守卫（V3-5）：处理器不改写 INTO 变量（跨行残留），
    /// 命中基数由入口守卫在运行期裁决——0/多行整段退回原解释器。
    pub entry_guard: bool,
}

/// 多 OUT 查找函数（§3.2 LookupTuple）的调用方绑定：单条
/// `SELECT 多列 INTO 局部变量` 经直线拷贝写齐全部 OUT 形参与返回值，
/// WHEN OTHERS 处理器给每个 OUT 与返回值赋全定义常量且不 Raise。
/// 渲染为一条 GROUP BY 派生表（COUNT(*) AS sd_cnt），每个调用方变量
/// 投影 `CASE WHEN sd_cnt = 1 THEN <列> ELSE <兜底> END`。
pub(super) struct TupleQuery {
    pub table: Name,
    /// SELECT 输出列（与 INTO 目标位置对齐）。
    pub columns: Vec<Name>,
    /// 渲染后的静态 WHERE（空串 = 无过滤；相关键不在其中）。
    pub filter: String,
    /// 相关键等值（可多键）；空 = 循环不变量（保守拒绝，不渲染）。
    pub correlations: Vec<Correlation>,
    /// 静态过滤引用的调用方变量绑定（帧单值，运行期解析）。
    pub filter_binds: Vec<(String, BindSrc)>,
    /// 调用方变量绑定（OUT 实参槽位 + 返回值赋值目标）。
    pub slots: Vec<TupleSlot>,
}

/// 单个调用方变量的取值来源与兜底。
pub(super) struct TupleSlot {
    pub var: VarName,
    pub source: TupleSource,
    /// 处理器兜底常量（SpinValue::Str/Int/Null 可渲染）。
    pub default: SpinValue,
}

/// 槽位取值来源：查询列（按 `columns` 下标）或全路径常量。
pub(super) enum TupleSource {
    Col(usize),
    Const(SpinValue),
}

/// 渲染产物：单条派生表 + 共享命中条件；每槽位一条值表达式。
#[derive(Clone)]
pub(super) struct ResolvedTuple {
    pub cte: String,
    pub join: String,
    /// “恰 1 行命中”布尔表达式（`sd_lkN.sd_cnt = 1`，无匹配/多匹配均 false）。
    pub hit: String,
    pub slots: Vec<ResolvedTupleSlot>,
    /// 逐键 (维表键列类型, 游标字段)，与源列严格一致才放行。
    pub key_checks: Vec<(String, Name)>,
}

/// 已解析槽位：命中值表达式 + 兜底 SQL + 来源列类型证据。
#[derive(Clone)]
pub(super) struct ResolvedTupleSlot {
    pub var: VarName,
    /// 命中时的值表达式（`sd_lkN.sd_vI` 或常量字面量）。
    pub value: String,
    pub default_sql: String,
    /// 来源列类型（常量槽位为 None，不做列类型对齐）。
    pub kind: Option<String>,
    pub nullable: bool,
}

/// 渲染产物：命中条件 + 命中值 + 兜底常量；最终 CASE 由 copy 组装
/// （同组多查找时合取全部 hit，保证异常优先级不穿透）。
#[derive(Clone)]
pub(super) struct LookupRender {
    pub cte: Option<String>,
    pub join: Option<String>,
    /// “恰 1 行命中”布尔表达式（无匹配为 false，NULL 安全）。
    pub hit: String,
    /// 命中时的值表达式（保留 NULL）。
    pub value: String,
    pub default_sql: String,
    /// 相关形态：逐键 (维表键列类型, 游标字段)，与源列严格一致才放行。
    pub key_checks: Vec<(String, Name)>,
    /// Top-1 入口并列一致性守卫 SQL，期望 COUNT(*) = 0。
    pub tie_guard: Option<String>,
    /// 并列守卫须随行的形参绑定（LATERAL 静态过滤含 BETWEEN 等）；
    /// 入口守卫从运行帧解析这些绑定后求值（其余渲染器无守卫绑定）。
    pub guard_binds: Vec<(String, BindSrc)>,
}

/// 结构解析 + 模式识别后的已解析查找（generate 期填充 schema 证据）。
#[derive(Clone)]
pub(super) struct ResolvedLookup {
    pub var: VarName,
    pub kind: String,
    pub nullable: bool,
    pub render: LookupRender,
    pub group: usize,
    pub filter_on_miss: bool,
    /// 异构 CASE 写汇（copy 据此组装计算 CASE，绕过常规 kind 对齐）。
    pub case_sink: Option<CaseSink>,
    /// 渲染文本引用的其他查找变量（跨查找替换记录）：批写注册其派生表/
    /// 连接须先于本查找（LATERAL 引用其别名），按依赖拓扑递归登记。
    pub deps: Vec<VarName>,
    /// §3.5 入口守卫视图的最终底表名（写集相交复核用；普通查找为 None）。
    pub guard_base: Option<Name>,
}

/// 识别 pc 处的兜底查找组（同一 WHEN OTHERS 内 n 条连续 SELECT INTO）：
/// 形态不符 = Ok(None)；形态符合但全定义证明失败 = Err(UnprovenQueryTotalness)。
pub(super) fn fallback_group_at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<Option<Vec<LookupQuery>>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let [entry] = entries.as_slice() else {
        return Ok(None);
    };
    let Some(shape) = region::fallback_group(prog, pc, entry) else {
        return Ok(None);
    };
    let writes = region::write_set(prog, region, catalog)?;
    let mut out = Vec::new();
    for (i, (var, default)) in shape.into_iter().enumerate() {
        let IrStmt::SqlQuery { sql, binds, .. } = &prog.code[pc + 1 + i] else {
            return Ok(None);
        };
        let query = parse::parse_total(sql, var, default, binds, &region.record)?;
        if writes.contains(&query.table.0)
            || query.in_filters.iter().any(|f| writes.contains(&f.table.0))
        {
            return Err(SkipReason::TargetReadDependency);
        }
        out.push(query);
    }
    Ok(Some(out))
}

/// 识别 pc 处的 IF 路由型常量兜底闭包（§4.3 + §4.4 B1）：主线首条 IF 由
/// 循环不变量路由（THEN 入口守卫证明不可达），ELSE 为连续 SELECT INTO 组，
/// 处理器重置成员子集（全量 m=n 或部分 m<n）。部分兜底的未重置成员须组内
/// 局部死（仅被同组 CASE 写汇作操作数读取，INSERT/出口/他处均无读取），
/// 其批写值经 group_hits 合取门控，落空时输出取重置兜底，残值不可观察。
/// 返回 (IF 条件, ELSE 查找组, 续跑 pc)；形态不符 = Ok(None)；全定义证明
/// 失败 = Err(UnprovenQueryTotalness)；局部死证明失败 = Err(UnsupportedControlFlow)。
pub(super) fn fallback_if_group_at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<Option<(IrExpr, Vec<LookupQuery>, usize)>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let [entry] = entries.as_slice() else {
        return Ok(None);
    };
    let Some(iff) = region::fallback_if_group(prog, pc, entry) else {
        return Ok(None);
    };
    let writes = region::write_set(prog, region, catalog)?;
    let mut out = Vec::new();
    for (i, var) in iff.query_vars.iter().enumerate() {
        let IrStmt::SqlQuery { sql, binds, .. } = &prog.code[iff.else_start + i] else {
            return Ok(None);
        };
        let default = iff
            .resets
            .iter()
            .find(|(rv, _)| rv == var)
            .map(|(_, d)| d.clone())
            .unwrap_or(SpinValue::Null);
        let query = parse::parse_total(sql, var.clone(), default, binds, &region.record)?;
        if writes.contains(&query.table.0)
            || query.in_filters.iter().any(|f| writes.contains(&f.table.0))
        {
            return Err(SkipReason::TargetReadDependency);
        }
        out.push(query);
    }
    // THEN 分支（pc+2..else_start）由入口路由守卫证明不可达，其读取不参与
    // 集合化计划，从组内局部死证明中豁免；ELSE 同组 CASE 写汇操作数读取仍允许。
    let then_pcs: Vec<usize> = (pc + 2..iff.else_start).collect();
    for q in &out {
        if iff.resets.iter().any(|(rv, _)| rv == &q.var) {
            continue;
        }
        let mut allowed: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, m)| m.case_sink.as_ref().is_some_and(|cs| cs.operand == q.var))
            .map(|(j, _)| iff.else_start + j)
            .collect();
        allowed.extend(then_pcs.iter().copied());
        if !super::dead::read_only_within(prog, &q.var, &allowed) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
    }
    Ok(Some((iff.cond, out, iff.resume_pc)))
}

/// 识别 pc 处的跳行 trap 查找（§5 步骤 4）：PushTrap(WHEN OTHERS → 跳行)
/// 紧随单条相关 SELECT INTO。处理器体须为跳过型（Nop / NoOp 调用 +
/// Jump 回循环头），落空抛错 → 整行过滤。形态不符 = Ok(None)；
/// 全定义证明失败 = Err(UnprovenQueryTotalness)。仅相关查找（不变量
/// 循环查询另由入口求值子系统处理）。
pub(super) fn skip_lookup_at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<Option<LookupQuery>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let [entry] = entries.as_slice() else {
        return Ok(None);
    };
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return Ok(None);
    }
    for bpc in entry.body_start..entry.body_end {
        match prog.code.get(bpc) {
            Some(IrStmt::Nop) => {}
            Some(IrStmt::Jump(t)) if *t == region.fetch_pc => {}
            Some(IrStmt::CallRoutine {
                name,
                args,
                out_targets,
            }) if super::catalog_summary::call_is_noop(catalog, prog, name, args, out_targets) => {}
            _ => return Ok(None),
        }
    }
    let Some(IrStmt::SqlQuery {
        sql,
        binds,
        into,
        fn_sites,
    }) = prog.code.get(pc + 1)
    else {
        return Ok(None);
    };
    let [AssignTarget::Var(var)] = into.as_slice() else {
        return Ok(None);
    };
    if !fn_sites.is_empty() {
        return Ok(None);
    }
    let mut query = parse::parse_total(sql, var.clone(), SpinValue::Null, binds, &region.record)?;
    if query.correlations.is_empty() {
        return Ok(None);
    }
    let writes = region::write_set(prog, region, catalog)?;
    if writes.contains(&query.table.0)
        || query.in_filters.iter().any(|f| writes.contains(&f.table.0))
    {
        return Err(SkipReason::TargetReadDependency);
    }
    query.filter_on_miss = true;
    Ok(Some(query))
}
/// 相关 = GROUP BY 派生表（去重防行倍增）+ LEFT JOIN，
/// sd_cnt 为 NULL（无匹配）时 hit 为 false，命中 NULL 仍保留。
pub(super) fn render(
    lq: &LookupQuery,
    table: &super::schema::Table,
    inner_tables: &[(Name, super::schema::Table)],
    index: usize,
    cur_scan: &str,
) -> Result<LookupRender, SkipReason> {
    if lq.entry_guard {
        return super::vguard::render(lq, table, index, cur_scan);
    }
    let filter = combined_filter(lq, inner_tables)?;
    if lq.case_sink.is_some() {
        return Ok(aggregate::case_gate(lq, table));
    }
    if !lq.in_lists.is_empty() && lq.top1.is_none() {
        // 游标字段 IN-list 仅支持有序 Top-1（LATERAL 逐行）；其余形态保守拒绝。
        return Err(SkipReason::UnprovenQueryTotalness);
    }
    let has_range = lq.correlations.iter().any(|c| c.op != CorrelationOp::Eq);
    if lq.top1.is_some() {
        if has_range {
            return Err(SkipReason::UnprovenQueryTotalness);
        }
        if !lq.in_lists.is_empty() {
            return top1::render_top1_lateral(lq, table, cur_scan, index);
        }
        return top1::render_top1(lq, table, &filter, index);
    }
    if lq.correlations.is_empty() {
        return Ok(aggregate::scalar(lq, table, &filter));
    }
    if has_range || lq.in_filters.iter().any(|f| !f.correlations.is_empty()) {
        // IN 子查询本身相关（引用游标字段）：独立 CTE 无法引用 sd_cur，
        // 改逐行 LATERAL 聚合（保留 EXISTS 半连接语义与 COUNT 计数）。
        return lateral::render_correlated_lateral(lq, table, inner_tables, index);
    }
    aggregate::grouped(lq, table, &filter, index)
}

/// 静态谓词 + IN 子查询谓词合并为完整 WHERE 串。
fn combined_filter(
    lq: &LookupQuery,
    inner_tables: &[(Name, super::schema::Table)],
) -> Result<String, SkipReason> {
    let mut in_preds = Vec::new();
    for f in &lq.in_filters {
        let Some((_, inner)) = inner_tables.iter().find(|(n, _)| *n == f.table) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        in_preds.push(format!(
            "{} IN (SELECT {} FROM {}{})",
            f.column.sql(),
            f.inner_column.sql(),
            inner.qualified,
            f.filter
        ));
    }
    Ok(match (lq.filter.is_empty(), in_preds.is_empty()) {
        (true, true) => String::new(),
        (false, true) => lq.filter.clone(),
        (true, false) => format!(" WHERE {}", in_preds.join(" AND ")),
        (false, false) => format!("{} AND {}", lq.filter, in_preds.join(" AND ")),
    })
}

/// 函数内联 Lookup（§4.3 A7）→ 全定义文法解析：实参须为游标记录字段
/// （相关键），兜底须为常量；其余形态（不变量绑定/非常量兜底）保守
/// 拒绝，交回解释器。复用 [`parse::parse_total`] 的相关文法（含多键）。
pub(super) fn parse_inline(
    sql: &str,
    args: &[(String, crate::ir::IrExpr)],
    default: &crate::ir::IrExpr,
    record: &VarName,
) -> Result<LookupQuery, SkipReason> {
    use crate::ir::IrExpr;
    let bad = SkipReason::UnprovenQueryTotalness;
    let IrExpr::Const(default_sv) = default else {
        return Err(bad);
    };
    let mut binds = Vec::with_capacity(args.len());
    for (ph, a) in args {
        match a {
            IrExpr::Field { rec, field } if rec == record => binds.push((
                ph.clone(),
                BindSrc::Field {
                    rec: rec.clone(),
                    field: field.clone(),
                },
            )),
            _ => return Err(bad),
        }
    }
    parse::parse_total(
        sql,
        VarName("__sd_inline".to_string()),
        default_sv.clone(),
        &binds,
        record,
    )
}
