//! LookupTuple 全定义文法（设计 §3.2）：多列裸投影 + 与单列查找同口径的
//! WHERE 谓词文法；其余形态一律 UnprovenQueryTotalness。

use super::super::{BindSrc, Correlation, InFilter, InList, Name, SkipReason, VarName};
use super::{parse_where, projection_alias, LiteralTokens};

/// 多列投影解析产物（LookupTuple 共用 WHERE 文法）。
pub(crate) struct TupleParts {
    pub columns: Vec<Name>,
    pub table: Name,
    pub filter: String,
    pub correlations: Vec<Correlation>,
    pub in_filters: Vec<InFilter>,
    pub in_lists: Vec<InList>,
    pub binds: Vec<(String, BindSrc)>,
}

/// `SELECT 列 (, 列)* FROM 表 [别名] [WHERE ...]`。
/// 仅裸列投影（可带别名）；ORDER BY/LIMIT/派生表/CASE/coalesce 一律拒绝。
pub(crate) fn parse_tuple_total(
    sql: &str,
    binds: &[(String, BindSrc)],
    record: &VarName,
) -> Result<TupleParts, SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let mut t = LiteralTokens::new(sql).ok_or(bad)?;
    if !t.take("select") || t.peek() == Some("distinct") || t.peek() == Some("all") {
        return Err(bad);
    }
    let mut columns = Vec::new();
    loop {
        let col = t.column().ok_or(bad)?;
        projection_alias(&mut t, &col)?;
        columns.push(col);
        if !t.take(",") {
            break;
        }
    }
    if columns.is_empty() || !t.take("from") || t.peek() == Some("(") {
        return Err(bad);
    }
    let table = t.name().ok_or(bad)?;
    t.skip_alias();
    let predicates = parse_where(&mut t, binds, record)?;
    if !t.done() {
        return Err(bad);
    }
    Ok(TupleParts {
        columns,
        table,
        filter: predicates.sql(),
        correlations: predicates.correlations,
        in_filters: predicates.in_filters,
        in_lists: predicates.in_lists,
        binds: predicates.binds,
    })
}
