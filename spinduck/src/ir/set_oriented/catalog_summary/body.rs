//! 函数体的顺序赋值、条件与兜底查找折叠。

use super::{arg_total, provably_str_null, pure_expr, subst_env, CallChecks, InlinedCall};
use super::{AssignTarget, IrExpr, IrProg, IrStmt, RoutineCatalog, SpinType, VarName};
use crate::ir::BindSrc;

/// 在顺序代入的同时保留槽位安装与被消去求值的证明义务。
pub(super) fn fold_body(
    catalog: &RoutineCatalog,
    prog: &IrProg,
    visiting: &mut Vec<String>,
) -> Option<InlinedCall> {
    BodyFolder {
        catalog,
        prog,
        visiting,
        env: Vec::new(),
        checks: CallChecks::default(),
        result: None,
    }
    .fold()
}

struct BodyFolder<'a> {
    catalog: &'a RoutineCatalog,
    prog: &'a IrProg,
    visiting: &'a mut Vec<String>,
    env: Vec<(String, IrExpr)>,
    checks: CallChecks,
    result: Option<IrExpr>,
}

impl BodyFolder<'_> {
    fn fold(mut self) -> Option<InlinedCall> {
        let code = &self.prog.code;
        let mut pc = 0;
        while pc < code.len() {
            match &code[pc] {
                IrStmt::Nop => pc += 1,
                IrStmt::Assign {
                    target: AssignTarget::Var(v),
                    expr,
                } => {
                    self.assign(v, expr)?;
                    pc += 1;
                }
                IrStmt::PushTrap { .. } => pc = self.lookup(pc)?,
                IrStmt::JumpIfNot { cond, to } => {
                    self.conditional(pc, cond, *to)?;
                    pc = *to;
                }
                IrStmt::Return { value: Some(expr) } => {
                    if self.result.is_some() {
                        return None;
                    }
                    self.result = Some(subst_env(expr.clone(), &self.env));
                    pc += 1;
                }
                _ => return None,
            }
        }
        if matches!(self.result, Some(IrExpr::Const(_))) && !constant_body_total(self.prog) {
            return None;
        }
        Some(InlinedCall {
            expr: self.result?,
            checks: self.checks,
        })
    }

    fn assign(&mut self, var: &VarName, expr: &IrExpr) -> Option<()> {
        if !arg_total(expr) && super::super::dead::read_only_within(self.prog, var, &[]) {
            return None;
        }
        if self.prog.var_types.get(&var.0) == Some(&SpinType::Varchar) {
            self.checks
                .varchar_inputs
                .push(subst_env(expr.clone(), &self.env));
        }
        if let Some((_, prior)) = self
            .env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&var.0))
        {
            retain_evaluation(&mut self.checks, prior);
        }
        apply_assign(self.catalog, &mut self.env, var, expr, self.visiting)
    }

    fn lookup(&mut self, pc: usize) -> Option<usize> {
        let (key, lookup, next) =
            fold_lookup(self.catalog, &self.prog.code, pc, &self.env, self.visiting)?;
        if self.prog.var_types.get(&key) == Some(&SpinType::Varchar) {
            self.checks.varchar_inputs.push(lookup.clone());
        }
        write_env(&mut self.env, key, lookup);
        Some(next)
    }

    fn conditional(&mut self, pc: usize, cond: &IrExpr, to: usize) -> Option<()> {
        let code = &self.prog.code;
        if to <= pc + 1 || to > code.len() {
            return None;
        }
        if !matches!(code.get(to - 1), Some(IrStmt::Jump(t)) if *t == to) {
            return None;
        }
        let cond = subst_env(cond.clone(), &self.env);
        if !pure_expr(self.catalog, &cond, self.visiting) {
            return None;
        }
        retain_evaluation(&mut self.checks, &cond);
        let mut then_env = self.env.clone();
        let assigned = self.fold_then(&code[pc + 1..to - 1], &cond, &mut then_env)?;
        self.merge_then(&assigned, &then_env, &cond)
    }

    fn fold_then(
        &mut self,
        code: &[IrStmt],
        cond: &IrExpr,
        env: &mut Vec<(String, IrExpr)>,
    ) -> Option<Vec<String>> {
        let mut assigned = Vec::new();
        for stmt in code {
            match stmt {
                IrStmt::Nop => {}
                IrStmt::Assign {
                    target: AssignTarget::Var(var),
                    expr,
                } => {
                    let key = var.0.to_ascii_lowercase();
                    let rhs = subst_env(expr.clone(), env);
                    if self.prog.var_types.get(&key) == Some(&SpinType::Varchar) {
                        self.checks.varchar_inputs.push(rhs.clone());
                    }
                    if !arg_total(&rhs) {
                        self.checks.evaluations.push(IrExpr::Case {
                            operand: None,
                            whens: vec![(cond.clone(), rhs)],
                            else_expr: None,
                        });
                    }
                    apply_assign(self.catalog, env, var, expr, self.visiting)?;
                    if !assigned.contains(&key) {
                        assigned.push(key);
                    }
                }
                _ => return None,
            }
        }
        Some(assigned)
    }

    fn merge_then(
        &mut self,
        assigned: &[String],
        then_env: &[(String, IrExpr)],
        cond: &IrExpr,
    ) -> Option<()> {
        for key in assigned {
            let then_val = then_env.iter().find(|(k, _)| k == key)?.1.clone();
            let prior = self
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| IrExpr::Var(VarName(key.clone())));
            retain_evaluation(&mut self.checks, &prior);
            let case = IrExpr::Case {
                operand: None,
                whens: vec![(cond.clone(), then_val)],
                else_expr: Some(Box::new(prior)),
            };
            write_env(&mut self.env, key.clone(), case);
        }
        Some(())
    }
}

fn retain_evaluation(checks: &mut CallChecks, expr: &IrExpr) {
    if !arg_total(expr) {
        checks.evaluations.push(expr.clone());
    }
}

fn write_env(env: &mut Vec<(String, IrExpr)>, key: String, expr: IrExpr) {
    if let Some(slot) = env.iter_mut().find(|(k, _)| k == &key) {
        slot.1 = expr;
    } else {
        env.push((key, expr));
    }
}

fn constant_body_total(prog: &IrProg) -> bool {
    prog.code.iter().all(|stmt| match stmt {
        IrStmt::Nop | IrStmt::Return { .. } => true,
        IrStmt::Assign {
            target: AssignTarget::Var(v),
            expr,
        } => {
            arg_total(expr)
                && (prog.var_types.get(&v.0) != Some(&SpinType::Varchar)
                    || provably_str_null(expr, prog))
        }
        _ => false,
    })
}

fn apply_assign(
    catalog: &RoutineCatalog,
    env: &mut Vec<(String, IrExpr)>,
    var: &VarName,
    expr: &IrExpr,
    visiting: &mut Vec<String>,
) -> Option<()> {
    let rhs = subst_env(expr.clone(), env);
    if !pure_expr(catalog, &rhs, visiting) {
        return None;
    }
    write_env(env, var.0.to_ascii_lowercase(), rhs);
    Some(())
}

fn fold_lookup(
    catalog: &RoutineCatalog,
    code: &[IrStmt],
    pc: usize,
    env: &[(String, IrExpr)],
    visiting: &mut Vec<String>,
) -> Option<(String, IrExpr, usize)> {
    let IrStmt::SqlQuery {
        sql,
        binds,
        into,
        fn_sites,
    } = code.get(pc + 1)?
    else {
        return None;
    };
    if !fn_sites.is_empty() {
        return None;
    }
    let [AssignTarget::Var(var)] = into.as_slice() else {
        return None;
    };
    let (default, resume) = lookup_handler(code, pc, var)?;
    let default = subst_env(default.clone(), env);
    if !pure_expr(catalog, &default, visiting) {
        return None;
    }
    let mut args = Vec::with_capacity(binds.len());
    for (ph, src) in binds {
        let arg = match src {
            BindSrc::Var(v) => IrExpr::Var(v.clone()),
            BindSrc::Field { rec, field } => IrExpr::Field {
                rec: rec.clone(),
                field: field.clone(),
            },
        };
        args.push((ph.clone(), subst_env(arg, env)));
    }
    let lookup = IrExpr::Lookup {
        query: sql.clone(),
        args,
        default: Box::new(default),
    };
    Some((var.0.to_ascii_lowercase(), lookup, resume))
}

fn lookup_handler<'a>(code: &'a [IrStmt], pc: usize, var: &VarName) -> Option<(&'a IrExpr, usize)> {
    let IrStmt::PushTrap { entries } = code.get(pc)? else {
        return None;
    };
    let [entry] = entries.as_slice() else {
        return None;
    };
    if !entry.conds.iter().all(|c| c.eq_ignore_ascii_case("others")) {
        return None;
    }
    let (start, end) = (entry.body_start, entry.body_end);
    if end != start + 1 || end >= code.len() {
        return None;
    }
    if !matches!(code.get(pc + 2), Some(IrStmt::PopTrap)) {
        return None;
    }
    let IrStmt::Jump(resume) = code.get(pc + 3)? else {
        return None;
    };
    let IrStmt::Assign {
        target: AssignTarget::Var(handler_var),
        expr,
    } = code.get(start)?
    else {
        return None;
    };
    if !handler_var.0.eq_ignore_ascii_case(&var.0) {
        return None;
    }
    if !matches!(code.get(end), Some(IrStmt::Jump(target)) if target == resume) {
        return None;
    }
    Some((expr, *resume))
}
