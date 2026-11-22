//! 函数体的顺序赋值、条件与兜底查找折叠。

use super::sqlcols::select_list_parts;
use super::{arg_total, provably_str_null, pure_expr, subst_env, CallChecks, InlinedCall};
use super::{AssignTarget, IrExpr, IrProg, IrStmt, RoutineCatalog, SpinType, VarName};
use crate::catalog::{ParamMode, RoutineParam};
use crate::ir::BindSrc;

/// 在顺序代入的同时保留槽位安装与被消去求值的证明义务。
pub(super) fn fold_body(
    catalog: &RoutineCatalog,
    prog: &IrProg,
    params: &[RoutineParam],
    visiting: &mut Vec<String>,
) -> Option<InlinedCall> {
    BodyFolder {
        catalog,
        prog,
        params,
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
    params: &'a [RoutineParam],
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
        let mut outs = Vec::new();
        for (idx, p) in self.params.iter().enumerate() {
            match p.mode {
                ParamMode::In => {}
                ParamMode::InOut => return None,
                ParamMode::Out => {
                    let key = p.name.to_ascii_lowercase();
                    if let Some((_, expr)) = self.env.iter().find(|(k, _)| *k == key) {
                        outs.push((idx, expr.clone()));
                    }
                }
            }
        }
        Some(InlinedCall {
            expr: self.result?,
            outs,
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
        let prog = self.prog;
        fold_trap(
            self.catalog,
            prog,
            self.visiting,
            &mut self.checks,
            &mut self.env,
            &prog.code,
            pc,
        )
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

/// `$v_` 绑定 → 实参表达式（env 代入后）。
/// `BindSrc::Expr` 仅存于 lookup 文法内部，不出现在 IR 绑定；出现即形态破坏，
/// 保守返回 None 交回调用方放弃折叠。
fn bind_args(
    binds: &[(String, BindSrc)],
    env: &[(String, IrExpr)],
) -> Option<Vec<(String, IrExpr)>> {
    binds
        .iter()
        .map(|(ph, src)| {
            let arg = match src {
                BindSrc::Var(v) => IrExpr::Var(v.clone()),
                BindSrc::Field { rec, field } => IrExpr::Field {
                    rec: rec.clone(),
                    field: field.clone(),
                },
                BindSrc::Expr { .. } => return None,
            };
            Some((ph.clone(), subst_env(arg, env)))
        })
        .collect()
}

/// trap 形态解构结果（均借用 `code`）。
struct TrapShape<'a> {
    sql: &'a str,
    binds: &'a [(String, BindSrc)],
    targets: Vec<VarName>,
    columns: Vec<String>,
    from_pos: usize,
    resume: usize,
    body: (usize, usize),
}

/// `PushTrap(others)` + `SqlQuery INTO` + `PopTrap` + `Jump(rejoin)` 形态识别；
/// handler 段非空且以 `Jump(rejoin)` 收束。任一环节不可证 → None。
fn trap_shape<'a>(code: &'a [IrStmt], pc: usize) -> Option<TrapShape<'a>> {
    let IrStmt::PushTrap { entries } = code.get(pc)? else {
        return None;
    };
    let [entry] = entries.as_slice() else {
        return None;
    };
    if !entry.conds.iter().all(|c| c.eq_ignore_ascii_case("others")) {
        return None;
    }
    let IrStmt::SqlQuery {
        sql,
        binds,
        into,
        fn_sites,
    } = code.get(pc + 1)?
    else {
        return None;
    };
    if !fn_sites.is_empty() || into.is_empty() {
        return None;
    }
    let mut targets = Vec::with_capacity(into.len());
    for t in into {
        let AssignTarget::Var(v) = t else {
            return None;
        };
        targets.push(v.clone());
    }
    if !matches!(code.get(pc + 2), Some(IrStmt::PopTrap)) {
        return None;
    }
    let IrStmt::Jump(resume) = code.get(pc + 3)? else {
        return None;
    };
    let resume = *resume;
    let (bstart, bend) = (entry.body_start, entry.body_end);
    if bend >= code.len() || bend <= bstart {
        return None;
    }
    if !matches!(code.get(bend), Some(IrStmt::Jump(t)) if *t == resume) {
        return None;
    }
    let (columns, from_pos) = select_list_parts(sql)?;
    if columns.len() != targets.len() {
        return None;
    }
    Some(TrapShape {
        sql,
        binds,
        targets,
        columns,
        from_pos,
        resume,
        body: (bstart, bend),
    })
}

/// handler 段顺序折叠：Nop 跳过、Assign 代入、嵌套 PushTrap 递归；
/// 赋值目标必须属于本层 INTO 目标集（否则条件写回无法用 env 表达 → None）；
/// 其余语句形态（含段内无条件跳转）→ None。
fn fold_span(
    catalog: &RoutineCatalog,
    prog: &IrProg,
    visiting: &mut Vec<String>,
    checks: &mut CallChecks,
    env: &mut Vec<(String, IrExpr)>,
    code: &[IrStmt],
    shape: &TrapShape<'_>,
) -> Option<()> {
    let allowed: std::collections::BTreeSet<String> = shape
        .targets
        .iter()
        .map(|v| v.0.to_ascii_lowercase())
        .collect();
    let (mut i, end) = shape.body;
    while i < end {
        match &code[i] {
            IrStmt::Nop => i += 1,
            IrStmt::Assign {
                target: AssignTarget::Var(v),
                expr,
            } => {
                if !allowed.contains(&v.0.to_ascii_lowercase()) {
                    return None;
                }
                apply_assign(catalog, env, v, expr, visiting)?;
                i += 1;
            }
            IrStmt::PushTrap { .. } => {
                i = fold_trap(catalog, prog, visiting, checks, env, code, i)?;
            }
            _ => return None,
        }
    }
    Some(())
}

/// trap 保护段折叠：handler 段在克隆 env 上折叠（嵌套 trap 递归、普通赋值
/// 为条件兜底）；每个 INTO 目标生成独立 `Lookup`——query 包装为单列
/// `SELECT <col> FROM (<原SQL>)`，default 取 handler 折叠后该变量的值。
/// 返回 rejoin 目标 pc；形态不可证 → None。
fn fold_trap(
    catalog: &RoutineCatalog,
    prog: &IrProg,
    visiting: &mut Vec<String>,
    checks: &mut CallChecks,
    env: &mut Vec<(String, IrExpr)>,
    code: &[IrStmt],
    pc: usize,
) -> Option<usize> {
    let shape = trap_shape(code, pc)?;
    let mut henv = env.clone();
    fold_span(catalog, prog, visiting, checks, &mut henv, code, &shape)?;
    let args = bind_args(shape.binds, env)?;
    for (k, var) in shape.targets.iter().enumerate() {
        let key = var.0.to_ascii_lowercase();
        let (_, default) = henv.iter().find(|(n, _)| *n == key)?;
        if !pure_expr(catalog, default, visiting) {
            return None;
        }
        // 单目标保持原 SQL 逐字（下游 parse_inline/parse_total 文法约束）；
        // 多目标按原列文本拼接单列 SELECT，保留 FROM 起的全部子句。
        let query = if shape.targets.len() == 1 {
            shape.sql.to_string()
        } else {
            format!(
                "SELECT {} {}",
                shape.columns[k],
                &shape.sql[shape.from_pos..]
            )
        };
        let lookup = IrExpr::Lookup {
            query,
            args: args.clone(),
            default: Box::new(default.clone()),
        };
        if prog.var_types.get(&key) == Some(&SpinType::Varchar) {
            checks.varchar_inputs.push(lookup.clone());
        }
        write_env(env, key, lookup);
    }
    Some(shape.resume)
}
