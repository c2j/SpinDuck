//! §3.3 条件算术覆写走查：IF 门识别、形态证明与槽位冲突校验。

use super::*;

/// §3.3 IF 门条件形态：Field/Var/Const 叶子与 And/Or/比较/Not。
pub(super) fn cond_shape(cond: &IrExpr) -> bool {
    match cond {
        IrExpr::Const(_) | IrExpr::Field { .. } | IrExpr::Var(_) => true,
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
        IrExpr::Var(_) => true,
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
fn vars_bound(expr: &IrExpr, lookups: &[lookup::LookupQuery]) -> bool {
    let mut vars = Vec::new();
    vars_of(expr, &mut vars);
    vars.iter().all(|v| lookups.iter().any(|l| &l.var == v))
}

/// IF 门整体判定：条件形态 + 变量绑定 + 门体恰一条乘/减赋值。
pub(super) fn gate(
    prog: &IrProg,
    pc: usize,
    cond: &IrExpr,
    lookups: &[lookup::LookupQuery],
    record: &VarName,
) -> Option<(VarName, IrExpr)> {
    if !cond_shape(cond) || !vars_bound(cond, lookups) {
        return None;
    }
    let IrStmt::Assign {
        target: AssignTarget::Var(v),
        expr,
    } = &prog.code[pc + 1]
    else {
        return None;
    };
    if !expr_shape(expr, record) || !vars_bound(expr, lookups) {
        return None;
    }
    Some((v.clone(), expr.clone()))
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
        let (var, expr) = gate(ctx.prog, pc, cond, &self.lookups, &ctx.region.record)?;
        self.gate = Some((
            Gate::Arith {
                cond: cond.clone(),
                var,
                expr,
            },
            to,
        ));
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
