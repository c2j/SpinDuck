//! V3-4 条件无 GROUP BY 聚合查表（设计 §3.4）：trap 包裹的
//! `SELECT coalesce(sum(coalesce(t.col, d0)), d1) INTO v`，
//! WHERE 拆为等值关联键（→LEFT JOIN）、静态过滤（→CTE WHERE）、
//! 游标字段门（→内层 CASE）；字段覆写 `r.f := v`（可选 IF 门）
//! 投影为外层 `CASE WHEN <IF> THEN (CASE WHEN <门> THEN COALESCE(_jf.v,d1) ELSE d1 END) ELSE sd_cur.f END`。
//! 关联列/聚合表达式/decode 不可证全定义 → UnprovenQueryTotalness。

use super::region::{self, CursorRegion};
use super::shape::Name;
use super::{BindSrc, IrExpr, IrProg, IrStmt, SkipReason, VarName};
use crate::catalog::RoutineCatalog;
use crate::ir::CursorSource;
use crate::value::SpinValue;

mod parse;
pub(super) use parse::parse_agg;

/// 关联键的游标侧：裸字段或 CASE 模板（解析期已替换为 `sd_cur.`）。
#[derive(Clone)]
pub(super) enum AggCursor {
    Field(Name),
    /// `CASE sd_cur.f WHEN 'a' THEN 'x' ... END`（占位符已替换）。
    Case(String),
}

/// 单个等值关联键：维表列 ↔ 游标侧表达式。
pub(super) struct AggKey {
    pub dim: Name,
    pub cursor: AggCursor,
}

/// 识别产物：函数体 SqlQuery 部件 + 门谓词 + 兜底。
pub(super) struct AggQuery {
    /// INTO 目标变量（聚合结果槽位）。
    pub var: VarName,
    pub table: Name,
    /// FROM 别名（列引用限定符）。
    pub alias: String,
    pub keys: Vec<AggKey>,
    /// 聚合源列（`t.jyfy` → jyfy）。
    pub value_col: Name,
    /// 内层 coalesce 默认（SUM 入参 NULL 兜底）。
    pub inner_default: String,
    /// 外层 coalesce 默认（空集/未命中兜底）。
    pub outer_default: String,
    /// 静态过滤 WHERE 片段（可为空，不含前导 WHERE）。
    pub static_filter: String,
    /// 静态过滤占位符绑定（帧单值，运行期解析）。
    pub static_binds: Vec<(String, BindSrc)>,
    /// 内层门谓词（已渲染 `sd_cur.` 前缀）。
    pub gate_preds: Vec<String>,
    /// 引用到的游标字段（供投影期 schema 校验）。
    pub cursor_fields: Vec<Name>,
    /// 处理器兜底常量（全定义已证；异常路径集合化下不可达，仅存证）。
    pub handler_default: SpinValue,
}

/// 字段覆写：`r.<field> := <agg var>`，可挂 IF 门条件。
pub(super) struct FieldWrite {
    pub field: Name,
    pub var: VarName,
    /// IF 门（None = 无条件覆写）。
    pub gate: Option<IrExpr>,
    /// 写入 pc（供前读证明）。
    pub pc: usize,
}

/// 渲染产物：单条 GROUP BY 派生表 + LEFT JOIN + 命中值表达式。
#[derive(Clone)]
pub(super) struct ResolvedAgg {
    /// INTO 槽位变量（与 FieldWrite.var 对应）。
    pub var: VarName,
    pub cte: String,
    pub join: String,
    /// 命中值：`COALESCE(sd_agN.sd_v0, <外层默认>)`。
    pub value: String,
    /// 外层默认 SQL（门假/未命中共用）。
    pub outer_default: String,
    /// 内层门谓词（已渲染 `sd_cur.` 前缀，AND 连接；空 = 无门）。
    pub gates: String,
    /// 逐键 (维表键列类型, 游标字段名)；CASE 键无单一字段不入列。
    pub key_checks: Vec<(String, Name)>,
    /// 聚合源列类型证据。
    pub value_kind: Option<String>,
    pub value_nullable: bool,
    /// 全部引用游标字段（关联键 + 门谓词），投影期存在性校验。
    pub cursor_fields: Vec<Name>,
}

/// pc 处识别 trap 聚合：形态 = fallback_group 同构骨架
/// （SqlQuery → PopTrap → Jump；处理器 = 常量赋值 + Jump），
/// 但 SELECT 项必须是可证全定义的聚合表达式。
/// 形态不符 = Ok(None)；全定义证明失败 = Err(UnprovenQueryTotalness)。
pub(super) fn at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &RoutineCatalog,
) -> Result<Option<(AggQuery, usize)>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let [entry] = entries.as_slice() else {
        return Ok(None);
    };
    // 骨架复用 fallback_group：n=1 查询 + 处理器单常量赋值 + 双 Jump。
    let Some(shape) = region::fallback_group(prog, pc, entry) else {
        return Ok(None);
    };
    let [(var, handler_default)] = shape.as_slice() else {
        return Ok(None);
    };
    let IrStmt::SqlQuery {
        sql,
        binds,
        fn_sites,
        ..
    } = &prog.code[pc + 1]
    else {
        return Ok(None);
    };
    if !fn_sites.is_empty() {
        return Ok(None);
    }
    let Some(query) = parse_agg(sql, binds, &region.record, var, handler_default)? else {
        return Ok(None);
    };
    // 异常兜底与聚合空集默认必须同值：集合化无法区分异常/空集路径，
    // 未命中统一取外层默认，两者不等即语义漂移。
    if crate::sql::render::const_to_sql(&query.handler_default).as_deref()
        != Some(query.outer_default.as_str())
    {
        return Err(SkipReason::UnprovenQueryTotalness);
    }
    let writes = region::write_set(prog, region, catalog)?;
    if writes.contains(&query.table.0) {
        return Err(SkipReason::TargetReadDependency);
    }
    // 静态过滤绑定必须循环不变（帧单值）。
    for (_, src) in &query.static_binds {
        if let BindSrc::Var(v) = src {
            if !region::var_invariant_in_region(prog, region, v) {
                return Err(SkipReason::UnprovenQueryTotalness);
            }
        }
    }
    Ok(Some((query, entry.body_end + 1)))
}

/// IF 门条件可渲染性：仅允许 Field/Const 叶子与 And/Or/比较/Not。
pub(super) fn gate_renderable(cond: &IrExpr) -> bool {
    match cond {
        IrExpr::Const(_) | IrExpr::Field { .. } => true,
        IrExpr::Not(e) => gate_renderable(e),
        IrExpr::Bin { op, l, r } => {
            matches!(
                op,
                super::BinOp::And
                    | super::BinOp::Or
                    | super::BinOp::Eq
                    | super::BinOp::Ne
                    | super::BinOp::Lt
                    | super::BinOp::Le
                    | super::BinOp::Gt
                    | super::BinOp::Ge
            ) && gate_renderable(l)
                && gate_renderable(r)
        }
        _ => false,
    }
}

/// 门条件 → SQL（`sd_cur.` 前缀；Field 必须是本记录游标列）。
pub(super) fn gate_sql(
    cond: &IrExpr,
    record: &VarName,
    select: &super::shape::Select,
    source: &super::schema::Table,
) -> Result<String, SkipReason> {
    match cond {
        IrExpr::Const(c) => {
            crate::sql::render::const_to_sql(c).ok_or(SkipReason::UnprovenExpression)
        }
        IrExpr::Field { rec, field } => {
            if rec != record {
                return Err(SkipReason::UnprovenExpression);
            }
            let name = Name(field.clone());
            super::copy::source_column(select, source, &name)?;
            Ok(format!("sd_cur.{}", name.sql()))
        }
        IrExpr::Not(e) => Ok(format!("NOT ({})", gate_sql(e, record, select, source)?)),
        IrExpr::Bin { op, l, r } => {
            let (ls, rs) = (
                gate_sql(l, record, select, source)?,
                gate_sql(r, record, select, source)?,
            );
            let op_sql = match op {
                super::BinOp::And => "AND",
                super::BinOp::Or => "OR",
                super::BinOp::Eq => "=",
                super::BinOp::Ne => "<>",
                super::BinOp::Lt => "<",
                super::BinOp::Le => "<=",
                super::BinOp::Gt => ">",
                super::BinOp::Ge => ">=",
                _ => return Err(SkipReason::UnprovenExpression),
            };
            Ok(format!("({ls} {op_sql} {rs})"))
        }
        _ => Err(SkipReason::UnprovenExpression),
    }
}

/// AggQuery + 维表 schema → 渲染产物（CTE/LEFT JOIN/命中值/门谓词）。
pub(super) fn render(
    aq: &AggQuery,
    table: &super::schema::Table,
    index: usize,
) -> Result<ResolvedAgg, SkipReason> {
    let alias = format!("sd_ag{index}");
    let mut join_ons = Vec::new();
    let mut checks = Vec::new();
    let mut key_cols = Vec::new();
    for (i, k) in aq.keys.iter().enumerate() {
        let Some(key_col) = table.columns.iter().find(|c| c.name == k.dim) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        if let AggCursor::Field(f) = &k.cursor {
            checks.push((key_col.kind.clone(), f.clone()));
        }
        key_cols.push(format!("{}.{} AS sd_k{i}", aq.alias, k.dim.sql()));
        let rhs = match &k.cursor {
            AggCursor::Field(f) => format!("sd_cur.{}", f.sql()),
            AggCursor::Case(tpl) => tpl.clone(),
        };
        join_ons.push(format!("{alias}.sd_k{i} = {rhs}"));
    }
    let value_col = table
        .columns
        .iter()
        .find(|c| c.name == aq.value_col)
        .ok_or(SkipReason::UnknownObjectDependency)?;
    let static_where = if aq.static_filter.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", aq.static_filter)
    };
    let group_by = aq
        .keys
        .iter()
        .map(|k| format!("{}.{}", aq.alias, k.dim.sql()))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(ResolvedAgg {
        var: aq.var.clone(),
        cte: format!(
            "{alias} AS (SELECT {}, COALESCE(SUM(COALESCE({}.{}, {})), {}) AS sd_v0 \
             FROM {}{}{} GROUP BY {})",
            key_cols.join(", "),
            aq.alias,
            aq.value_col.sql(),
            aq.inner_default,
            aq.outer_default,
            table.qualified,
            alias_suffix(&aq.alias),
            static_where,
            group_by
        ),
        join: format!("LEFT JOIN {alias} ON {}", join_ons.join(" AND ")),
        value: format!("COALESCE({alias}.sd_v0, {})", aq.outer_default),
        outer_default: aq.outer_default.clone(),
        gates: aq.gate_preds.join(" AND "),
        key_checks: checks,
        value_kind: Some(value_col.kind.clone()),
        value_nullable: value_col.nullable,
        cursor_fields: aq.cursor_fields.clone(),
    })
}

fn alias_suffix(alias: &str) -> String {
    if alias.is_empty() {
        String::new()
    } else {
        format!(" AS {alias}")
    }
}

/// 写入 pc 之前区域内是否已读取 (rec, field)：投影 CASE 全局生效，
/// 前读会拿到覆写后值而非原值，必须拒绝。
pub(super) fn field_read_before(
    prog: &IrProg,
    region: &CursorRegion,
    rec: &VarName,
    field: &str,
    before_pc: usize,
) -> bool {
    prog.code[region.open_pc..before_pc]
        .iter()
        .any(|stmt| stmt_field_read(stmt, rec, field))
}

fn stmt_field_read(stmt: &IrStmt, rec: &VarName, field: &str) -> bool {
    match stmt {
        IrStmt::Assign { expr, .. }
        | IrStmt::Perform { expr }
        | IrStmt::FoldedQuery { expr, .. } => expr_field_read(expr, rec, field),
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => {
            expr_field_read(cond, rec, field)
        }
        IrStmt::Raise { message } | IrStmt::Return { value: message } => message
            .as_ref()
            .is_some_and(|e| expr_field_read(e, rec, field)),
        IrStmt::SqlExec { binds, .. } | IrStmt::SqlQuery { binds, .. } => {
            bind_field_read(binds, rec, field)
        }
        IrStmt::SqlExecDynamic { text_expr, .. } => expr_field_read(text_expr, rec, field),
        IrStmt::OpenCursor { source, .. } => match source {
            CursorSource::Static { binds, .. } => bind_field_read(binds, rec, field),
            CursorSource::Dynamic { query_expr } => expr_field_read(query_expr, rec, field),
        },
        IrStmt::CallRoutine { args, .. } => args.iter().any(|e| expr_field_read(e, rec, field)),
        _ => false,
    }
}

fn bind_field_read(binds: &[(String, BindSrc)], rec: &VarName, field: &str) -> bool {
    binds.iter().any(
        |(_, src)| matches!(src, BindSrc::Field { rec: r, field: f } if r == rec && f == field),
    )
}

fn expr_field_read(e: &IrExpr, rec: &VarName, field: &str) -> bool {
    match e {
        IrExpr::Field { rec: r, field: f } => r == rec && f == field,
        IrExpr::Bin { l, r, .. } => {
            expr_field_read(l, rec, field) || expr_field_read(r, rec, field)
        }
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand
                .as_ref()
                .is_some_and(|e| expr_field_read(e, rec, field))
                || whens
                    .iter()
                    .any(|(a, b)| expr_field_read(a, rec, field) || expr_field_read(b, rec, field))
                || else_expr
                    .as_ref()
                    .is_some_and(|e| expr_field_read(e, rec, field))
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_field_read(e, rec, field),
        IrExpr::FuncCall { args, .. } | IrExpr::SqlFn { args, .. } => {
            args.iter().any(|a| expr_field_read(a, rec, field))
        }
        IrExpr::Lookup { args, default, .. } => {
            args.iter().any(|(_, a)| expr_field_read(a, rec, field))
                || expr_field_read(default, rec, field)
        }
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::CursorAttr { .. } => false,
    }
}
