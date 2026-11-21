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
        kernel_guard: String::new(),
    })
}

/// ArithWrite 覆写变量作为表达式操作数的值绑定：
/// `CASE WHEN cond THEN arith ELSE 原查找值 END`（不含写汇 TRY_CAST
/// ——目标收窄由外层表达式自身守卫裁决）。cond 为比较/逻辑白名单
/// 渲染、恒安全；arith 子树的溢出守卫经 `render_expr`→`guarded_bin`
/// 自动上浮到 `env.kernel_guards`。dbl 为 DOUBLE 域同义渲染：
/// 分支选择与值域 sql 一致（cond/操作数均安全），供外层值域守卫
/// 对含 DECIMAL 算术的绑定做模长计算。
pub(super) fn value_binding(
    region: &CopyLoop<'_>,
    write: &ArithWrite,
    env: &mut ProjEnv<'_>,
) -> Result<expr::BoundExpr, SkipReason> {
    let Some(lq) = env.lookups.iter().find(|l| l.var == write.var) else {
        return Err(SkipReason::UnprovenExpression);
    };
    let cond = cond_sql(&write.cond, region, env)?;
    let (arith, arith_dec) = render_expr(&write.expr, region, env)?;
    let members: Vec<_> = env.lookups.iter().filter(|l| l.group == lq.group).collect();
    let hits = group_hits(&members, env.select, env.source)?;
    for member in &members {
        register_lookup(member, env.lookups, env.ctes, env.joins);
    }
    // 基值三分支与 lookup_projection 同口径：CASE 写汇→sink 值级
    // 渲染（合成目标列 kind=lq.kind，其 TRY_CAST 即查找自身精度）；
    // 跳行查找→hits 行过滤守卫；兜底查找→命中/落空 CASE。
    // 公共标度 = 算术结果标度（CASE 取最小标度会把算术分支舍入
    // 丢精度——解释器保留全精度，必须对齐到算术侧）。基值标度大于
    // 算术标度时无法无损对齐，保守拒绝。
    let arith_scale = arith_dec.scale;
    let (base, base_guard, base_kernel, _base_scale) = if let Some(cs) = &lq.case_sink {
        // CASE 写汇基值：sink_value 产出 DECIMAL 对齐的值 SQL 与
        // 内核守卫；hits/兜底门控内嵌（同 sink 非 allow_filter 形态）。
        let (dv, dv_kernel, scale) = super::case::sink_value(region, cs, env)?;
        if u32::from(scale) > arith_scale {
            return Err(SkipReason::UnprovenExpression);
        }
        let gated = format!(
            "CASE WHEN {hits} THEN {dv} ELSE {} END",
            lq.render.default_sql
        );
        // 基值升标度到 DECIMAL(38, arith_scale)：scale 只升不降无损，
        // 整数位容量 = 38-arith_scale——幅值越界行由内核守卫裁决
        // （DOUBLE 域求值永不抛错），准入后 CAST 不会溢出。
        let int_cap = 38 - i64::from(arith_scale);
        if int_cap < 1 {
            return Err(SkipReason::UnprovenExpression);
        }
        env.kernel_guards.push(format!(
            "(({gated}) IS NULL) OR (abs(CAST(({gated}) AS DOUBLE)) < 1e{})",
            int_cap - 1
        ));
        (
            format!("CAST(({gated}) AS DECIMAL(38,{arith_scale}))"),
            String::new(),
            dv_kernel,
            scale,
        )
    } else if lq.filter_on_miss {
        if !region.allow_filter {
            return Err(SkipReason::UnsupportedWrite);
        }
        (lq.render.value.clone(), hits, String::new(), 0)
    } else {
        (
            format!(
                "CASE WHEN {hits} THEN {} ELSE {} END",
                lq.render.value, lq.render.default_sql
            ),
            String::new(),
            String::new(),
            0,
        )
    };
    // 基值域：CASE 写汇产出 DECIMAL(≤38, 公共标度)（decimal_value
    // 对齐结果）；其余分支即查找声明精度。
    let bd = if lq.case_sink.is_some() {
        expr::Domain::Decimal {
            precision: 38,
            scale: arith_scale as u8,
        }
    } else {
        let Some((bp, bs)) = super::case::decimal(&lq.kind) else {
            return Err(SkipReason::UnprovenExpression);
        };
        expr::Domain::Decimal {
            precision: bp,
            scale: bs,
        }
    };
    let ad = expr::Domain::Decimal {
        precision: (arith_dec.int + arith_dec.scale) as u8,
        scale: arith_dec.scale as u8,
    };
    let domain = ad.unify(bd)?;
    let dbl = render_double(&write.expr, region, env)
        .ok()
        .map(|d| format!("CASE WHEN {cond} THEN {d} ELSE CAST(({base}) AS DOUBLE) END"));
    Ok(expr::BoundExpr {
        value: expr::GuardedExpr {
            expr: format!("CASE WHEN {cond} THEN ({arith}) ELSE ({base}) END"),
            guard: base_guard,
            kernel_guard: base_kernel,
        },
        domain,
        dbl,
        safe: false,
    })
}

/// 条件 → SQL：Field/Var/Const 叶子与比较/逻辑算子；Var 须为查找变量。
pub(super) fn cond_sql(
    cond: &IrExpr,
    region: &CopyLoop<'_>,
    env: &mut ProjEnv<'_>,
) -> Result<String, SkipReason> {
    match cond {
        IrExpr::Const(c) => {
            let Some(sql) = const_to_sql(c) else {
                return Err(SkipReason::UnprovenExpression);
            };
            Ok(sql)
        }
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
                BinOp::Mul => ld.mul(rd),
                BinOp::Sub => ld.sub(rd),
                _ => {
                    return Err(SkipReason::UnprovenExpression);
                }
            };
            // 静态精度超 38：内核结果类型封顶 DECIMAL(38, scale_res)，
            // 改发 DOUBLE 域幅值守卫（永不抛错），越界行入口整体回退。
            let d = match d {
                Ok(d) => d,
                Err(_) => guarded_bin(expr, op, ld, rd, region, env)?,
            };
            let op_sql = if matches!(op, BinOp::Mul) { "*" } else { "-" };
            Ok((format!("({ls} {op_sql} {rs})"), d))
        }
        _ => Err(SkipReason::UnprovenExpression),
    }
}

/// 数值常量 → (字面量, 值域)；Int 按十进制位数、Decimal 按声明精度保守取。
fn const_dec(c: &SpinValue) -> Result<(String, Dec), SkipReason> {
    let Some(sql) = const_to_sql(c) else {
        return Err(SkipReason::UnprovenExpression);
    };
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
    let Some((p, s)) = super::case::decimal(&col.kind) else {
        return Err(SkipReason::UnprovenExpression);
    };
    Ok((format!("sd_cur.{}", name.sql()), Dec::of_parts(p, s)?))
}

/// 查找变量 → (`CASE WHEN hits THEN value ELSE default END`, DECIMAL 值域)。
fn var_dec(v: &VarName, env: &mut ProjEnv<'_>) -> Result<(String, Dec), SkipReason> {
    let Some(lq) = env.lookups.iter().find(|l| &l.var == v) else {
        return Err(SkipReason::UnprovenExpression);
    };
    let Some((p, s)) = super::case::decimal(&lq.kind) else {
        return Err(SkipReason::UnprovenExpression);
    };
    let d = Dec::of_parts(p, s)?;
    Ok((lookup_case(v, env)?, d))
}

/// DECIMAL `*`/`-` 静态宽度超 38 时的值域守卫：内核结果类型为
/// DECIMAL(38, scale_res)（乘 scale 相加、减 scale 取大者），仅当
/// |结果| 越出值域才抛错。守卫以 DOUBLE 域重算整个子树——不求值
/// DECIMAL 算术，故嵌套溢出安全；NULL 传播显式放行。
/// scale_res > 38 属内核绑定期类型错误，无法行级守卫，保守拒绝。
fn guarded_bin(
    expr: &IrExpr,
    op: &BinOp,
    ld: Dec,
    rd: Dec,
    region: &CopyLoop<'_>,
    env: &mut ProjEnv<'_>,
) -> Result<Dec, SkipReason> {
    let scale_res = match op {
        BinOp::Mul => ld.scale + rd.scale,
        BinOp::Sub => ld.scale.max(rd.scale),
        _ => return Err(SkipReason::UnprovenExpression),
    };
    if scale_res > 38 {
        return Err(SkipReason::UnprovenExpression);
    }
    let int_cap = 38 - scale_res;
    if int_cap < 1 {
        return Err(SkipReason::UnprovenExpression);
    }
    let dbl = render_double(expr, region, env)?;
    env.kernel_guards.push(format!(
        "(({dbl}) IS NULL) OR (abs({dbl}) < 1e{})",
        int_cap - 1
    ));
    Ok(Dec {
        int: int_cap,
        scale: scale_res,
    })
}

/// 算术子树的 DOUBLE 域渲染：叶子 CAST AS DOUBLE、内部节点同构
/// 组合——全程不求值 DECIMAL 算术，守卫求值永不内核报错。
fn render_double(
    expr: &IrExpr,
    region: &CopyLoop<'_>,
    env: &mut ProjEnv<'_>,
) -> Result<String, SkipReason> {
    match expr {
        IrExpr::Const(c) => {
            let sql = const_to_sql(c).ok_or(SkipReason::UnprovenExpression)?;
            Ok(format!("CAST(({sql}) AS DOUBLE)"))
        }
        IrExpr::Var(v) => {
            let (sql, _) = var_dec(v, env)?;
            Ok(format!("CAST(({sql}) AS DOUBLE)"))
        }
        IrExpr::Field { rec, field } => {
            if rec != region.record {
                return Err(SkipReason::UnprovenExpression);
            }
            let name = Name(field.clone());
            source_column(env.select, env.source, &name)?;
            Ok(format!("CAST(sd_cur.{} AS DOUBLE)", name.sql()))
        }
        IrExpr::Bin { op, l, r } => {
            let op_sql = match op {
                BinOp::Mul => "*",
                BinOp::Sub => "-",
                _ => return Err(SkipReason::UnprovenExpression),
            };
            Ok(format!(
                "({} {op_sql} {})",
                render_double(l, region, env)?,
                render_double(r, region, env)?
            ))
        }
        _ => Err(SkipReason::UnprovenExpression),
    }
}

/// 查找变量 → 逐行代入值表达式（命中取值/落空兜底）；
/// 跳行查找与 CASE 写汇不是普通值来源，拒绝。
fn lookup_case(v: &VarName, env: &mut ProjEnv<'_>) -> Result<String, SkipReason> {
    let Some(lq) = env.lookups.iter().find(|l| &l.var == v) else {
        return Err(SkipReason::UnprovenExpression);
    };
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
