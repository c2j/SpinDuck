//! 把改写后的点查 SELECT 认成“不变谓词扫一遍 + 等值键探测”。
//!
//! 认不出的语句返回 `None`，调用方原样下发。日期 `BETWEEN` 和字面量等值留在
//! 构建 SQL 里，由 DuckDB 计算；探测端不重写这套三值逻辑。

use crate::value::SpinValue;
use ogsql_parser::ast::{
    Expr, Ident, OrderByItem, SelectStatement, SelectTarget, Statement, TableRef,
};
use ogsql_parser::parser::Parser;

pub(super) mod sqlgen;
use ogsql_parser::token::tokenizer::Tokenizer;
use sqlgen::{
    col_name, flatten_and, parse_limit, parse_order, peel, proj_of, render_col, render_lit,
    render_scan, table_of, Pred, Proj, SubLookup, TableName,
};

/// 一次构建查询。SQL 已带上日期区间和字面量等值，`binds` 是区间参数名。
#[derive(Debug)]
pub(super) struct Build {
    pub sql: String,
    pub binds: Vec<String>,
    pub width: usize,
}

/// 探测时才能求值的键或列表成员。
#[derive(Debug)]
pub(super) enum ProbeVal {
    Bind(String),
    Lit(SpinValue),
    /// `concat(substring($bind, start, len), suffix)`，席位类别候选的改写形态。
    SubstrConcat {
        bind: String,
        start: i64,
        len: i64,
        suffix: String,
    },
}

#[derive(Debug)]
pub(super) struct Filter {
    pub at: usize,
    pub vals: Vec<ProbeVal>,
}

#[derive(Debug)]
pub(super) struct SortKey {
    pub at: usize,
    pub desc: bool,
    pub nulls_first: bool,
}

#[derive(Debug)]
pub(super) struct RowPlan {
    pub key_at: Vec<usize>,
    pub key_binds: Vec<String>,
    pub filters: Vec<Filter>,
    pub order: Vec<SortKey>,
    pub limit: Option<usize>,
    pub proj_at: Vec<usize>,
    /// 与 `proj_at` 对齐。列投影记列名，`coalesce` 记 `None`。
    pub proj_cols: Vec<Option<String>>,
}

#[derive(Debug)]
pub(super) struct SemiPlan {
    pub outer_key_at: Vec<usize>,
    pub outer_key_binds: Vec<String>,
    pub outer_match_at: usize,
    pub outer_proj_at: Vec<usize>,
    pub inner_key_at: Vec<usize>,
    pub inner_key_binds: Vec<String>,
    pub inner_match_at: usize,
}

#[derive(Debug)]
pub(super) enum Kind {
    Rows(RowPlan),
    Semi(SemiPlan),
}

#[derive(Debug)]
pub(super) struct Plan {
    pub tables: Vec<String>,
    pub builds: Vec<Build>,
    pub kind: Kind,
}

impl Plan {
    pub(super) fn proj_len(&self) -> usize {
        match &self.kind {
            Kind::Rows(r) => r.proj_at.len(),
            Kind::Semi(s) => s.outer_proj_at.len(),
        }
    }

    /// 构建用到的区间绑定，按出现顺序去重。不同日期是不同的缓存键。
    pub(super) fn range_binds(&self) -> Vec<String> {
        let mut out = Vec::new();
        for build in &self.builds {
            for name in &build.binds {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
        out
    }
}

/// 认一条改写后的 SELECT。`None` 表示本缓存不接管。
pub(super) fn recognize(sql: &str) -> Option<Plan> {
    let sel = parse_select(sql)?;
    if !is_plain(&sel) {
        return None;
    }
    let sel = flatten(sel)?;
    if !is_plain(&sel) {
        return None;
    }
    analyze(sel)
}

fn parse_select(sql: &str) -> Option<SelectStatement> {
    let tokens = Tokenizer::new(sql).tokenize().ok()?;
    let stmts = Parser::new(tokens).parse();
    if stmts.len() != 1 {
        return None;
    }
    match stmts.into_iter().next()? {
        Statement::Select(sel) => Some(sel.node),
        _ => None,
    }
}

fn is_plain(sel: &SelectStatement) -> bool {
    sel.hints.is_empty()
        && sel.with.is_none()
        && !sel.distinct
        && sel.distinct_on.is_empty()
        && sel.into_targets.is_none()
        && !sel.bulk_collect
        && sel.into_table.is_none()
        && sel.connect_by.is_none()
        && sel.group_by.is_empty()
        && sel.having.is_none()
        && sel.offset.is_none()
        && sel.fetch.is_none()
        && sel.lock_clause.is_none()
        && sel.window_clause.is_empty()
        && sel.set_operation.is_none()
}

/// `SELECT a.col FROM (内层 ORDER BY ...) AS a LIMIT n` 收成内层点查。
fn flatten(sel: SelectStatement) -> Option<SelectStatement> {
    if !matches!(sel.from.first(), Some(TableRef::Subquery { .. })) {
        return Some(sel);
    }
    if sel.where_clause.is_some() || !sel.order_by.is_empty() || sel.from.len() != 1 {
        return None;
    }
    let TableRef::Subquery {
        query,
        alias,
        lateral,
    } = &sel.from[0]
    else {
        return None;
    };
    if *lateral {
        return None;
    }
    let mut inner = (**query).clone();
    if inner.limit.is_some() && sel.limit.is_some() {
        return None;
    }
    inner.targets = sel
        .targets
        .iter()
        .map(|t| map_outer_target(t, &inner.targets, alias))
        .collect::<Option<Vec<_>>>()?;
    if sel.limit.is_some() {
        inner.limit = sel.limit;
    }
    Some(inner)
}

fn map_outer_target(
    outer: &SelectTarget,
    inner: &[SelectTarget],
    sub_alias: &Option<Ident>,
) -> Option<SelectTarget> {
    let SelectTarget::Expr(expr, _) = outer else {
        return None;
    };
    let Expr::ColumnRef(name) = peel(expr) else {
        return None;
    };
    if name.len() == 2 {
        let ok = sub_alias
            .as_ref()
            .is_some_and(|a| a.value.eq_ignore_ascii_case(&name[0].value));
        if !ok {
            return None;
        }
    }
    let want = name.last()?.value.to_ascii_lowercase();
    inner
        .iter()
        .find(|t| target_name(t).as_deref() == Some(want.as_str()))
        .cloned()
}

fn target_name(t: &SelectTarget) -> Option<String> {
    match t {
        SelectTarget::Expr(_, Some(a)) => Some(a.value.to_ascii_lowercase()),
        SelectTarget::Expr(e, None) => col_name(e),
        SelectTarget::Star(_) => None,
    }
}

fn analyze(sel: SelectStatement) -> Option<Plan> {
    if sel.from.len() != 1 {
        return None;
    }
    let (table, alias) = table_of(&sel.from[0])?;
    let projs = sel
        .targets
        .iter()
        .map(|t| proj_of(t, &alias))
        .collect::<Option<Vec<_>>>()?;
    if projs.is_empty() {
        return None;
    }
    let preds = preds_of(sel.where_clause.as_ref(), &alias)?;
    if preds.iter().any(|p| matches!(p, Pred::InSub(_))) {
        return semi_plan(table, alias, projs, preds, &sel);
    }
    row_plan(
        table,
        alias,
        projs,
        preds,
        &sel.order_by,
        sel.limit.as_ref(),
    )
}

fn preds_of(where_clause: Option<&Expr>, alias: &Option<String>) -> Option<Vec<Pred>> {
    let Some(w) = where_clause else {
        return Some(Vec::new());
    };
    let mut out = Vec::new();
    if !flatten_and(w, alias, &mut out) {
        return None;
    }
    Some(out)
}

fn row_plan(
    table: TableName,
    alias: Option<String>,
    projs: Vec<Proj>,
    preds: Vec<Pred>,
    order_by: &[OrderByItem],
    limit: Option<&Expr>,
) -> Option<Plan> {
    let limit = parse_limit(limit)?;
    let order_cols = parse_order(order_by, &alias)?;
    if limit.is_some() && order_cols.is_empty() {
        return None;
    }
    let split = split_preds(preds)?;
    if dup_name(split.keys.iter().map(|(c, _)| c)) || dup_name(split.filters.iter().map(|(c, _)| c))
    {
        return None;
    }
    let mut slots = Vec::new();
    let key_at = slot_at(&mut slots, split.keys.iter().map(|(c, _)| c.as_str()));
    let filter_at = slot_at(&mut slots, split.filters.iter().map(|(c, _)| c.as_str()));
    let order_at = slot_at(&mut slots, order_cols.iter().map(|(c, _, _)| c.as_str()));
    let (proj_at, proj_cols) = project_at(&mut slots, &projs, &alias)?;
    let sql = render_scan(&table, &alias, &slots, &split.lits, &split.ranges)?;
    Some(Plan {
        tables: vec![table.last.to_ascii_lowercase()],
        builds: vec![Build {
            sql,
            binds: split.ranges.iter().map(|(b, _, _)| b.clone()).collect(),
            width: slots.len(),
        }],
        kind: Kind::Rows(RowPlan {
            key_at,
            key_binds: split.keys.into_iter().map(|(_, b)| b).collect(),
            filters: filter_at
                .into_iter()
                .zip(split.filters)
                .map(|(at, (_, vals))| Filter { at, vals })
                .collect(),
            order: order_at
                .into_iter()
                .zip(order_cols)
                .map(|(at, (_, desc, nulls_first))| SortKey {
                    at,
                    desc,
                    nulls_first,
                })
                .collect(),
            limit,
            proj_at,
            proj_cols,
        }),
    })
}

fn slot_at<'a>(slots: &mut Vec<Slot>, names: impl Iterator<Item = &'a str>) -> Vec<usize> {
    names.map(|name| ensure_col(slots, name)).collect()
}

struct Split {
    keys: Vec<(String, String)>,
    lits: Vec<(String, SpinValue)>,
    ranges: Vec<(String, String, String)>,
    filters: Vec<(String, Vec<ProbeVal>)>,
}

fn split_preds(preds: Vec<Pred>) -> Option<Split> {
    let mut split = Split {
        keys: Vec::new(),
        lits: Vec::new(),
        ranges: Vec::new(),
        filters: Vec::new(),
    };
    for pred in preds {
        match pred {
            Pred::Eq { col, value } => match value {
                ProbeVal::Bind(b) => split.keys.push((col, b)),
                ProbeVal::Lit(v) => split.lits.push((col, v)),
                other => split.filters.push((col, vec![other])),
            },
            Pred::Between { bind, low, high } => split.ranges.push((bind, low, high)),
            Pred::InList { col, vals } => split.filters.push((col, vals)),
            Pred::InSub(_) => return None,
        }
    }
    Some(split)
}

fn semi_plan(
    table: TableName,
    alias: Option<String>,
    projs: Vec<Proj>,
    preds: Vec<Pred>,
    sel: &SelectStatement,
) -> Option<Plan> {
    if !sel.order_by.is_empty() || sel.limit.is_some() {
        return None;
    }
    let (sub, rest) = take_one_sub(preds)?;
    let outer = row_plan(table, alias, projs, rest, &[], None)?;
    let outer_bits = semi_outer(&outer, &sub.outer_col)?;
    let inner = row_plan(
        sub.table,
        sub.alias,
        vec![Proj::Col(sub.inner_match)],
        sub.preds,
        &[],
        None,
    )?;
    let inner_bits = semi_inner(&inner)?;
    let mut tables = outer.tables;
    tables.extend(inner.tables);
    let mut builds = outer.builds;
    builds.extend(inner.builds);
    Some(Plan {
        tables,
        builds,
        kind: Kind::Semi(SemiPlan {
            outer_key_at: outer_bits.key_at,
            outer_key_binds: outer_bits.key_binds,
            outer_match_at: outer_bits.match_at,
            outer_proj_at: outer_bits.proj_at,
            inner_key_at: inner_bits.0,
            inner_key_binds: inner_bits.1,
            inner_match_at: inner_bits.2,
        }),
    })
}

struct OuterBits {
    key_at: Vec<usize>,
    key_binds: Vec<String>,
    match_at: usize,
    proj_at: Vec<usize>,
}

fn semi_outer(plan: &Plan, match_col: &str) -> Option<OuterBits> {
    let Kind::Rows(rows) = &plan.kind else {
        return None;
    };
    if !rows.filters.is_empty() || rows.limit.is_some() {
        return None;
    }
    let match_at = rows
        .proj_at
        .iter()
        .zip(&rows.proj_cols)
        .find(|(_, col)| col.as_deref() == Some(match_col))
        .map(|(at, _)| *at)?;
    Some(OuterBits {
        key_at: rows.key_at.clone(),
        key_binds: rows.key_binds.clone(),
        match_at,
        proj_at: rows.proj_at.clone(),
    })
}

fn semi_inner(plan: &Plan) -> Option<(Vec<usize>, Vec<String>, usize)> {
    let Kind::Rows(rows) = &plan.kind else {
        return None;
    };
    if !rows.filters.is_empty() || rows.proj_at.len() != 1 {
        return None;
    }
    Some((rows.key_at.clone(), rows.key_binds.clone(), rows.proj_at[0]))
}

fn take_one_sub(preds: Vec<Pred>) -> Option<(SubLookup, Vec<Pred>)> {
    let mut sub = None;
    let mut rest = Vec::new();
    for pred in preds {
        match pred {
            Pred::InSub(s) => {
                if sub.is_some() {
                    return None;
                }
                sub = Some(s);
            }
            Pred::InList { .. } => return None,
            other => rest.push(other),
        }
    }
    Some((sub?, rest))
}

fn dup_name<'a>(names: impl Iterator<Item = &'a String>) -> bool {
    let mut seen = Vec::new();
    for name in names {
        if seen.contains(name) {
            return true;
        }
        seen.push(name.clone());
    }
    false
}

enum Slot {
    Col(String),
    Expr(String),
}

fn ensure_col(slots: &mut Vec<Slot>, name: &str) -> usize {
    if let Some(i) = slots
        .iter()
        .position(|s| matches!(s, Slot::Col(n) if n == name))
    {
        return i;
    }
    slots.push(Slot::Col(name.to_string()));
    slots.len() - 1
}

fn project_at(
    slots: &mut Vec<Slot>,
    projs: &[Proj],
    alias: &Option<String>,
) -> Option<(Vec<usize>, Vec<Option<String>>)> {
    let mut at = Vec::new();
    let mut cols = Vec::new();
    for proj in projs {
        match proj {
            Proj::Col(c) => {
                at.push(ensure_col(slots, c));
                cols.push(Some(c.clone()));
            }
            Proj::Coalesce { col, lit } => {
                let expr = format!("coalesce({}, {})", render_col(alias, col), render_lit(lit)?);
                slots.push(Slot::Expr(expr));
                at.push(slots.len() - 1);
                cols.push(None);
            }
        }
    }
    Some((at, cols))
}
