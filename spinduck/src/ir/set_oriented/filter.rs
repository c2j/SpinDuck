//! 游标 WHERE 过滤式的无抛错证明（设计 §4.5 查询全定义）：白名单文法 +
//! 值域匹配。任何无法证明全定义的形态（隐式转换可能抛错、相关子查询、
//! 非白名单函数）一律退回 UnprovenQueryTotalness，绝不原样穿透。
//!
//! 文法：`filter := or`，`or := and ('or' and)*`，`and := unary ('and' unary)*`，
//! `unary := 'not' unary | '(' or ')' | predicate`，
//! `predicate := operand (比较 operand | 'in' '(' operand (',' operand)* ')'
//! | 'between' operand 'and' operand | 'is' ['not'] 'null')`。
//! operand := substr/substring(str, 整数, 整数) | [限定符 '.'] 源列 | 绑定 | 字面量。
//! 比较/IN/BETWEEN 两侧值域必须一致且可证（str↔varchar、num↔数值族）。

use super::schema::Table;
use super::shape::{is_cache_arg_col, Name, Tokens};
use super::{BindSrc, SkipReason};
use crate::types::SpinType;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dom {
    Str,
    Num,
    Unknown,
}

/// 证明过滤式在全部输入上求值不抛错；通过则原文透传至批写 SQL。
/// `fn_tables` 为本区域已准入 fn_site 的缓存表名集——WHERE 中形如
/// `(SELECT sd_v FROM sd_fnN WHERE sd_aK IS NOT DISTINCT FROM (<实参>)…)`
/// 的参数缓存子查询按 Dom::Str 操作数接纳，其内部实参递归要求全定义
/// （列/绑定/字面量/substr/嵌套缓存子查询）；表名不在集合内一律拒绝。
pub(super) fn prove(
    filter: &str,
    source: &Table,
    table: &Name,
    alias: &Option<Name>,
    binds: &[(String, BindSrc)],
    var_types: &BTreeMap<String, SpinType>,
    fn_tables: &[String],
) -> Result<(), SkipReason> {
    let t = Tokens::new(filter).ok_or(SkipReason::UnprovenQueryTotalness)?;
    let mut p = Prover {
        t,
        source,
        table,
        alias,
        binds,
        var_types,
        fn_tables,
    };
    p.or_expr()?;
    if !p.t.done() {
        return Err(SkipReason::UnprovenQueryTotalness);
    }
    Ok(())
}

struct Prover<'a> {
    t: Tokens,
    source: &'a Table,
    table: &'a Name,
    alias: &'a Option<Name>,
    binds: &'a [(String, BindSrc)],
    var_types: &'a BTreeMap<String, SpinType>,
    fn_tables: &'a [String],
}

impl Prover<'_> {
    fn or_expr(&mut self) -> Result<(), SkipReason> {
        self.and_expr()?;
        while self.t.take("or") {
            self.and_expr()?;
        }
        Ok(())
    }

    fn and_expr(&mut self) -> Result<(), SkipReason> {
        self.unary()?;
        while self.t.take("and") {
            self.unary()?;
        }
        Ok(())
    }

    fn unary(&mut self) -> Result<(), SkipReason> {
        let bad = SkipReason::UnprovenQueryTotalness;
        if self.t.take("not") {
            return self.unary();
        }
        // `(SELECT …)` 是操作数而非分组——交给 predicate→operand 解析。
        if !self.t.at_subquery() && self.t.take("(") {
            self.or_expr()?;
            return self.t.take(")").then_some(()).ok_or(bad);
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<(), SkipReason> {
        let bad = SkipReason::UnprovenQueryTotalness;
        let l = self.operand()?;
        if self.t.take("in") {
            if !self.t.take("(") {
                return Err(bad);
            }
            loop {
                let d = self.operand()?;
                if l == Dom::Unknown || d != l {
                    return Err(bad);
                }
                if !self.t.take(",") {
                    break;
                }
            }
            return self.t.take(")").then_some(()).ok_or(bad);
        }
        if self.t.take("between") {
            let a = self.operand()?;
            if !self.t.take("and") {
                return Err(bad);
            }
            let b = self.operand()?;
            if l == Dom::Unknown || a != l || b != l {
                return Err(bad);
            }
            return Ok(());
        }
        if self.t.take("is") {
            self.t.take("not");
            return self.t.take("null").then_some(()).ok_or(bad);
        }
        let op_ok = ["=", "<>", "!=", ">", "<", ">=", "<="]
            .iter()
            .any(|o| self.t.take(o));
        if !op_ok {
            return Err(bad);
        }
        let r = self.operand()?;
        if l == Dom::Unknown || r != l {
            return Err(bad);
        }
        Ok(())
    }

    fn operand(&mut self) -> Result<Dom, SkipReason> {
        let bad = SkipReason::UnprovenQueryTotalness;
        if self.t.at_subquery() {
            self.t.take("(");
            return self.cache_subquery();
        }
        let word = self.t.peek().ok_or(bad)?.to_string();
        if self.t.take("substr") || self.t.take("substring") {
            return self.substr_call();
        }
        if word.starts_with('$') {
            self.t.take(&word);
            let Some((_, src)) = self.binds.iter().find(|(p, _)| p == &word) else {
                return Err(bad);
            };
            return Ok(self.bind_domain(src));
        }
        if word.starts_with('\'') {
            self.t.take(&word);
            return Ok(Dom::Str);
        }
        if numeric_literal(&word) {
            self.t.take(&word);
            return Ok(Dom::Num);
        }
        let first = self.t.name().ok_or(bad)?;
        let col = if self.t.take(".") {
            let qual_ok = match self.alias {
                Some(a) => &first == a,
                None => &first == self.table,
            };
            if !qual_ok {
                return Err(bad);
            }
            self.t.name().ok_or(bad)?
        } else {
            first
        };
        let from = self
            .source
            .columns
            .iter()
            .find(|c| c.name == col)
            .ok_or(bad)?;
        Ok(kind_domain(&from.kind))
    }

    /// fn_site 参数缓存子查询（`(` 已消费）：
    /// `SELECT sd_v FROM <已准入缓存表> WHERE sd_aK IS NOT DISTINCT FROM (<实参>)…)`。
    /// 实参只允许全定义操作数（含嵌套缓存子查询）；返回 Dom::Str（sd_v 恒 VARCHAR）。
    fn cache_subquery(&mut self) -> Result<Dom, SkipReason> {
        let bad = SkipReason::UnprovenQueryTotalness;
        if !self.t.take("select") || !self.t.take("sd_v") || !self.t.take("from") {
            return Err(bad);
        }
        let table = self.t.name().ok_or(bad)?;
        if !self.fn_tables.iter().any(|f| f == &table.0) {
            return Err(bad);
        }
        if !self.t.take("where") {
            return Err(bad);
        }
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
            self.arg_operand()?;
            if !self.t.take(")") {
                return Err(bad);
            }
            if !self.t.take("and") {
                break;
            }
        }
        self.t.take(")").then_some(Dom::Str).ok_or(bad)
    }

    /// 缓存子查询实参：全定义操作数——裸操作数、括号嵌套或嵌套缓存子查询。
    fn arg_operand(&mut self) -> Result<(), SkipReason> {
        let bad = SkipReason::UnprovenQueryTotalness;
        if self.t.at_subquery() {
            self.t.take("(");
            self.cache_subquery()?;
            return Ok(());
        }
        if self.t.take("(") {
            self.arg_operand()?;
            return self.t.take(")").then_some(()).ok_or(bad);
        }
        self.operand().map(|_| ())
    }

    /// substr/substring 定宽截断（同语义，1-indexed）：首参必须字符串域，
    /// 起始 ≥1、长度 ≥0 的整数字面量。
    fn substr_call(&mut self) -> Result<Dom, SkipReason> {
        let bad = SkipReason::UnprovenQueryTotalness;
        if !self.t.take("(") {
            return Err(bad);
        }
        if self.operand()? != Dom::Str {
            return Err(bad);
        }
        if !self.t.take(",") {
            return Err(bad);
        }
        let start = self.t.number().ok_or(bad)?;
        if !self.t.take(",") {
            return Err(bad);
        }
        let len = self.t.number().ok_or(bad)?;
        if !self.t.take(")") {
            return Err(bad);
        }
        if start.parse::<i64>().is_ok_and(|v| v < 1) || len.parse::<i64>().is_ok_and(|v| v < 0) {
            return Err(bad);
        }
        Ok(Dom::Str)
    }

    /// 绑定值域：形参按 var_types 裁决；记录字段绑定属相关引用，拒绝。
    fn bind_domain(&self, src: &BindSrc) -> Dom {
        match src {
            BindSrc::Var(v) => match self.var_types.get(&v.0) {
                Some(SpinType::Varchar) => Dom::Str,
                Some(SpinType::Int | SpinType::BigInt | SpinType::Decimal { .. }) => Dom::Num,
                _ => Dom::Unknown,
            },
            BindSrc::Field { .. } => Dom::Unknown,
        }
    }
}

fn kind_domain(kind: &str) -> Dom {
    if kind == "varchar" {
        Dom::Str
    } else if matches!(
        kind,
        "bigint" | "integer" | "smallint" | "tinyint" | "double"
    ) || kind.starts_with("decimal(")
    {
        Dom::Num
    } else {
        Dom::Unknown
    }
}

fn numeric_literal(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|c| c.is_ascii_digit() || c == '.')
        && word.chars().filter(|c| *c == '.').count() <= 1
        && word.chars().any(|c| c.is_ascii_digit())
}
