//! 残余合取项的全定义谓词证明（issue #65 审核 B1）。
//!
//! 聚合预计算把残余谓词应用到维度表**全部行**，而原逐行查询只在
//! 键命中行上求值——残余里任何执行期可错形态（算术溢出、转换失败、
//! 除零、函数副作用）都会让预计算中止事务、改变业务结果。本模块把
//! 残余收敛到可证 total 的文法：比较 / `IS [NOT] NULL` / `BETWEEN` /
//! `IN` 列表与布尔连接，操作数仅列、字面量、`$v_` 不变量绑定或 NULL；
//! 逐对操作数做类型相容证明（列↔列同 kind 或同数值域，列↔字面量按
//! kind 白名单，`$v_` 绑定登记运行期复核项）。任一不可证形态返回
//! `None`，调用方整站禁用缓存。

use super::super::schema::Table;
use super::super::shape::{Name, Select};
use crate::ir::{BindSrc, VarName};
use std::collections::{BTreeSet, HashMap};

/// 残余合取项分析的共享上下文。
pub(super) struct ResidualCtx<'a> {
    /// `$v_` 占位符 → 绑定源（与 IR binds 同表）。
    pub bind_map: &'a HashMap<&'a str, &'a BindSrc>,
    /// 区域内被写变量：出现在残余里的 `$v_` 必须是循环不变量。
    pub written_vars: &'a BTreeSet<VarName>,
    /// 维度表结构证据（列 kind 查找）。
    pub dim: &'a Table,
    /// 站点 SELECT 形态（表名/别名限定符校验）。
    pub sel: &'a Select,
}

/// 单个残余合取项的证明产物。
#[derive(Default)]
pub(super) struct ResidualOk {
    /// 合取项引用的 `$v_` 不变量占位符（去重、保序）。
    pub inv_phs: Vec<String>,
    /// 运行期类型复核项：`(占位符, 列 kind)`——构建前按绑定值
    /// `SpinValue` 变体复核，不相容即禁用（不发 SQL，事务安全）。
    pub typed_binds: Vec<(String, String)>,
}

/// 字面量类型（词法形态判定）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lit {
    Str,
    Int,
    Dec,
    Bool,
}

/// 谓词操作数。
enum Opd {
    /// 维度表列（`kind` 为 schema 证据）。
    Col {
        kind: String,
    },
    Lit(Lit),
    /// `$v_` 不变量绑定占位符（类型延迟到构建期复核）。
    Bind(String),
    Null,
}

/// 分析一个残余合取项；不可证返回 `None`。
pub(super) fn check(conj: &[String], ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<()> {
    let mut p = PredParser { w: conj, i: 0 };
    p.parse_or(ctx, out)?;
    (p.i == conj.len()).then_some(())
}

struct PredParser<'a> {
    w: &'a [String],
    i: usize,
}

impl PredParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.w.get(self.i).map(String::as_str)
    }

    fn eat(&mut self, word: &str) -> bool {
        if self.peek() == Some(word) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// `or_expr := and_expr (OR and_expr)*`
    fn parse_or(&mut self, ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<()> {
        self.parse_and(ctx, out)?;
        while self.eat("or") {
            self.parse_and(ctx, out)?;
        }
        Some(())
    }

    /// `and_expr := unary (AND unary)*`（合取项内部的 AND 只可能
    /// 出现在括号内；顶层已由 `split_conjuncts` 切分）。
    fn parse_and(&mut self, ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<()> {
        self.parse_unary(ctx, out)?;
        while self.eat("and") {
            self.parse_unary(ctx, out)?;
        }
        Some(())
    }

    /// `unary := [NOT] atom`
    fn parse_unary(&mut self, ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<()> {
        let _ = self.eat("not");
        self.parse_atom(ctx, out)
    }

    /// `atom := '(' or_expr ')' | predicate`
    fn parse_atom(&mut self, ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<()> {
        if self.eat("(") {
            self.parse_or(ctx, out)?;
            return self.eat(")").then_some(());
        }
        self.parse_predicate(ctx, out)
    }

    /// `predicate := operand (IS [NOT] NULL | [NOT] BETWEEN opd AND opd |
    ///   [NOT] IN '(' opd (, opd)* ')' | cmp opd)`
    fn parse_predicate(&mut self, ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<()> {
        let left = self.operand(ctx, out)?;
        if self.eat("is") {
            let _ = self.eat("not");
            return self.eat("null").then_some(());
        }
        let neg = self.eat("not");
        if self.eat("between") {
            let lo = self.operand(ctx, out)?;
            if !self.eat("and") {
                return None;
            }
            let hi = self.operand(ctx, out)?;
            check_pair(&left, &lo, out)?;
            check_pair(&left, &hi, out)?;
            let _ = neg;
            return Some(());
        }
        if self.eat("in") {
            if !self.eat("(") {
                return None;
            }
            loop {
                let item = self.operand(ctx, out)?;
                check_pair(&left, &item, out)?;
                if self.eat(")") {
                    break;
                }
                if !self.eat(",") {
                    return None;
                }
            }
            let _ = neg;
            return Some(());
        }
        if neg {
            return None;
        }
        let op = self.peek()?;
        if !matches!(op, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
            return None;
        }
        self.i += 1;
        let right = self.operand(ctx, out)?;
        check_pair(&left, &right, out)
    }

    /// `operand := [qual '.'] col | literal | $v_bind | NULL`
    fn operand(&mut self, ctx: &ResidualCtx<'_>, out: &mut ResidualOk) -> Option<Opd> {
        let w = self.w.get(self.i)?.clone();
        if w == "null" {
            self.i += 1;
            return Some(Opd::Null);
        }
        if w == "true" || w == "false" {
            self.i += 1;
            return Some(Opd::Lit(Lit::Bool));
        }
        if w.starts_with("$v_") {
            self.i += 1;
            return self.bind_operand(&w, ctx, out);
        }
        if let Some(lit) = literal(&w) {
            self.i += 1;
            return Some(Opd::Lit(lit));
        }
        self.column_operand(ctx)
    }

    /// `$v_` 操作数：必须是区域不变量 Var 绑定；Field/Expr/区域内
    /// 被写变量一律不可证（逐行动值不能进聚合扫描）。
    fn bind_operand(
        &mut self,
        ph: &str,
        ctx: &ResidualCtx<'_>,
        out: &mut ResidualOk,
    ) -> Option<Opd> {
        match ctx.bind_map.get(ph) {
            Some(BindSrc::Var(v)) if !ctx.written_vars.contains(v) => {
                if !out.inv_phs.iter().any(|p| p == ph) {
                    out.inv_phs.push(ph.to_string());
                }
                Some(Opd::Bind(ph.to_string()))
            }
            _ => None,
        }
    }

    /// 列操作数：`[限定符 '.'] 名`；限定符必须是表名或别名，
    /// 列必须存在于维度表（kind 证据）。
    fn column_operand(&mut self, ctx: &ResidualCtx<'_>) -> Option<Opd> {
        let first = self.w.get(self.i)?.clone();
        let name = if first.starts_with('"') && first.ends_with('"') && first.len() >= 2 {
            first[1..first.len() - 1].replace("\"\"", "\"")
        } else if is_ident(&first) {
            first
        } else {
            return None;
        };
        self.i += 1;
        let col = if self.peek() == Some(".") {
            self.i += 1;
            let qual = name;
            let ok = qual == ctx.sel.table.0 || ctx.sel.alias.as_ref().is_some_and(|a| a.0 == qual);
            if !ok {
                return None;
            }
            let c = self.w.get(self.i)?.clone();
            self.i += 1;
            c
        } else {
            name
        };
        let kind = column_kind(ctx.dim, &Name(col))?;
        Some(Opd::Col { kind })
    }
}

/// 操作数对类型相容证明；`$v_` 绑定登记运行期复核项。
fn check_pair(a: &Opd, b: &Opd, out: &mut ResidualOk) -> Option<()> {
    use Opd::*;
    if matches!(a, Null) || matches!(b, Null) {
        return Some(());
    }
    match (a, b) {
        (Col { kind: k1, .. }, Col { kind: k2, .. }) => {
            (k1 == k2 || (numeric_kind(k1) && numeric_kind(k2))).then_some(())
        }
        (Col { kind, .. }, Lit(l)) | (Lit(l), Col { kind, .. }) => lit_ok(*l, kind).then_some(()),
        (Col { kind, .. }, Bind(ph)) | (Bind(ph), Col { kind, .. }) => {
            out.typed_binds.push((ph.clone(), kind.clone()));
            Some(())
        }
        _ => None,
    }
}

/// 字面量 ↔ 列 kind 白名单：Str 仅对 varchar；数值仅对数值域；
/// Bool 仅对 boolean。其余组合存在执行期转换错误面，禁用。
fn lit_ok(lit: Lit, kind: &str) -> bool {
    match lit {
        Lit::Str => kind == "varchar",
        Lit::Int | Lit::Dec => numeric_kind(kind),
        Lit::Bool => kind == "boolean",
    }
}

/// 数值域 kind：整数族 + double + decimal(p,s)。
fn numeric_kind(kind: &str) -> bool {
    matches!(
        kind,
        "bigint" | "integer" | "smallint" | "tinyint" | "double"
    ) || kind.starts_with("decimal(")
}

/// 运行期绑定值 ↔ 列 kind 复核（构建前调用；`SpinValue` 变体口径）。
pub(super) fn runtime_ok(value: &crate::value::SpinValue, kind: &str) -> bool {
    use crate::value::SpinValue as V;
    match value {
        V::Null => true,
        V::Bool(_) => kind == "boolean",
        V::Int(_) | V::Decimal(..) => numeric_kind(kind),
        V::Str(_) => kind == "varchar",
        V::Blob(_) | V::Record(_) => false,
    }
}

/// 词法字面量分类：`'..'` 串、整数、小数/科学计数；其余 None。
fn literal(w: &str) -> Option<Lit> {
    if w.starts_with('\'') {
        return Some(Lit::Str);
    }
    if w.parse::<i64>().is_ok() {
        return Some(Lit::Int);
    }
    if w.parse::<f64>().is_ok() {
        return Some(Lit::Dec);
    }
    None
}

/// 与 `Tokens::name` 同规则的裸标识符判定。
fn is_ident(w: &str) -> bool {
    let mut chars = w.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn column_kind(table: &Table, col: &Name) -> Option<String> {
    table
        .columns
        .iter()
        .find(|c| c.name == *col)
        .map(|c| c.kind.clone())
}
