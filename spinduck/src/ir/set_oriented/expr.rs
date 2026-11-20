//! 原生写入表达式的类型/值域证明与列级守卫：DECIMAL 减法/abs、数值除法/round、concat。

use super::schema::{Column, Table};
use super::shape::{is_cache_arg_col, Name, Select, Tokens};
use super::{BindSrc, CopyLoop, InsertSite, SkipReason, VarName};
use crate::types::SpinType;
use std::collections::{BTreeMap, BTreeSet};

/// 渲染后的写入表达式及其行级守卫（TRY_CAST 精确转换 + 可空性）。
pub(super) struct GuardedExpr {
    pub expr: String,
    pub guard: String,
}

/// 证明表达式在全部输入上求值不抛错，并生成目标列级守卫。
pub(super) fn prove(
    parameter: &str,
    region: &super::CopyLoop<'_>,
    source: (&Select, &Table),
    to: &Column,
    resolve: &mut impl FnMut(&VarName) -> Result<BoundExpr, SkipReason>,
) -> Result<GuardedExpr, SkipReason> {
    let mut t = Tokens::new(parameter).ok_or(SkipReason::UnprovenWriteCompleteness)?;
    let (select, source) = source;
    let mut p = Parser::new(&mut t, region, select, source, resolve);
    let (expr, _) = p.expr()?;
    if !p.t.done() {
        return Err(SkipReason::UnprovenWriteCompleteness);
    }
    let mut result = at_write_sink(expr, to);
    if !p.guards.is_empty() {
        result.guard = format!("({}) AND ({})", p.guards.join(") AND ("), result.guard);
    }
    Ok(result)
}

/// fn_site distinct 实参渲染：与 [`prove`] 同语法面，但产出裸表达式
/// （无写汇 TRY_CAST 守卫）——供 `SELECT DISTINCT <实参> FROM sd_cur`
/// 前导物化查询使用。解析期登记的行级守卫非空即拒绝（实参错误语义
/// 无法在前导查询中复现，保守退回）。
pub(super) fn render_arg(
    text: &str,
    region: &CopyLoop<'_>,
    select: &Select,
    source: &Table,
    resolve: &mut impl FnMut(&VarName) -> Result<BoundExpr, SkipReason>,
) -> Result<String, SkipReason> {
    let mut t = Tokens::new(text).ok_or(SkipReason::UnprovenWriteCompleteness)?;
    let mut p = Parser::new(&mut t, region, select, source, resolve);
    let (expr, _) = p.expr()?;
    if !p.t.done() || !p.guards.is_empty() {
        return Err(SkipReason::UnprovenWriteCompleteness);
    }
    Ok(expr)
}

pub(super) struct BoundExpr {
    pub value: GuardedExpr,
    pub domain: Domain,
}

pub(super) fn at_write_sink(expr: String, to: &Column) -> GuardedExpr {
    let ty = to.kind.to_ascii_uppercase();
    let cast = format!("({expr}) IS NULL OR TRY_CAST(({expr}) AS {ty}) IS NOT NULL");
    let guard = if to.nullable {
        cast
    } else {
        format!("({expr}) IS NOT NULL AND ({cast})")
    };
    GuardedExpr { expr, guard }
}

/// PL 整数除法下沉（设计 §4.5 情形 A / §4.6）：精确 `//`（禁浮点/TRUNC），
/// PL 除零守卫复现 BIZ-0124 跳行；NULL/0 因 NULL 传播合法保留。
pub(super) fn pl_division(lhs_field: &str, rhs_field: &str, to: &Column) -> GuardedExpr {
    let a = format!("sd_cur.{}", Name(lhs_field.to_string()).sql());
    let b = format!("sd_cur.{}", Name(rhs_field.to_string()).sql());
    let expr = format!("({a} // {b})");
    let zero = format!("({a}) IS NULL OR ({b}) IS NULL OR ({b}) <> 0");
    let ty = to.kind.to_ascii_uppercase();
    let cast = format!("{expr} IS NULL OR TRY_CAST({expr} AS {ty}) IS NOT NULL");
    let mut guard = format!("({zero}) AND ({cast})");
    if !to.nullable {
        guard = format!("{expr} IS NOT NULL AND ({guard})");
    }
    GuardedExpr { expr, guard }
}

struct Parser<'a> {
    t: &'a mut Tokens,
    binds: &'a [(String, BindSrc)],
    record: &'a VarName,
    select: &'a Select,
    source: &'a Table,
    sites: &'a [InsertSite],
    in_params: &'a BTreeSet<String>,
    var_types: &'a BTreeMap<String, SpinType>,
    resolve: &'a mut dyn FnMut(&VarName) -> Result<BoundExpr, SkipReason>,
    guards: Vec<String>,
}

impl<'a> Parser<'a> {
    fn new(
        t: &'a mut Tokens,
        region: &'a CopyLoop<'a>,
        select: &'a Select,
        source: &'a Table,
        resolve: &'a mut dyn FnMut(&VarName) -> Result<BoundExpr, SkipReason>,
    ) -> Self {
        Self {
            t,
            binds: region.binds,
            record: region.record,
            select,
            source,
            sites: &region.insert_sites,
            in_params: &region.in_params,
            var_types: region.var_types,
            resolve,
            guards: Vec::new(),
        }
    }
}

pub(super) enum Domain {
    Numeric,
    Decimal { precision: u8, scale: u8 },
    Other,
}

impl Domain {
    pub(super) fn is_numeric(&self) -> bool {
        !matches!(self, Self::Other)
    }

    pub(super) fn of(kind: &str) -> Self {
        if let Some(body) = kind
            .strip_prefix("decimal(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some((p, s)) = body.split_once(',') {
                if let (Ok(precision), Ok(scale)) = (p.trim().parse(), s.trim().parse()) {
                    return Self::Decimal { precision, scale };
                }
            }
        }
        if integer_kind(kind) || kind == "double" {
            Self::Numeric
        } else {
            Self::Other
        }
    }

    /// `+`/`-`：DECIMAL 按最大标度、整数位+进位证明（s==t 时退化为旧
    /// difference 规则）；非 DECIMAL 混合一律拒绝。
    fn combine(self, rhs: Self) -> Result<Self, SkipReason> {
        match (self, rhs) {
            (
                Self::Decimal {
                    precision: p,
                    scale: s,
                },
                Self::Decimal {
                    precision: q,
                    scale: t,
                },
            ) => {
                let scale = s.max(t);
                let int = (i64::from(p) - i64::from(s)).max(i64::from(q) - i64::from(t)) + 1;
                let precision = int + i64::from(scale);
                if precision > 38 {
                    return Err(SkipReason::UnprovenWriteCompleteness);
                }
                Ok(Self::Decimal {
                    precision: precision as u8,
                    scale,
                })
            }
            _ => Err(SkipReason::UnprovenWriteCompleteness),
        }
    }

    /// `*`：DECIMAL 乘法标度相加、整数位相加，精度超 38 拒绝。
    fn product(self, rhs: Self) -> Result<Self, SkipReason> {
        match (self, rhs) {
            (
                Self::Decimal {
                    precision: p,
                    scale: s,
                },
                Self::Decimal {
                    precision: q,
                    scale: t,
                },
            ) => {
                let scale = i64::from(s) + i64::from(t);
                let int = (i64::from(p) - i64::from(s)) + (i64::from(q) - i64::from(t));
                let precision = int + scale;
                if precision > 38 || scale > 38 {
                    return Err(SkipReason::UnprovenWriteCompleteness);
                }
                Ok(Self::Decimal {
                    precision: precision as u8,
                    scale: scale as u8,
                })
            }
            _ => Err(SkipReason::UnprovenWriteCompleteness),
        }
    }

    /// CASE/多分支结果域：同族合并（DECIMAL 按 combine，Numeric/Other 同类
    /// 直通），跨族拒绝。
    fn unify(self, rhs: Self) -> Result<Self, SkipReason> {
        match (self, rhs) {
            (a @ Self::Decimal { .. }, b @ Self::Decimal { .. }) => a.combine(b),
            (Self::Numeric, Self::Numeric) => Ok(Self::Numeric),
            (Self::Other, Self::Other) => Ok(Self::Other),
            _ => Err(SkipReason::UnprovenWriteCompleteness),
        }
    }
}

impl Parser<'_> {
    /// 加法层：`+`/`-` 同级左结合，域按 DECIMAL 证明合并。
    fn expr(&mut self) -> Result<(String, Domain), SkipReason> {
        let (mut out, mut domain) = self.product()?;
        loop {
            let subtract = self.t.take("-");
            if !subtract && !self.t.take("+") {
                break;
            }
            let (rhs, rhs_domain) = self.product()?;
            domain = domain.combine(rhs_domain)?;
            out = if subtract {
                format!("({out} - {rhs})")
            } else {
                format!("({out} + {rhs})")
            };
        }
        Ok((out, domain))
    }

    /// 乘法层：`*`（DECIMAL 域证明）与 `/`（数值域）同级左结合。
    fn product(&mut self) -> Result<(String, Domain), SkipReason> {
        let (mut out, mut domain) = self.term()?;
        loop {
            if self.t.take("*") {
                let (rhs, rhs_domain) = self.term()?;
                domain = domain.product(rhs_domain)?;
                out = format!("({out} * {rhs})");
            } else if self.t.take("/") {
                let (rhs, rhs_domain) = self.term()?;
                if !domain.is_numeric() || !rhs_domain.is_numeric() {
                    return Err(SkipReason::UnprovenWriteCompleteness);
                }
                out = format!("{out} / {rhs}");
                domain = Domain::Numeric;
            } else {
                break;
            }
        }
        Ok((out, domain))
    }

    fn term(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let word = self.t.peek().ok_or(bad)?.to_string();
        if self.t.at_subquery() {
            return self.cache_subquery();
        }
        if self.t.take("(") {
            let (inner, num) = self.expr()?;
            return self
                .t
                .take(")")
                .then_some((format!("({inner})"), num))
                .ok_or(bad);
        }
        if self.t.take("round") {
            return self.round_call();
        }
        if self.t.take("abs") {
            return self.abs_call();
        }
        if self.t.take("concat") {
            return self.concat_call();
        }
        if self.t.take("case") {
            return self.case_call();
        }
        if self.t.take("cast") {
            return self.cast_call();
        }
        if word.starts_with("$v_") {
            return self.binding(&word);
        }
        if word.starts_with('\'') {
            self.t.take(&word);
            return Ok((word, Domain::Other));
        }
        if numeric_literal(&word) {
            self.t.take(&word);
            return Ok((word.clone(), literal_domain(&word)));
        }
        Err(bad)
    }

    /// 简单式 CASE：`CASE <operand> WHEN <字面量> THEN <expr>… ELSE <expr> END`。
    /// operand/WHEN 只作比较不产出结果域；THEN/ELSE 域按 unify 合并。
    /// 搜索式 CASE（无 operand）不支持，保守拒绝。
    fn case_call(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let (operand, _) = self.expr()?;
        let mut whens = Vec::new();
        let mut domain: Option<Domain> = None;
        while self.t.take("when") {
            let w = self.when_literal()?;
            if !self.t.take("then") {
                return Err(bad);
            }
            let (then_sql, then_domain) = self.expr()?;
            domain = Some(match domain {
                None => then_domain,
                Some(d) => d.unify(then_domain)?,
            });
            whens.push(format!("WHEN {w} THEN {then_sql}"));
        }
        if whens.is_empty() || !self.t.take("else") {
            return Err(bad);
        }
        let (else_sql, else_domain) = self.expr()?;
        if !self.t.take("end") {
            return Err(bad);
        }
        let domain = domain.ok_or(bad)?.unify(else_domain)?;
        Ok((
            format!("CASE {operand} {} ELSE {else_sql} END", whens.join(" ")),
            domain,
        ))
    }

    /// WHEN 值仅允许字符串/数值字面量（比较操作数不作表达式展开）。
    fn when_literal(&mut self) -> Result<String, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let w = self.t.peek().ok_or(bad)?.to_string();
        if !w.starts_with('\'') && !numeric_literal(&w) {
            return Err(bad);
        }
        self.t.take(&w);
        Ok(w)
    }

    /// `CAST(<expr> AS DECIMAL(p,s))`：改写 TRY_CAST + 行级守卫复现
    /// 解释器"转换失败→语句错误→trap 跳过"语义；仅支持 DECIMAL 目标。
    fn cast_call(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let (inner, _) = self.expr()?;
        if !self.t.take("as") || !self.t.take("decimal") || !self.t.take("(") {
            return Err(bad);
        }
        let p = self.t.number().ok_or(bad)?;
        if !self.t.take(",") {
            return Err(bad);
        }
        let s = self.t.number().ok_or(bad)?;
        if !self.t.take(")") || !self.t.take(")") {
            return Err(bad);
        }
        let (Ok(precision), Ok(scale)) = (p.parse::<u8>(), s.parse::<u8>()) else {
            return Err(bad);
        };
        if precision > 38 || scale > precision {
            return Err(bad);
        }
        let rendered = format!("TRY_CAST(({inner}) AS DECIMAL({p}, {s}))");
        self.guards
            .push(format!("({inner}) IS NULL OR ({rendered}) IS NOT NULL"));
        Ok((rendered, Domain::Decimal { precision, scale }))
    }

    fn abs_call(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let ph = self.t.peek().ok_or(bad)?.to_string();
        let (field, domain) = self.bind_field(&ph)?;
        // DECIMAL 的正负值域对称；整数最小值的 abs 可能溢出，不能照搬。
        if !matches!(domain, Domain::Decimal { .. }) || !self.t.take(")") {
            return Err(bad);
        }
        Ok((format!("abs(sd_cur.{})", Name(field).sql()), domain))
    }

    fn round_call(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let (inner, num) = self.expr()?;
        if !num.is_numeric() || !self.t.take(",") {
            return Err(bad);
        }
        let k = self.t.number().ok_or(bad)?;
        self.t
            .take(")")
            .then_some((format!("round({inner}, {k})"), Domain::Numeric))
            .ok_or(bad)
    }

    /// concat 对白名单标量类型一律安全（隐式 VARCHAR 转换不抛错），
    /// 结果属字符串域；NULL 语义与 Off 的 concat_null_as_empty 改写逐字一致。
    fn concat_call(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let (first, _) = self.expr()?;
        let mut args = vec![first];
        while self.t.take(",") {
            let (arg, _) = self.expr()?;
            args.push(arg);
        }
        if !self.t.take(")") {
            return Err(bad);
        }
        Ok((format!("concat({})", args.join(", ")), Domain::Other))
    }

    /// INSERT 位 fn_site 参数缓存子查询叶：`(SELECT sd_v FROM sd_fnN
    /// WHERE sd_aK IS NOT DISTINCT FROM (<实参>)…)`——表名按已准入
    /// InsertSite 改写 `sd_bfN`，实参递归走表达式语法（`$v_` 占位符
    /// → sd_cur 列 / 行级表达式 / IN 形参占位符）。结果域 VARCHAR。
    fn cache_subquery(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !(self.t.take("(")
            && self.t.take("select")
            && self.t.take("sd_v")
            && self.t.take("from"))
        {
            return Err(bad);
        }
        let table = self.t.name().ok_or(bad)?;
        let Some(site) = self.sites.iter().find(|s| s.orig_table == table.0) else {
            return Err(bad);
        };
        if !self.t.take("where") {
            return Err(bad);
        }
        let mut conds = Vec::new();
        loop {
            let col = self.t.name().ok_or(bad)?;
            if !is_cache_arg_col(&col.0) {
                return Err(bad);
            }
            if !(self.t.take("is")
                && self.t.take("not")
                && self.t.take("distinct")
                && self.t.take("from")
                && self.t.take("("))
            {
                return Err(bad);
            }
            let (arg, _) = self.expr()?;
            if !self.t.take(")") {
                return Err(bad);
            }
            conds.push(format!("{} IS NOT DISTINCT FROM ({arg})", col.sql()));
            if !self.t.take("and") {
                break;
            }
        }
        self.t
            .take(")")
            .then(|| {
                (
                    format!(
                        "(SELECT sd_v FROM {} WHERE {})",
                        site.table,
                        conds.join(" AND ")
                    ),
                    Domain::Other,
                )
            })
            .ok_or(bad)
    }

    fn binding(&mut self, word: &str) -> Result<(String, Domain), SkipReason> {
        if let Some((_, BindSrc::Var(var))) = self.binds.iter().find(|(p, _)| p == word) {
            // 循环不变 IN 形参：占位符原样保留，运行期随 plan.binds 解析。
            if self.in_params.contains(&var.0.to_ascii_lowercase()) {
                self.t.take(word);
                return Ok((word.to_string(), type_domain(self.var_types, var)));
            }
            let bound = (self.resolve)(var)?;
            self.t.take(word);
            if !bound.value.guard.is_empty() {
                self.guards.push(bound.value.guard);
            }
            return Ok((format!("({})", bound.value.expr), bound.domain));
        }
        let (field, domain) = self.bind_field(word)?;
        Ok((format!("sd_cur.{}", Name(field).sql()), domain))
    }

    /// 绑定必须解析为游标记录字段；数值域标志供 `/` 的无抛错证明裁决。
    fn bind_field(&mut self, word: &str) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let Some((_, BindSrc::Field { rec, field })) = self.binds.iter().find(|(p, _)| p == word)
        else {
            return Err(bad);
        };
        let field = Name(field.clone());
        if rec != self.record || self.select.columns.iter().filter(|c| **c == field).count() != 1 {
            return Err(bad);
        }
        let src = self.select.source_of(&field).ok_or(bad)?;
        let from = self
            .source
            .columns
            .iter()
            .find(|c| c.name == *src)
            .ok_or(bad)?;
        if !self.t.take(word) {
            return Err(bad);
        }
        Ok((field.0, Domain::of(&from.kind)))
    }
}

/// IN 形参绑定的值域：Varchar→Other（字符串域），数值族→Numeric
/// （DECIMAL 精度不可静态裁决，混合运算自然退回）。
fn type_domain(var_types: &BTreeMap<String, SpinType>, var: &VarName) -> Domain {
    match var_types.get(&var.0) {
        Some(SpinType::Varchar) => Domain::Other,
        _ => Domain::Numeric,
    }
}

/// PL Concat 下沉（设计 §4.6）：解释器双侧 NULL→NULL、单侧 NULL 视作空串，
/// 与原生 SQL 面 concat_null_as_empty（全 NULL→''）不同，必须双 NULL CASE。
pub(super) fn pl_concat(lhs_field: &str, rhs_field: &str, to: &Column) -> GuardedExpr {
    let a = format!("sd_cur.{}", Name(lhs_field.to_string()).sql());
    let b = format!("sd_cur.{}", Name(rhs_field.to_string()).sql());
    let expr =
        format!("CASE WHEN ({a}) IS NULL AND ({b}) IS NULL THEN NULL ELSE concat({a}, {b}) END");
    let ty = to.kind.to_ascii_uppercase();
    let cast = format!("{expr} IS NULL OR TRY_CAST({expr} AS {ty}) IS NOT NULL");
    let guard = if to.nullable {
        cast
    } else {
        format!("{expr} IS NOT NULL AND ({cast})")
    };
    GuardedExpr { expr, guard }
}

/// PL Int/Int 除法等价前提：源列必须是整数族（SpinValue::Int 域）。
pub(super) fn integer_kind(kind: &str) -> bool {
    matches!(kind, "bigint" | "integer" | "smallint" | "tinyint")
}

/// Concat 操作数白名单：as_str_loose 与 DuckDB →VARCHAR 渲染逐字一致的
/// 类型（varchar/整数族/定宽 decimal）；DOUBLE/BOOLEAN 渲染口径未证，拒绝。
pub(super) fn concat_kind(kind: &str) -> bool {
    kind == "varchar" || integer_kind(kind) || kind.starts_with("decimal(")
}

fn numeric_literal(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|c| c.is_ascii_digit() || c == '.')
        && word.chars().filter(|c| *c == '.').count() <= 1
        && word.chars().any(|c| c.is_ascii_digit())
}

/// 数值字面量的 DECIMAL 域：整数位/小数位按词法计数（值域上界证明，
/// 非常量折叠）；位数超 38 归 Other（下游合并自然拒绝）。
fn literal_domain(word: &str) -> Domain {
    let (int_part, frac_part) = word.split_once('.').unwrap_or((word, ""));
    let int_digits = int_part.trim_start_matches('0').len();
    let precision = (int_digits + frac_part.len()).max(1);
    if precision > 38 {
        return Domain::Other;
    }
    Domain::Decimal {
        precision: precision as u8,
        scale: frac_part.len() as u8,
    }
}
