//! §3.3 条件算术覆写走查：IF 门识别、形态证明与槽位冲突校验。

use super::*;

/// §3.3 IF 门条件形态：Field/Var/Const/Lookup 叶子与 And/Or/比较/Not。
/// Lookup 叶子为内联函数 OUT 写回代入后的计数门控查找（§4.3 A7）。
pub(super) fn cond_shape(cond: &IrExpr) -> bool {
    match cond {
        IrExpr::Const(_) | IrExpr::Field { .. } | IrExpr::Var(_) | IrExpr::Lookup { .. } => true,
        IrExpr::Not(e) => cond_shape(e),
        IrExpr::Bin { op, l, r } => {
            matches!(
                op,
                BinOp::And
                    | BinOp::Or
                    | BinOp::Eq
                    | BinOp::Ne
                    | BinOp::Lt
                    | BinOp::Le
                    | BinOp::Gt
                    | BinOp::Ge
            ) && cond_shape(l)
                && cond_shape(r)
        }
        _ => false,
    }
}

/// §3.3 表达式形态：仅 DECIMAL 乘/减，叶子为记录字段/变量/数值常量。
fn expr_shape(expr: &IrExpr, record: &VarName) -> bool {
    match expr {
        IrExpr::Bin {
            op: BinOp::Mul | BinOp::Sub,
            l,
            r,
        } => expr_shape(l, record) && expr_shape(r, record),
        IrExpr::Field { rec, .. } => rec == record,
        IrExpr::Var(_) | IrExpr::Lookup { .. } => true,
        IrExpr::Const(c) => matches!(
            c,
            crate::value::SpinValue::Int(_) | crate::value::SpinValue::Decimal(..)
        ),
        _ => false,
    }
}

/// 收集表达式中的变量叶子（Bin/Not 递归；其余形态无 Var）。
fn vars_of(expr: &IrExpr, out: &mut Vec<VarName>) {
    match expr {
        IrExpr::Var(v) => out.push(v.clone()),
        IrExpr::Bin { l, r, .. } => {
            vars_of(l, out);
            vars_of(r, out);
        }
        IrExpr::Not(e) => vars_of(e, out),
        _ => {}
    }
}

/// 条件与赋值表达式的变量叶子须全部是已识别查找变量（逐行代入）。
/// Lookup 子树内部的变量不在此列——由 `lookup_binds_invariant` 单独证明。
fn vars_bound(expr: &IrExpr, lookups: &[lookup::LookupQuery]) -> bool {
    let mut vars = Vec::new();
    vars_of(expr, &mut vars);
    vars.iter().all(|v| lookups.iter().any(|l| &l.var == v))
}

/// 内联变量逐行代入：`self.inlined` 经 `resolve_locals` 已封闭（表达式内
/// 不再引用其他内联变量），单次顺序替换即得不动点。
fn subst_inlined(expr: &IrExpr, inlined: &[(VarName, IrExpr)]) -> IrExpr {
    let mut cur = expr.clone();
    for (v, ie) in inlined {
        cur = catalog_summary::subst_var(cur, &v.0, ie);
    }
    cur
}

/// 收集表达式中所有 Lookup 子树（args + default，含嵌套 Lookup）内部的
/// Var 叶子；Lookup 之外的变量由 `vars_bound` 按查找变量口径另行证明。
fn lookup_inner_vars(expr: &IrExpr, out: &mut Vec<VarName>) {
    match expr {
        IrExpr::Lookup { args, default, .. } => {
            for (_, a) in args {
                all_vars(a, out);
            }
            all_vars(default, out);
        }
        IrExpr::Bin { l, r, .. } => {
            lookup_inner_vars(l, out);
            lookup_inner_vars(r, out);
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => lookup_inner_vars(e, out),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                lookup_inner_vars(o, out);
            }
            for (c, v) in whens {
                lookup_inner_vars(c, out);
                lookup_inner_vars(v, out);
            }
            if let Some(e) = else_expr {
                lookup_inner_vars(e, out);
            }
        }
        IrExpr::FuncCall { args, .. } | IrExpr::SqlFn { args, .. } => {
            for a in args {
                lookup_inner_vars(a, out);
            }
        }
        _ => {}
    }
}

/// 表达式内全部 Var 叶子（含 Lookup 内部）。
fn all_vars(expr: &IrExpr, out: &mut Vec<VarName>) {
    match expr {
        IrExpr::Var(v) => out.push(v.clone()),
        IrExpr::Lookup { args, default, .. } => {
            for (_, a) in args {
                all_vars(a, out);
            }
            all_vars(default, out);
        }
        IrExpr::Bin { l, r, .. } => {
            all_vars(l, out);
            all_vars(r, out);
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => all_vars(e, out),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                all_vars(o, out);
            }
            for (c, v) in whens {
                all_vars(c, out);
                all_vars(v, out);
            }
            if let Some(e) = else_expr {
                all_vars(e, out);
            }
        }
        IrExpr::FuncCall { args, .. } | IrExpr::SqlFn { args, .. } => {
            for a in args {
                all_vars(a, out);
            }
        }
        _ => {}
    }
}

/// Lookup 内部的 Var 绑定渲染为静态实参，必须是循环不变的 IN 形参
/// （如 p_i_date）；区域查找变量等逐行变量不可证 → 拒绝。
fn lookup_binds_invariant(expr: &IrExpr, in_params: &std::collections::BTreeSet<String>) -> bool {
    let mut vars = Vec::new();
    lookup_inner_vars(expr, &mut vars);
    vars.iter()
        .all(|v| in_params.contains(&v.0.to_ascii_lowercase()))
}

/// IF 门整体判定：内联变量代入 + 条件形态 + 变量绑定 + 门体恰一条乘/减赋值。
#[allow(clippy::too_many_arguments)]
pub(super) fn gate(
    prog: &IrProg,
    pc: usize,
    cond: &IrExpr,
    lookups: &[lookup::LookupQuery],
    record: &VarName,
    inlined: &[(VarName, IrExpr)],
    in_params: &std::collections::BTreeSet<String>,
) -> Option<(VarName, IrExpr, IrExpr)> {
    let cond = subst_inlined(cond, inlined);
    let IrStmt::Assign {
        target: AssignTarget::Var(v),
        expr,
    } = &prog.code[pc + 1]
    else {
        return None;
    };
    let expr = subst_inlined(expr, inlined);
    if !cond_shape(&cond)
        || !vars_bound(&cond, lookups)
        || !lookup_binds_invariant(&cond, in_params)
        || !expr_shape(&expr, record)
        || !vars_bound(&expr, lookups)
        || !lookup_binds_invariant(&expr, in_params)
    {
        return None;
    }
    Some((v.clone(), cond, expr))
}

impl<'a> Body<'a> {
    /// §3.3 算术覆写门识别；命中返回续跑 pc（跳过门体赋值语句）。
    pub(super) fn arith_gate(
        &mut self,
        ctx: &Context<'_, '_>,
        pc: usize,
        cond: &IrExpr,
        to: usize,
        back_pc: usize,
    ) -> Option<usize> {
        if self.gate.is_some()
            || ctx.mode != SetOrientedMode::BatchAtomic
            || to != pc + 3
            || to >= back_pc
        {
            return None;
        }
        let (var, cond, expr) = gate(
            ctx.prog,
            pc,
            cond,
            &self.lookups,
            &ctx.region.record,
            &self.inlined,
            &ctx.in_params,
        )?;
        self.gate = Some((Gate::Arith { cond, var, expr }, to));
        Some(pc + 2)
    }
}

/// 槽位冲突校验：覆写变量须是已识别查找且无其他绑定；同变量至多一次。
pub(super) fn validate(body: &Body<'_>, mode: SetOrientedMode) -> Result<(), SkipReason> {
    if body.arith_writes.is_empty() {
        return Ok(());
    }
    if mode != SetOrientedMode::BatchAtomic {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    for w in &body.arith_writes {
        let conflicts = body.folded.iter().any(|(v, _)| v == &w.var)
            || body.inlined.iter().any(|(v, _)| v == &w.var)
            || body.sinks.iter().any(|(v, _, _, _)| v == &w.var)
            || body.tuple_vars.contains(&w.var)
            || body.agg_vars.contains(&w.var)
            || body.dead.contains(&w.var)
            || body.arith_writes.iter().filter(|o| o.var == w.var).count() > 1;
        let lq = body.lookups.iter().find(|l| l.var == w.var);
        if conflicts || lq.is_none_or(|l| l.filter_on_miss || l.entry_guard) {
            return Err(SkipReason::UnsupportedControlFlow);
        }
    }
    Ok(())
}
