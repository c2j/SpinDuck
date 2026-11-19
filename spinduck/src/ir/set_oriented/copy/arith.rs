//! §3.3 条件算术覆写渲染：IF 门内 `var := <DECIMAL 乘/减>` 投影为外层
//! `CASE WHEN <cond> THEN TRY_CAST(<arith>) ELSE TRY_CAST(<原值>) END`。
//! 精度模型与 `runtime/eval/numeric.rs` 一致：乘 scale=sa+sb、
//! 减 scale=max(sa,sb) 对齐、结果宽度 ≤38 否则计划期 UnprovenExpression。

use super::inline::ProjEnv;
use super::{expr, group_hits, register_lookup, source_column};
use super::{Column, CopyLoop, Name, SkipReason};
use crate::ir::{BinOp, IrExpr};
use crate::sql::render::const_to_sql;
use crate::value::SpinValue;

use super::super::{ArithWrite, VarName};

/// DECIMAL 值域：整数位数 + 标度（与 numeric.rs 的 (unscaled, scale) 对偶）。
#[derive(Clone, Copy)]
struct Dec {
    int: u32,
    scale: u32,
}

impl Dec {
    /// DECIMAL(p,s) 列 → 值域；p>38 或 s>p 不可证，拒绝。
    fn of_parts(precision: u8, scale: u8) -> Result<Self, SkipReason> {
        if precision > 38 || scale > precision {
            return Err(SkipReason::UnprovenExpression);
        }
        Ok(Self {
            int: u32::from(precision - scale),
            scale: u32::from(scale),
        })
    }

    /// 乘法：scale 相加，整数位 = 两侧整数位 + 1（符号/进位余量），
    /// 宽度 int+scale > 38 即计划期拒绝（不留运行期溢出）。
    fn mul(self, rhs: Self) -> Result<Self, SkipReason> {
        Self::combine(self.int + rhs.int + 1, self.scale + rhs.scale)
    }

    /// 减法：scale 取大者对齐（numeric.rs 同口径），整数位 +1。
    fn sub(self, rhs: Self) -> Result<Self, SkipReason> {
        Self::combine(self.int.max(rhs.int) + 1, self.scale.max(rhs.scale))
    }

    fn combine(int: u32, scale: u32) -> Result<Self, SkipReason> {
        if int + scale > 38 {
            return Err(SkipReason::UnprovenExpression);
        }
        Ok(Self { int, scale })
    }
}

/// 条件算术覆写投影：`CASE WHEN cond THEN arith ELSE 原查找值 END`，
/// 两侧写汇守卫按条件分支合成（CASE in WHERE，逐行取所选分支守卫）。
pub(super) fn projection(
    region: &CopyLoop<'_>,
    write: &ArithWrite,
    to: &Column,
    env: &mut ProjEnv<'_>,
) -> Result<expr::GuardedExpr, SkipReason> {
    let lq = env
        .lookups
        .iter()
        .find(|l| l.var == write.var)
        .ok_or(SkipReason::UnprovenExpression)?;
    let base = super::lookup_projection(region, lq, to, env)?;
    let cond = cond_sql(&write.cond, region, env)?;
    let (arith, _) = render_expr(&write.expr, region, env)?;
    let arith_guard = expr::at_write_sink(arith.clone(), to);
    let ty = &to.kind;
    let base_guard = if base.guard.is_empty() {
        "TRUE".to_string()
    } else {
        base.guard
    };
    Ok(expr::GuardedExpr {
        expr: format!(
            "CASE WHEN {cond} THEN TRY_CAST(({arith}) AS {ty}) \
             ELSE TRY_CAST(({}) AS {ty}) END",
            base.expr
        ),
        guard: format!(
            "CASE WHEN {cond} THEN ({}) ELSE ({}) END",
            arith_guard.guard, base_guard
        ),
    })
}

/// 条件 → SQL：Field/Var/Const 叶子与比较/逻辑算子；Var 须为查找变量。
pub(super) fn cond_sql(
    cond: &IrExpr,
    region: &CopyLoop<'_>,
    env: &mut ProjEnv<'_>,
) -> Result<String, SkipReason> {
    match cond {
        IrExpr::Const(c) => const_to_sql(c).ok_or(SkipReason::UnprovenExpression),
        IrExpr::Field { rec, field } => {
            if rec != region.record {
                return Err(SkipReason::UnprovenExpression);
            }
            let name = Name(field.clone());
            source_column(env.select, env.source, &name)?;
            Ok(format!("sd_cur.{}", name.sql()))
        }
        IrExpr::Var(v) => lookup_case(v, env),
        IrExpr::Not(e) => Ok(format!("NOT ({})", cond_sql(e, region, env)?)),
        IrExpr::Bin { op, l, r } => {
            let op_sql = match op {
                BinOp::And => "AND",
                BinOp::Or => "OR",
                BinOp::Eq => "=",
                BinOp::Ne => "<>",
                BinOp::Lt => "<",
                BinOp::Le => "<=",
                BinOp::Gt => ">",
                BinOp::Ge => ">=",
                _ => return Err(SkipReason::UnprovenExpression),
            };
            Ok(format!(
                "({} {op_sql} {})",
                cond_sql(l, region, env)?,
                cond_sql(r, region, env)?
            ))
        }
        _ => Err(SkipReason::UnprovenExpression),
    }
}

/// 乘/减表达式 → (SQL, 值域)；叶子限记录字段/查找变量/数值常量。
fn render_expr(
    expr: &IrExpr,
    region: &CopyLoop<'_>,
    env: &mut ProjEnv<'_>,
) -> Result<(String, Dec), SkipReason> {
    match expr {
        IrExpr::Const(c) => const_dec(c),
        IrExpr::Var(v) => var_dec(v, env),
        IrExpr::Field { rec, field } => field_dec(region, env, rec, field),
        IrExpr::Bin { op, l, r } => {
            let (ls, ld) = render_expr(l, region, env)?;
            let (rs, rd) = render_expr(r, region, env)?;
            let d = match op {
                BinOp::Mul => ld.mul(rd)?,
                BinOp::Sub => ld.sub(rd)?,
                _ => return Err(SkipReason::UnprovenExpression),
            };
            let op_sql = if matches!(op, BinOp::Mul) { "*" } else { "-" };
            Ok((format!("({ls} {op_sql} {rs})"), d))
        }
        _ => Err(SkipReason::UnprovenExpression),
    }
}

/// 数值常量 → (字面量, 值域)；Int 按十进制位数、Decimal 按声明精度保守取。
fn const_dec(c: &SpinValue) -> Result<(String, Dec), SkipReason> {
    let sql = const_to_sql(c).ok_or(SkipReason::UnprovenExpression)?;
    let d = match c {
        SpinValue::Int(i) => Dec {
            int: i.unsigned_abs().to_string().len() as u32,
            scale: 0,
        },
        SpinValue::Decimal(_, p, s) => Dec::of_parts(*p, *s)?,
        _ => return Err(SkipReason::UnprovenExpression),
    };
    Ok((sql, d))
}

/// 游标字段 → (`sd_cur."f"`, DECIMAL 值域)；非 DECIMAL 列不可证。
fn field_dec(
    region: &CopyLoop<'_>,
    env: &mut ProjEnv<'_>,
    rec: &VarName,
    field: &str,
) -> Result<(String, Dec), SkipReason> {
    if rec != region.record {
        return Err(SkipReason::UnprovenExpression);
    }
    let name = Name(field.to_string());
    let col = source_column(env.select, env.source, &name)?;
    let (p, s) = super::case::decimal(&col.kind).ok_or(SkipReason::UnprovenExpression)?;
    Ok((format!("sd_cur.{}", name.sql()), Dec::of_parts(p, s)?))
}

/// 查找变量 → (`CASE WHEN hits THEN value ELSE default END`, DECIMAL 值域)。
fn var_dec(v: &VarName, env: &mut ProjEnv<'_>) -> Result<(String, Dec), SkipReason> {
    let lq = env
        .lookups
        .iter()
        .find(|l| &l.var == v)
        .ok_or(SkipReason::UnprovenExpression)?;
    let (p, s) = super::case::decimal(&lq.kind).ok_or(SkipReason::UnprovenExpression)?;
    let d = Dec::of_parts(p, s)?;
    Ok((lookup_case(v, env)?, d))
}

/// 查找变量 → 逐行代入值表达式（命中取值/落空兜底）；
/// 跳行查找与 CASE 写汇不是普通值来源，拒绝。
fn lookup_case(v: &VarName, env: &mut ProjEnv<'_>) -> Result<String, SkipReason> {
    let lq = env
        .lookups
        .iter()
        .find(|l| &l.var == v)
        .ok_or(SkipReason::UnprovenExpression)?;
    if lq.filter_on_miss || lq.case_sink.is_some() {
        return Err(SkipReason::UnprovenExpression);
    }
    let members: Vec<_> = env.lookups.iter().filter(|l| l.group == lq.group).collect();
    let hits = group_hits(&members, env.select, env.source)?;
    for member in &members {
        register_lookup(member, env.lookups, env.ctes, env.joins);
    }
    Ok(format!(
        "CASE WHEN {hits} THEN {} ELSE {} END",
        lq.render.value, lq.render.default_sql
    ))
}
