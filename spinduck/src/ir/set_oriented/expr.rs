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
    /// 守卫可能遮蔽内核域行级错误（DECIMAL 算术/转换值域溢出）：
    /// 解释器按行 INSERT 遇内核错误会 ROLLBACK 全部已写行，批写单
    /// 内核域守卫（DOUBLE 求值、永不抛错）：与 `guard` 分离——坏行
    /// COUNT 必须先用它裁决，否则守卫查询自身求值溢出算术会内核报错。
    pub kernel_guard: String,
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
    let mut p = Parser::new(&mut t, region, select, source, resolve, Some(to));
    let node = p.expr()?;
    if !p.t.done() {
        return Err(SkipReason::UnprovenWriteCompleteness);
    }
    let mut result = at_write_sink(node.sql, to);
    if !p.guards.is_empty() {
        result.guard = format!("({}) AND ({})", p.guards.join(") AND ("), result.guard);
    }
    result.kernel_guard = p.kernel_guards.join(" AND ");
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
    let mut p = Parser::new(&mut t, region, select, source, resolve, None);
    let node = p.expr()?;
    if !p.t.done() || !p.guards.is_empty() || !p.kernel_guards.is_empty() {
        return Err(SkipReason::UnprovenWriteCompleteness);
    }
    Ok(node.sql)
}

pub(super) struct BoundExpr {
    pub value: GuardedExpr,
    pub domain: Domain,
    /// 值的 DOUBLE 域守卫渲染（None = 无法证明守卫求值安全）。
    pub dbl: Option<String>,
    /// `value.expr` 不含可抛错 DECIMAL 算术（守卫 COUNT 可直接求值、
    /// CASE 操作数比较保持精确语义）。
    pub safe: bool,
}

pub(super) fn at_write_sink(expr: String, to: &Column) -> GuardedExpr {
    let ty = to.kind.to_ascii_uppercase();
    let cast = format!("({expr}) IS NULL OR TRY_CAST(({expr}) AS {ty}) IS NOT NULL");
    let guard = if to.nullable {
        cast
    } else {
        format!("({expr}) IS NOT NULL AND ({cast})")
    };
    GuardedExpr {
        expr,
        guard,
        kernel_guard: String::new(),
    }
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
    /// 内核域守卫（DOUBLE 求值）：与 `guards` 分离，供坏行 COUNT 先行裁决。
    kernel_guards: Vec<String>,
    /// 写汇列（prove 路径）：DECIMAL 算术值域溢出守卫的目标值域证明依据；
    /// render_arg 无写汇，恒 None（溢出一律拒绝）。
    to: Option<&'a Column>,
}

/// 解析节点的四路渲染：值域 SQL / 值域 / 安全性 / DOUBLE 域守卫渲染。
/// `dbl` 让外层算术的值域守卫可以对**任意**操作数（含内含 DECIMAL
/// 算术的 Var 绑定）做 DOUBLE 域模长计算——守卫求值永不触发内核
/// 溢出，故不再需要"操作数 total"限制。
struct Node {
    /// 写值/比较用 SQL：可含 DECIMAL 算术（仅批写在准入后求值）。
    sql: String,
    domain: Domain,
    /// `sql` 不含可抛错 DECIMAL 算术：守卫 COUNT 可直接求值，且可作
    /// CASE 操作数（比较须精确语义，DOUBLE 域会引入舍入误判）。
    safe: bool,
    /// DOUBLE 域同义渲染（永不抛错）；None = 无法证明守卫求值安全。
    dbl: Option<String>,
}

impl Node {
    /// 安全叶子的 DOUBLE 域渲染：数值域 CAST（恒成功）、Other 域
    /// TRY_CAST（坏值→NULL，与写汇 TRY_CAST 的 NULL 传播一致；
    /// Other 域本不可入算术，dbl 仅在 CAST 包装后间接参与守卫）。
    fn dbl_for(sql: &str, domain: Domain) -> Option<String> {
        if domain.is_numeric() {
            Some(format!("CAST(({sql}) AS DOUBLE)"))
        } else {
            Some(format!("TRY_CAST(({sql}) AS DOUBLE)"))
        }
    }

    /// 安全数值叶子：sql 可直接进守卫，dbl 取 CAST/TRY_CAST AS DOUBLE。
    fn leaf(sql: String, domain: Domain) -> Self {
        let dbl = Self::dbl_for(&sql, domain);
        Self {
            sql,
            domain,
            safe: true,
            dbl,
        }
    }

    /// 安全非数值叶子（字符串/Other 域）：TRY_CAST DOUBLE 域渲染。
    fn other(sql: String) -> Self {
        Self {
            dbl: Self::dbl_for(&sql, Domain::Other),
            sql,
            domain: Domain::Other,
            safe: true,
        }
    }

    /// 两侧 DOUBLE 域渲染按算符组合；任一缺失则整体缺失。
    fn combine_dbl(l: &Option<String>, r: &Option<String>, op: char) -> Option<String> {
        Some(format!("({} {} {})", l.as_ref()?, op, r.as_ref()?))
    }
}

impl<'a> Parser<'a> {
    fn new(
        t: &'a mut Tokens,
        region: &'a CopyLoop<'a>,
        select: &'a Select,
        source: &'a Table,
        resolve: &'a mut dyn FnMut(&VarName) -> Result<BoundExpr, SkipReason>,
        to: Option<&'a Column>,
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
            kernel_guards: Vec::new(),
            to,
        }
    }
}

#[derive(Clone, Copy, Debug)]
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
    pub(super) fn unify(self, rhs: Self) -> Result<Self, SkipReason> {
        match (self, rhs) {
            // CASE 分支合并是值域并集而非算术：标度取大者、整数位取
            // 各分支最大值，不加 combine 的 +1 进位余量。
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
                let int = (i64::from(p) - i64::from(s)).max(i64::from(q) - i64::from(t));
                let precision = int + i64::from(scale);
                if precision > 38 {
                    return Err(SkipReason::UnprovenWriteCompleteness);
                }
                Ok(Self::Decimal {
                    precision: precision as u8,
                    scale,
                })
            }
            (Self::Numeric, Self::Numeric) => Ok(Self::Numeric),
            (Self::Other, Self::Other) => Ok(Self::Other),
            _ => Err(SkipReason::UnprovenWriteCompleteness),
        }
    }
}

impl Parser<'_> {
    /// 加法层：`+`/`-` 同级左结合，域按 DECIMAL 证明合并。
    fn expr(&mut self) -> Result<Node, SkipReason> {
        let mut lhs = self.product()?;
        loop {
            let subtract = self.t.take("-");
            if !subtract && !self.t.take("+") {
                break;
            }
            let rhs = self.product()?;
            let op = if subtract { '-' } else { '+' };
            match lhs.domain.combine(rhs.domain) {
                Ok(d) => lhs.domain = d,
                Err(reason) => {
                    lhs.domain = self
                        .overflow_guard(&lhs, &rhs, op, lhs.domain, rhs.domain)
                        .ok_or(reason)?;
                }
            }
            lhs.dbl = Node::combine_dbl(&lhs.dbl, &rhs.dbl, op);
            lhs.sql = format!("({} {op} {})", lhs.sql, rhs.sql);
            lhs.safe = false;
        }
        Ok(lhs)
    }

    /// 乘法层：`*`（DECIMAL 域证明）与 `/`（数值域）同级左结合。
    fn product(&mut self) -> Result<Node, SkipReason> {
        let mut lhs = self.term()?;
        loop {
            if self.t.take("*") {
                let rhs = self.term()?;
                match lhs.domain.product(rhs.domain) {
                    Ok(d) => lhs.domain = d,
                    Err(reason) => {
                        lhs.domain = self
                            .overflow_guard(&lhs, &rhs, '*', lhs.domain, rhs.domain)
                            .ok_or(reason)?;
                    }
                }
                lhs.dbl = Node::combine_dbl(&lhs.dbl, &rhs.dbl, '*');
                lhs.sql = format!("({} * {})", lhs.sql, rhs.sql);
                lhs.safe = false;
            } else if self.t.take("/") {
                let rhs = self.term()?;
                if !lhs.domain.is_numeric() || !rhs.domain.is_numeric() {
                    return Err(SkipReason::UnprovenWriteCompleteness);
                }
                // `/` 结果 DOUBLE 域、除零得 inf 不抛错：safe 继承操作数。
                lhs.dbl = Node::combine_dbl(&lhs.dbl, &rhs.dbl, '/');
                lhs.sql = format!("{}/{}", lhs.sql, rhs.sql);
                lhs.domain = Domain::Numeric;
                lhs.safe = lhs.safe && rhs.safe;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    /// DECIMAL `+`/`-`/`*` 静态精度证明超 38 时的值域守卫：内核结果
    /// 类型恒为 DECIMAL(38, scale_res)（`+`/`-` 标度取大者、`*` 标度
    /// 相加且须 ≤36 使整数位 ≥2），仅当 |结果| 越出值域才抛错。
    /// 守卫以操作数的 DOUBLE 域渲染重算模长——不求值 DECIMAL 算术，
    /// 故对含未证算术的 Var 绑定同样安全；越界行在解释器中是内核
    /// 错误（ROLLBACK 全部已写行），批写无法逐行复现，故计入内核域
    /// 坏行守卫要求 bad_rows==0 入口准入。
    /// 一致性前提：目标列整数位 +2 ≤ 内核整数位，保证守卫滤掉的行
    /// 必然也过不了目标 TRY_CAST（语义上本就该跳过/回退）。
    /// 任一操作数缺 DOUBLE 域渲染时无法证明守卫求值安全，保守拒绝。
    fn overflow_guard(
        &mut self,
        lhs: &Node,
        rhs: &Node,
        op: char,
        lhs_domain: Domain,
        rhs_domain: Domain,
    ) -> Option<Domain> {
        let (Domain::Decimal { scale: s, .. }, Domain::Decimal { scale: t, .. }) =
            (lhs_domain, rhs_domain)
        else {
            return None;
        };
        let (Some(dl), Some(dr)) = (&lhs.dbl, &rhs.dbl) else {
            return None;
        };
        let scale_res = match op {
            '+' | '-' => s.max(t),
            '*' => {
                let sum = i64::from(s) + i64::from(t);
                if sum > 36 {
                    return None;
                }
                sum as u8
            }
            _ => return None,
        };
        let kernel_int = 38 - i64::from(scale_res);
        if kernel_int < 2 {
            return None;
        }
        let to = self.to?;
        let kind = to.kind.to_ascii_lowercase();
        let inner = kind
            .strip_prefix("decimal(")
            .and_then(|k| k.strip_suffix(')'))?;
        let (p, s) = inner.split_once(',')?;
        let (Ok(p), Ok(s)) = (p.trim().parse::<i64>(), s.trim().parse::<i64>()) else {
            return None;
        };
        if p - s + 2 > kernel_int {
            return None;
        }
        let mag = match op {
            '+' => format!("abs({dl} + {dr})"),
            '-' => format!("abs({dl} - {dr})"),
            _ => format!("abs({dl} * {dr})"),
        };
        self.kernel_guards.push(format!(
            "(({dl}) IS NULL) OR (({dr}) IS NULL) OR ({mag} < 1e{})",
            kernel_int - 1
        ));
        Some(Domain::Decimal {
            precision: 38,
            scale: scale_res,
        })
    }

    fn term(&mut self) -> Result<Node, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let word = self.t.peek().ok_or(bad)?.to_string();
        if self.t.at_subquery() {
            return self.cache_subquery();
        }
        if self.t.take("(") {
            let inner = self.expr()?;
            return self
                .t
                .take(")")
                .then(|| Node {
                    sql: format!("({})", inner.sql),
                    domain: inner.domain,
                    safe: inner.safe,
                    dbl: inner.dbl,
                })
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
            return Ok(Node::other(word));
        }
        if numeric_literal(&word) {
            self.t.take(&word);
            return Ok(Node::leaf(word.clone(), literal_domain(&word)));
        }
        Err(bad)
    }

    /// 简单式 CASE：`CASE <operand> WHEN <字面量> THEN <expr>… ELSE <expr> END`。
    /// operand/WHEN 只作比较不产出结果域；THEN/ELSE 域按 unify 合并。
    /// 搜索式 CASE（无 operand）不支持，保守拒绝。
    fn case_call(&mut self) -> Result<Node, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        let operand = self.expr()?;
        let mut whens: Vec<(String, Node)> = Vec::new();
        let mut domain: Option<Domain> = None;
        while self.t.take("when") {
            let w = self.when_literal()?;
            if !self.t.take("then") {
                return Err(bad);
            }
            let then = self.expr()?;
            domain = Some(match domain {
                None => then.domain,
                Some(d) => d.unify(then.domain)?,
            });
            whens.push((w, then));
        }
        if whens.is_empty() || !self.t.take("else") {
            return Err(bad);
        }
        let els = self.expr()?;
        if !self.t.take("end") {
            return Err(bad);
        }
        let domain = domain.ok_or(bad)?.unify(els.domain)?;
        // DOUBLE 域 CASE：操作数比较沿用原值域 sql（精确语义），
        // 要求 operand.safe；分支取 DOUBLE 渲染。操作数含未证算术
        // 时无法保证分支选择一致，dbl 缺失 → 外层守卫保守拒绝。
        let mut dbl = None;
        if domain.is_numeric() && operand.safe {
            let mut segs = Vec::new();
            let mut ok = true;
            for (w, b) in &whens {
                match &b.dbl {
                    Some(d) => segs.push(format!("WHEN {w} THEN {d}")),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some(d) = &els.dbl {
                    dbl = Some(format!(
                        "CASE {} {} ELSE {d} END",
                        operand.sql,
                        segs.join(" ")
                    ));
                }
            }
        }
        let safe = operand.safe && whens.iter().all(|(_, b)| b.safe) && els.safe;
        Ok(Node {
            sql: format!(
                "CASE {} {} ELSE {} END",
                operand.sql,
                whens
                    .iter()
                    .map(|(w, b)| format!("WHEN {w} THEN {}", b.sql))
                    .collect::<Vec<_>>()
                    .join(" "),
                els.sql
            ),
            domain,
            safe,
            dbl,
        })
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
    fn cast_call(&mut self) -> Result<Node, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let inner = self.expr()?;
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
        let rendered = format!("TRY_CAST(({}) AS DECIMAL({p}, {s}))", inner.sql);
        self.guards.push(format!(
            "({}) IS NULL OR ({rendered}) IS NOT NULL",
            inner.sql
        ));
        // TRY_CAST 永不抛错；dbl 沿用内层（DECIMAL(p,s) 收窄只减精度，
        // 模长上界不变；TRY_CAST 失败→NULL 方向保守）。
        Ok(Node {
            sql: rendered,
            domain: Domain::Decimal { precision, scale },
            safe: inner.safe,
            dbl: inner.dbl,
        })
    }

    fn abs_call(&mut self) -> Result<Node, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let inner = self.expr()?;
        // DECIMAL 的正负值域对称；整数最小值的 abs 可能溢出，不能照搬。
        if !matches!(inner.domain, Domain::Decimal { .. }) || !self.t.take(")") {
            return Err(bad);
        }
        Ok(Node {
            dbl: inner.dbl.map(|d| format!("abs({d})")),
            sql: format!("abs({})", inner.sql),
            domain: inner.domain,
            safe: inner.safe,
        })
    }

    fn round_call(&mut self) -> Result<Node, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let inner = self.expr()?;
        if !inner.domain.is_numeric() || !self.t.take(",") {
            return Err(bad);
        }
        let k = self.t.number().ok_or(bad)?;
        if !self.t.take(")") {
            return Err(bad);
        }
        Ok(Node {
            dbl: inner.dbl.map(|d| format!("round({d}, {k})")),
            sql: format!("round({}, {k})", inner.sql),
            domain: Domain::Numeric,
            safe: inner.safe,
        })
    }

    /// concat 对白名单标量类型一律安全（隐式 VARCHAR 转换不抛错），
    /// 结果属字符串域；NULL 语义与 Off 的 concat_null_as_empty 改写逐字一致。
    fn concat_call(&mut self) -> Result<Node, SkipReason> {
        let bad = SkipReason::UnprovenWriteCompleteness;
        if !self.t.take("(") {
            return Err(bad);
        }
        let mut args = vec![self.expr()?];
        while self.t.take(",") {
            args.push(self.expr()?);
        }
        if !self.t.take(")") {
            return Err(bad);
        }
        Ok(Node {
            sql: format!(
                "concat({})",
                args.iter()
                    .map(|a| a.sql.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            domain: Domain::Other,
            safe: args.iter().all(|a| a.safe),
            dbl: None,
        })
    }

    /// INSERT 位 fn_site 参数缓存子查询叶：`(SELECT sd_v FROM sd_fnN
    /// WHERE sd_aK IS NOT DISTINCT FROM (<实参>)…)`——表名按已准入
    /// InsertSite 改写 `sd_bfN`，实参递归走表达式语法（`$v_` 占位符
    /// → sd_cur 列 / 行级表达式 / IN 形参占位符）。结果域 VARCHAR。
    fn cache_subquery(&mut self) -> Result<Node, SkipReason> {
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
        let mut args_safe = true;
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
            let arg = self.expr()?;
            if !self.t.take(")") {
                return Err(bad);
            }
            args_safe &= arg.safe;
            conds.push(format!("{} IS NOT DISTINCT FROM ({})", col.sql(), arg.sql));
            if !self.t.take("and") {
                break;
            }
        }
        self.t
            .take(")")
            .then(|| Node {
                sql: format!(
                    "(SELECT sd_v FROM {} WHERE {})",
                    site.table,
                    conds.join(" AND ")
                ),
                domain: Domain::Other,
                safe: args_safe,
                dbl: None,
            })
            .ok_or(bad)
    }

    fn binding(&mut self, word: &str) -> Result<Node, SkipReason> {
        if let Some((_, BindSrc::Var(var))) = self.binds.iter().find(|(p, _)| p == word) {
            // 循环不变 IN 形参：占位符原样保留，运行期随 plan.binds 解析。
            if self.in_params.contains(&var.0.to_ascii_lowercase()) {
                self.t.take(word);
                let domain = type_domain(self.var_types, var);
                return Ok(Node {
                    dbl: Node::dbl_for(word, domain),
                    sql: word.to_string(),
                    domain,
                    safe: true,
                });
            }
            let bound = (self.resolve)(var)?;
            self.t.take(word);
            if !bound.value.guard.is_empty() {
                self.guards.push(bound.value.guard);
            }
            // 绑定值自带的内核域守卫（如 ArithWrite 内部溢出算术）
            // 上浮为本表达式的内核守卫——入口准入统一裁决。
            if !bound.value.kernel_guard.is_empty() {
                self.kernel_guards.push(bound.value.kernel_guard);
            }
            return Ok(Node {
                sql: format!("({})", bound.value.expr),
                domain: bound.domain,
                safe: bound.safe,
                dbl: bound.dbl,
            });
        }
        let (field, domain) = self.bind_field(word)?;
        let col = format!("sd_cur.{}", Name(field).sql());
        Ok(Node {
            dbl: Node::dbl_for(&col, domain),
            sql: col,
            domain,
            safe: true,
        })
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
    GuardedExpr {
        expr,
        guard,
        kernel_guard: String::new(),
    }
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
