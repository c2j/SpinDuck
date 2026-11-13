//! 构建 SQL 的渲染，以及谓词分类。
//!
//! 日期区间和字面量等值留在 SQL 里；等值绑定、IN 列表和半连接在探测时求值。

use super::*;
use ogsql_parser::ast::{
    Expr, Literal, ObjectName, OrderByItem, SelectStatement, SelectTarget, Statement, TableRef,
};
use ogsql_parser::parser::Parser;
use ogsql_parser::token::tokenizer::Tokenizer;

/// SELECT 列表的结果列名。别名优先；无别名的列取列名；表达式退回 `c{i}`。
pub(in crate::lookup_cache) fn output_names(sql: &str) -> Option<Vec<String>> {
    let tokens = Tokenizer::new(sql).tokenize().ok()?;
    let stmts = Parser::new(tokens).parse();
    if stmts.len() != 1 {
        return None;
    }
    let Statement::Select(sel) = stmts.into_iter().next()? else {
        return None;
    };
    if sel.node.targets.is_empty() {
        return None;
    }
    Some(
        sel.node
            .targets
            .iter()
            .enumerate()
            .map(|(i, target)| select_output_name(target).unwrap_or_else(|| format!("c{i}")))
            .collect(),
    )
}

fn select_output_name(target: &SelectTarget) -> Option<String> {
    match target {
        SelectTarget::Expr(_, Some(alias)) => Some(alias.value.to_ascii_lowercase()),
        SelectTarget::Expr(expr, None) => col_name(expr),
        SelectTarget::Star(_) => None,
    }
}

pub(super) fn render_scan(
    table: &TableName,
    alias: &Option<String>,
    slots: &[Slot],
    lits: &[(String, SpinValue)],
    ranges: &[(String, String, String)],
) -> Option<String> {
    if slots.is_empty() {
        return None;
    }
    let cols = slots
        .iter()
        .map(|s| match s {
            Slot::Col(c) => render_col(alias, c),
            Slot::Expr(e) => e.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let from = match alias {
        Some(a) => format!("{} AS {a}", table.rendered),
        None => table.rendered.clone(),
    };
    let mut where_parts = Vec::new();
    for (col, val) in lits {
        where_parts.push(format!("{} = {}", render_col(alias, col), render_lit(val)?));
    }
    for (bind, low, high) in ranges {
        where_parts.push(format!(
            "{bind} BETWEEN {} AND {}",
            render_col(alias, low),
            render_col(alias, high)
        ));
    }
    let where_sql = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };
    Some(format!("SELECT {cols} FROM {from}{where_sql}"))
}

pub(super) fn render_col(alias: &Option<String>, col: &str) -> String {
    match alias {
        Some(a) => format!("{a}.{col}"),
        None => col.to_string(),
    }
}

pub(super) fn render_lit(v: &SpinValue) -> Option<String> {
    match v {
        SpinValue::Str(s) => Some(format!("'{}'", s.replace('\'', "''"))),
        SpinValue::Int(i) => Some(i.to_string()),
        SpinValue::Bool(true) => Some("TRUE".to_string()),
        SpinValue::Bool(false) => Some("FALSE".to_string()),
        SpinValue::Null => Some("NULL".to_string()),
        _ => None,
    }
}

pub(super) fn parse_limit(limit: Option<&Expr>) -> Option<Option<usize>> {
    match limit {
        None => Some(None),
        Some(Expr::Literal(Literal::Integer(n))) if *n >= 0 => Some(Some(*n as usize)),
        _ => None,
    }
}

pub(super) fn parse_order(
    items: &[OrderByItem],
    alias: &Option<String>,
) -> Option<Vec<(String, bool, bool)>> {
    items
        .iter()
        .map(|it| {
            if it.using.is_some() {
                return None;
            }
            let col = qualified_col(&it.expr, alias)?;
            let desc = it.asc == Some(false);
            let nulls_first = it.nulls_first.unwrap_or(desc);
            Some((col, desc, nulls_first))
        })
        .collect()
}

pub(super) struct TableName {
    pub rendered: String,
    pub last: String,
}

pub(super) fn table_of(t: &TableRef) -> Option<(TableName, Option<String>)> {
    let TableRef::Table {
        name,
        alias,
        partition,
        timecapsule,
        tablesample,
        ..
    } = t
    else {
        return None;
    };
    if partition.is_some() || timecapsule.is_some() || tablesample.is_some() {
        return None;
    }
    let last = name.last()?.value.clone();
    let rendered = render_name(name);
    let alias = alias.as_ref().map(|a| a.value.clone());
    Some((TableName { rendered, last }, alias))
}

pub(super) fn render_name(name: &ObjectName) -> String {
    name.iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

pub(super) enum Proj {
    Col(String),
    Coalesce { col: String, lit: SpinValue },
}

pub(super) fn proj_of(t: &SelectTarget, alias: &Option<String>) -> Option<Proj> {
    let SelectTarget::Expr(e, _) = t else {
        return None;
    };
    match peel(e) {
        Expr::FunctionCall {
            name,
            args,
            distinct,
            over,
            filter,
            ..
        } => {
            if *distinct || over.is_some() || filter.is_some() || args.len() != 2 {
                return None;
            }
            if !name.last()?.value.eq_ignore_ascii_case("coalesce") {
                return None;
            }
            let col = qualified_col(&args[0], alias)?;
            let lit = lit_val(&args[1])?;
            Some(Proj::Coalesce { col, lit })
        }
        _ => Some(Proj::Col(qualified_col(e, alias)?)),
    }
}

pub(super) enum Pred {
    Eq {
        col: String,
        value: ProbeVal,
    },
    Between {
        bind: String,
        low: String,
        high: String,
    },
    InList {
        col: String,
        vals: Vec<ProbeVal>,
    },
    InSub(SubLookup),
}

pub(super) struct SubLookup {
    pub outer_col: String,
    pub table: TableName,
    pub alias: Option<String>,
    pub inner_match: String,
    pub preds: Vec<Pred>,
}

pub(super) fn flatten_and(e: &Expr, alias: &Option<String>, out: &mut Vec<Pred>) -> bool {
    let e = peel(e);
    if let Expr::BinaryOp { left, op, right } = e {
        if op.eq_ignore_ascii_case("AND") {
            return flatten_and(left, alias, out) && flatten_and(right, alias, out);
        }
    }
    match classify(e, alias) {
        Some(pred) => {
            out.push(pred);
            true
        }
        None => false,
    }
}

fn classify(e: &Expr, alias: &Option<String>) -> Option<Pred> {
    match peel(e) {
        Expr::BinaryOp { left, op, right } if op == "=" => eq_pred(left, right, alias),
        Expr::BinaryOp { op, .. } if op.eq_ignore_ascii_case("OR") => or_as_in(e, alias),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } if !negated => Some(Pred::Between {
            bind: bind_name(expr)?,
            low: qualified_col(low, alias)?,
            high: qualified_col(high, alias)?,
        }),
        Expr::InList {
            expr,
            list,
            negated,
        } if !negated => Some(Pred::InList {
            col: qualified_col(expr, alias)?,
            vals: list.iter().map(probe_val).collect::<Option<Vec<_>>>()?,
        }),
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } if !negated => in_sub(expr, subquery, alias),
        _ => None,
    }
}

fn eq_pred(left: &Expr, right: &Expr, alias: &Option<String>) -> Option<Pred> {
    let (col_side, val_side) = if qualified_col(left, alias).is_some() && probe_val(right).is_some()
    {
        (left, right)
    } else if qualified_col(right, alias).is_some() && probe_val(left).is_some() {
        (right, left)
    } else {
        return None;
    };
    Some(Pred::Eq {
        col: qualified_col(col_side, alias)?,
        value: probe_val(val_side)?,
    })
}

fn or_as_in(e: &Expr, alias: &Option<String>) -> Option<Pred> {
    let mut terms = Vec::new();
    if !flatten_or(e, alias, &mut terms) || terms.len() < 2 {
        return None;
    }
    let mut col = None;
    let mut vals = Vec::new();
    for term in terms {
        let Pred::Eq { col: c, value } = term else {
            return None;
        };
        if *col.get_or_insert(c.clone()) != c {
            return None;
        }
        vals.push(value);
    }
    Some(Pred::InList { col: col?, vals })
}

fn flatten_or(e: &Expr, alias: &Option<String>, out: &mut Vec<Pred>) -> bool {
    let e = peel(e);
    if let Expr::BinaryOp { left, op, right } = e {
        if op.eq_ignore_ascii_case("OR") {
            return flatten_or(left, alias, out) && flatten_or(right, alias, out);
        }
    }
    match classify(e, alias) {
        Some(pred) => {
            out.push(pred);
            true
        }
        None => false,
    }
}

fn in_sub(expr: &Expr, sub: &SelectStatement, outer_alias: &Option<String>) -> Option<Pred> {
    if !is_plain(sub) || !sub.order_by.is_empty() || sub.limit.is_some() || sub.from.len() != 1 {
        return None;
    }
    let outer_col = qualified_col(expr, outer_alias)?;
    let (table, alias) = table_of(&sub.from[0])?;
    let projs = sub
        .targets
        .iter()
        .map(|t| proj_of(t, &alias))
        .collect::<Option<Vec<_>>>()?;
    let Proj::Col(inner_match) = projs.first()? else {
        return None;
    };
    if projs.len() != 1 {
        return None;
    }
    let preds = preds_of(sub.where_clause.as_ref(), &alias)?;
    if preds
        .iter()
        .any(|p| matches!(p, Pred::InSub(_) | Pred::InList { .. }))
    {
        return None;
    }
    Some(Pred::InSub(SubLookup {
        outer_col,
        table,
        alias,
        inner_match: inner_match.clone(),
        preds,
    }))
}

fn probe_val(e: &Expr) -> Option<ProbeVal> {
    if let Some(name) = bind_name(e) {
        return Some(ProbeVal::Bind(name));
    }
    if let Some(v) = lit_val(e) {
        return Some(ProbeVal::Lit(v));
    }
    substr_concat(peel(e))
}

fn substr_concat(e: &Expr) -> Option<ProbeVal> {
    let Expr::FunctionCall {
        name,
        args,
        distinct,
        over,
        filter,
        ..
    } = e
    else {
        return None;
    };
    if *distinct || over.is_some() || filter.is_some() || args.len() != 2 {
        return None;
    }
    if !name.last()?.value.eq_ignore_ascii_case("concat") {
        return None;
    }
    let suffix = match lit_val(&args[1]) {
        Some(SpinValue::Str(s)) => s,
        _ => return None,
    };
    let (fname, fargs) = fn_args(&args[0])?;
    if !matches!(fname.as_str(), "substring" | "substr") || fargs.len() != 3 {
        return None;
    }
    let bind = bind_name(&fargs[0])?;
    let start = int_lit(&fargs[1])?;
    let len = int_lit(&fargs[2])?;
    if start < 1 || len < 0 {
        return None;
    }
    Some(ProbeVal::SubstrConcat {
        bind,
        start,
        len,
        suffix,
    })
}

fn fn_args(e: &Expr) -> Option<(String, Vec<Expr>)> {
    match peel(e) {
        Expr::FunctionCall {
            name,
            args,
            distinct,
            over,
            filter,
            ..
        } => {
            if *distinct || over.is_some() || filter.is_some() {
                return None;
            }
            Some((name.last()?.value.to_ascii_lowercase(), args.clone()))
        }
        Expr::SpecialFunction { name, args, .. } => Some((name.to_ascii_lowercase(), args.clone())),
        _ => None,
    }
}

fn qualified_col(e: &Expr, alias: &Option<String>) -> Option<String> {
    let Expr::ColumnRef(name) = peel(e) else {
        return None;
    };
    if name.len() == 2 {
        let ok = alias
            .as_ref()
            .is_some_and(|a| a.eq_ignore_ascii_case(&name[0].value));
        if !ok {
            return None;
        }
    } else if name.len() != 1 {
        return None;
    }
    let col = name.last()?.value.to_ascii_lowercase();
    if col.starts_with('$') {
        return None;
    }
    Some(col)
}

pub(super) fn col_name(e: &Expr) -> Option<String> {
    let Expr::ColumnRef(name) = peel(e) else {
        return None;
    };
    name.last().map(|i| i.value.to_ascii_lowercase())
}

fn bind_name(e: &Expr) -> Option<String> {
    let Expr::ColumnRef(name) = peel(e) else {
        return None;
    };
    if name.len() == 1 && name[0].value.starts_with('$') {
        Some(name[0].value.clone())
    } else {
        None
    }
}

fn lit_val(e: &Expr) -> Option<SpinValue> {
    match peel(e) {
        Expr::Literal(Literal::String(s)) => Some(SpinValue::Str(s.clone())),
        Expr::Literal(Literal::Integer(i)) => Some(SpinValue::Int(*i)),
        Expr::Literal(Literal::Boolean(b)) => Some(SpinValue::Bool(*b)),
        Expr::Literal(Literal::Null) => Some(SpinValue::Null),
        _ => None,
    }
}

fn int_lit(e: &Expr) -> Option<i64> {
    match lit_val(e)? {
        SpinValue::Int(i) => Some(i),
        _ => None,
    }
}

pub(super) fn peel(e: &Expr) -> &Expr {
    match e {
        Expr::Parenthesized(inner) => peel(inner),
        _ => e,
    }
}
