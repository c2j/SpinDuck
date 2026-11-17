//! 原生写入表达式的类型/值域证明与列级守卫：DECIMAL 减法/abs、数值除法/round、concat。

use super::schema::{Column, Table};
use super::shape::{Name, Select, Tokens};
use super::{BindSrc, SkipReason, VarName};

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
    let mut p = Parser {
        t: &mut t,
        binds: region.binds,
        record: region.record,
        select: source.0,
        source: source.1,
        resolve,
        guards: Vec::new(),
    };
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
    resolve: &'a mut dyn FnMut(&VarName) -> Result<BoundExpr, SkipReason>,
    guards: Vec<String>,
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

    fn difference(self, rhs: Self) -> Result<Self, SkipReason> {
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
            ) if s == t && p.max(q) < 38 => Ok(Self::Decimal {
                precision: p.max(q) + 1,
                scale: s,
            }),
            _ => Err(SkipReason::UnprovenWriteCompleteness),
        }
    }
}

impl Parser<'_> {
    fn expr(&mut self) -> Result<(String, Domain), SkipReason> {
        let (mut out, mut domain) = self.quotient()?;
        while self.t.take("-") {
            let (rhs, rhs_domain) = self.quotient()?;
            domain = domain.difference(rhs_domain)?;
            out = format!("({out} - {rhs})");
        }
        Ok((out, domain))
    }

    fn quotient(&mut self) -> Result<(String, Domain), SkipReason> {
        let (mut out, mut domain) = self.term()?;
        while self.t.take("/") {
            let (rhs, rhs_domain) = self.term()?;
            if !domain.is_numeric() || !rhs_domain.is_numeric() {
                return Err(SkipReason::UnprovenWriteCompleteness);
            }
            out = format!("{out} / {rhs}");
            domain = Domain::Numeric;
        }
        Ok((out, domain))
    }

    fn term(&mut self) -> Result<(String, Domain), SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let word = self.t.peek().ok_or(bad)?.to_string();
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
        if word.starts_with("$v_") {
            return self.binding(&word);
        }
        if numeric_literal(&word) {
            self.t.take(&word);
            return Ok((word, Domain::Numeric));
        }
        Err(bad)
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

    fn binding(&mut self, word: &str) -> Result<(String, Domain), SkipReason> {
        if let Some((_, BindSrc::Var(var))) = self.binds.iter().find(|(p, _)| p == word) {
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
