//! 兜底查找全定义文法解析（§4.4 B1）：SELECT 投影 / WHERE 谓词 / 有序
//! Top-1 / 异构 CASE 写汇的 token 流识别，产出 LookupQuery；其余形态
//! （函数/CAST/除法/嵌套子查询）一律保守拒绝，交回解释器。

mod in_pred;
mod tokens;
mod tuple;

pub(crate) use tuple::parse_tuple_total;

use super::{
    BindSrc, CaseSink, Correlation, CorrelationOp, InFilter, InList, LookupQuery, Name, SkipReason,
    Top1, VarName,
};
use crate::value::SpinValue;
use tokens::LiteralTokens;

/// 全定义文法：`SELECT col FROM table [WHERE cmp (AND cmp)*]
/// [ORDER BY 列 [ASC|DESC] LIMIT 1]`，cmp := 列 op 字面量
/// （=/<>/!=/</<=/>/>=）、列 = 游标字段绑定（可多键）或
/// 列 IN (SELECT 列 FROM 表 [WHERE 静态谓词])。
/// 其余形态（函数/CAST/除法）一律 UnprovenQueryTotalness。
pub(super) fn parse_total(
    sql: &str,
    var: VarName,
    default: SpinValue,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<LookupQuery, SkipReason> {
    let default_sql = fallback_sql(&default)?;
    let mut t = LiteralTokens::new(sql).ok_or(SkipReason::UnprovenQueryTotalness)?;
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("select") {
        return Err(bad);
    }
    let (column, nvl_default, case_sink) = if t.peek() == Some("case") {
        let cs = parse_case_sink(&mut t, binds, record)?;
        (Name(String::new()), None, Some(cs))
    } else {
        let (c, n) = parse_projection(&mut t).ok_or(bad)?;
        (c, n, None)
    };
    if !t.take("from") {
        return Err(bad);
    }
    if t.peek() == Some("(") {
        // 派生表包装 Top-1（语料 SELECT a.col FROM (SELECT ... ORDER BY) a
        // LIMIT 1）：仅 plain 投影适用；CASE 写汇/nvl 投影保守拒绝。
        if case_sink.is_some() || nvl_default.is_some() {
            return Err(bad);
        }
        let mut lq = parse_derived_top1(&mut t, binds, record, &column)?;
        lq.var = var;
        lq.default_sql = default_sql;
        return Ok(lq);
    }
    let table = t.name().ok_or(bad)?;
    t.skip_alias();
    let predicates = parse_where(&mut t, binds, record)?;
    let top1 = parse_top1(&mut t)?;
    validate_suffix(&t, &case_sink, &predicates, &top1)?;
    Ok(LookupQuery {
        var,
        table,
        column,
        nvl_default,
        filter: predicates.sql(),
        default_sql,
        correlations: predicates.correlations,
        in_filters: predicates.in_filters,
        in_lists: predicates.in_lists,
        top1,
        case_sink,
        group: 0,
        filter_on_miss: false,
        filter_binds: predicates.binds,
    })
}

/// 异构 CASE 投影：`CASE $v_operand WHEN lit THEN $v_rec__field ... ELSE
/// $v_rec__field END`。操作数须为变量绑定（同组查找），分支须为游标字段绑定。
fn parse_case_sink(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<CaseSink, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    t.take("case");
    let operand_var = match parse_case_bind(t, binds, record)? {
        CaseBind::Var(v) => v,
        CaseBind::Field(_) => return Err(bad),
    };
    let mut whens = Vec::new();
    while t.take("when") {
        let key = t.literal().ok_or(bad)?;
        if !t.take("then") {
            return Err(bad);
        }
        let field = match parse_case_bind(t, binds, record)? {
            CaseBind::Field(f) => f,
            CaseBind::Var(_) => return Err(bad),
        };
        whens.push((key, field));
    }
    if whens.is_empty() || !t.take("else") {
        return Err(bad);
    }
    let else_field = match parse_case_bind(t, binds, record)? {
        CaseBind::Field(f) => f,
        CaseBind::Var(_) => return Err(bad),
    };
    if !t.take("end") {
        return Err(bad);
    }
    Ok(CaseSink {
        operand: operand_var,
        whens,
        else_field,
    })
}

enum CaseBind {
    Var(VarName),
    Field(Name),
}

/// CASE 内的绑定占位符：`$v_<var>`（变量）或 `$v_<rec>__<field>`（游标字段）。
fn parse_case_bind(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<CaseBind, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let ph = t.peek().ok_or(bad)?.to_string();
    if !ph.starts_with("$v_") {
        return Err(bad);
    }
    t.take(&ph);
    match binds.iter().find(|(p, _)| *p == ph).map(|(_, s)| s) {
        Some(BindSrc::Var(v)) => Ok(CaseBind::Var(v.clone())),
        Some(BindSrc::Field { rec, field }) if rec == record => {
            Ok(CaseBind::Field(Name(field.clone())))
        }
        _ => Err(bad),
    }
}

/// 投影：裸列或 `coalesce(列, 常量)`（源 nvl 经 IR 归一为 coalesce，
/// 命中 NULL 取常量）。其余函数投影保守拒绝（交回解释器）。
fn parse_projection(t: &mut LiteralTokens) -> Option<(Name, Option<String>)> {
    if t.peek() == Some("coalesce") && t.words.get(t.pos + 1).is_some_and(|w| w == "(") {
        t.take("coalesce");
        t.take("(");
        let col = t.column()?;
        if !t.take(",") {
            return None;
        }
        let lit = t.literal()?;
        if !t.take(")") {
            return None;
        }
        return Some((col, Some(lit)));
    }
    Some((t.column()?, None))
}

/// 有序 Top-1 后缀：`ORDER BY 列 [ASC|DESC] (, 列 [ASC|DESC])* LIMIT 1`，
/// 缺一即拒；至少一列。
fn parse_top1(t: &mut LiteralTokens) -> Result<Option<Top1>, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("order") {
        return Ok(None);
    }
    if !t.take("by") {
        return Err(bad);
    }
    let order_by = parse_order_by(t)?;
    if !t.take("limit") || t.literal().ok_or(bad)? != "1" {
        return Err(bad);
    }
    Ok(Some(Top1 { order_by }))
}

/// ORDER BY 列清单（不含 LIMIT）：`列 [ASC|DESC] (, 列 [ASC|DESC])*`，
/// 列可带表别名限定（前缀丢弃）；至少一列。派生表内层与平铺形态共用。
fn parse_order_by(t: &mut LiteralTokens) -> Result<Vec<(Name, bool)>, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let mut order_by = Vec::new();
    loop {
        let column = t.column().ok_or(bad)?;
        let descending = t.take("desc");
        if !descending {
            t.take("asc");
        }
        order_by.push((column, descending));
        if !t.take(",") {
            break;
        }
    }
    Ok(order_by)
}

/// 派生表包装 Top-1（§4.4 B1，语料 v_commision_fee_type 形态）：
/// `(SELECT 内列 [AS 别名] FROM 实表 [AS 别名] [WHERE ...] ORDER BY ...)`
/// `[AS] 外别名 LIMIT 1`。外层投影别名须等于内层投影别名；内层 WHERE 产出
/// 相关键/IN/静态过滤，内层 ORDER BY 产出排序键，外层 LIMIT 1 即首行语义。
/// table/column 取内层实表/实列（渲染器据此从实表取值）。仅相关 Top-1
/// （不变量首行歧义无法按组守卫）；var/default_sql 由调用方回填。
fn parse_derived_top1(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
    outer_col: &Name,
) -> Result<LookupQuery, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if !t.take("(") || !t.take("select") {
        return Err(bad);
    }
    let inner_col = t.column().ok_or(bad)?;
    let inner_alias = projection_alias(t, &inner_col)?;
    if &inner_alias != outer_col {
        return Err(bad);
    }
    if !t.take("from") {
        return Err(bad);
    }
    let table = t.name().ok_or(bad)?;
    t.skip_alias();
    let predicates = parse_where(t, binds, record)?;
    if !t.take("order") || !t.take("by") {
        return Err(bad);
    }
    let order_by = parse_order_by(t)?;
    if !t.take(")") {
        return Err(bad);
    }
    t.skip_alias();
    if !t.take("limit") || t.literal().ok_or(bad)? != "1" {
        return Err(bad);
    }
    if predicates.correlations.is_empty() {
        return Err(bad);
    }
    Ok(LookupQuery {
        var: VarName("__derived".to_string()),
        table,
        column: inner_col,
        nvl_default: None,
        filter: predicates.sql(),
        default_sql: String::new(),
        correlations: predicates.correlations,
        in_filters: predicates.in_filters,
        in_lists: predicates.in_lists,
        top1: Some(Top1 { order_by }),
        case_sink: None,
        group: 0,
        filter_on_miss: false,
        filter_binds: predicates.binds,
    })
}

#[derive(Default)]
struct WhereClause {
    parts: Vec<String>,
    correlations: Vec<Correlation>,
    in_filters: Vec<InFilter>,
    in_lists: Vec<InList>,
    binds: Vec<(String, BindSrc)>,
}

impl WhereClause {
    fn sql(&self) -> String {
        if self.parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.parts.join(" AND "))
        }
    }
}

pub(super) fn fallback_sql(default: &SpinValue) -> Result<String, SkipReason> {
    match default {
        SpinValue::Str(s) => Ok(format!("'{}'", s.replace('\'', "''"))),
        SpinValue::Int(i) => Ok(i.to_string()),
        SpinValue::Null => Ok("NULL".into()),
        _ => Err(SkipReason::UnprovenQueryTotalness),
    }
}

fn projection_alias(t: &mut LiteralTokens, column: &Name) -> Result<Name, SkipReason> {
    let has_alias = t.take("as")
        || t.peek().is_some_and(|w| {
            w != "from"
                && w.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        });
    if has_alias {
        t.name().ok_or(SkipReason::UnprovenQueryTotalness)
    } else {
        Ok(column.clone())
    }
}

fn validate_suffix(
    t: &LiteralTokens,
    case_sink: &Option<CaseSink>,
    predicates: &WhereClause,
    top1: &Option<Top1>,
) -> Result<(), SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    if top1.is_some() && predicates.correlations.is_empty() {
        return Err(bad);
    }
    if case_sink.is_some()
        && (!predicates.parts.is_empty()
            || !predicates.correlations.is_empty()
            || top1.is_some()
            || !predicates.in_filters.is_empty()
            || !predicates.in_lists.is_empty())
    {
        return Err(bad);
    }
    if !t.done() {
        return Err(bad);
    }
    Ok(())
}

fn parse_where(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<WhereClause, SkipReason> {
    let mut result = WhereClause::default();
    if !t.take("where") {
        return Ok(result);
    }
    loop {
        if t.peek().is_some_and(|w| w.starts_with("$v_"))
            && t.words.get(t.pos + 1).is_some_and(|w| w == "between")
        {
            parse_between(t, binds, record, &mut result)?;
        } else {
            parse_column_predicate(t, binds, record, &mut result)?;
        }
        if !t.take("and") {
            break;
        }
    }
    Ok(result)
}

fn parse_between(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
    result: &mut WhereClause,
) -> Result<(), SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let ph = t.peek().ok_or(bad)?.to_string();
    let Some((_, src)) = binds.iter().find(|(p, _)| *p == ph) else {
        return Err(bad);
    };
    t.take(&ph);
    t.take("between");
    let lo = t.column().ok_or(bad)?;
    if !t.take("and") {
        return Err(bad);
    }
    let hi = t.column().ok_or(bad)?;
    match src {
        BindSrc::Var(_) => {
            result.binds.push((ph.clone(), src.clone()));
            result
                .parts
                .push(format!("{ph} BETWEEN {} AND {}", lo.sql(), hi.sql()));
        }
        BindSrc::Field { rec, field } => {
            if rec != record {
                return Err(bad);
            }
            for (dim_column, op) in [(lo, CorrelationOp::Le), (hi, CorrelationOp::Ge)] {
                result.correlations.push(Correlation {
                    dim_column,
                    cursor_field: Name(field.clone()),
                    op,
                });
            }
        }
    }
    Ok(())
}

fn parse_column_predicate(
    t: &mut LiteralTokens,
    binds: &[(String, BindSrc)],
    record: &VarName,
    result: &mut WhereClause,
) -> Result<(), SkipReason> {
    let col = t.column().ok_or(SkipReason::UnprovenQueryTotalness)?;
    if t.take("in") {
        if t.peek() == Some("(") && t.words.get(t.pos + 1).is_some_and(|w| w == "select") {
            result.in_filters.push(in_pred::parse_in_filter(
                t,
                col,
                binds,
                record,
                &mut result.binds,
            )?);
        } else {
            result
                .in_lists
                .push(in_pred::parse_in_list(t, col, binds, record)?);
        }
    } else if !parse_var_eq(t, &col, binds, result)? {
        parse_cmp(t, col, binds, record, result)?;
    }
    Ok(())
}

/// 单个比较谓词：字面量比较或相关键等值绑定（可多键）。
fn parse_cmp(
    t: &mut LiteralTokens,
    col: Name,
    binds: &[(String, BindSrc)],
    record: &VarName,
    result: &mut WhereClause,
) -> Result<(), SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let op = t.peek().ok_or(bad)?.to_string();
    if !matches!(op.as_str(), "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
        return Err(bad);
    }
    t.take(&op);
    if op == "=" && t.peek().is_some_and(|w| w.starts_with("$v_")) {
        let word = t.peek().ok_or(bad)?.to_string();
        t.take(&word);
        let Some((_, BindSrc::Field { rec, field })) = binds.iter().find(|(p, _)| *p == word)
        else {
            return Err(bad);
        };
        if rec != record {
            return Err(bad);
        }
        result.correlations.push(Correlation {
            dim_column: col,
            cursor_field: Name(field.clone()),
            op: CorrelationOp::Eq,
        });
    } else {
        let lit = t.literal().ok_or(bad)?;
        result.parts.push(format!("{} {op} {lit}", col.sql()));
    }
    Ok(())
}

/// 循环不变变量等值过滤（§4.4 B1）：`col = $v_var`（Var 绑定，如
/// s.fund_code = v_fund_code）。作为静态谓词进过滤串并携带绑定，运行期
/// 从帧解析单值；变量循环不变性由 generate 的区域赋值证明裁决。非此形态
/// 返回 Ok(false) 交回 parse_cmp（相关键 Field 等值/字面量比较）。
fn parse_var_eq(
    t: &mut LiteralTokens,
    col: &Name,
    binds: &[(String, BindSrc)],
    result: &mut WhereClause,
) -> Result<bool, SkipReason> {
    if t.peek() != Some("=") {
        return Ok(false);
    }
    let Some(w) = t.words.get(t.pos + 1).cloned() else {
        return Ok(false);
    };
    if !w.starts_with("$v_") {
        return Ok(false);
    }
    let Some((_, src)) = binds.iter().find(|(p, _)| *p == w) else {
        return Ok(false);
    };
    if !matches!(src, BindSrc::Var(_)) {
        return Ok(false);
    }
    t.take("=");
    t.take(&w);
    result.binds.push((w.clone(), src.clone()));
    result.parts.push(format!("{} = {w}", col.sql()));
    Ok(true)
}
