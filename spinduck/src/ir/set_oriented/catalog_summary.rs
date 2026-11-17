//! 跨例程摘要（设计 §4.3 A7）：NoOp / PureExpr / Unregistered / Impure。
//! NoOp 消除必须证明实参安装与 OUT/INOUT 写回均无值/错误可观察差异。

mod body;
mod expand;
mod subst;
#[cfg(test)]
mod tests;

use body::fold_body;
use expand::expand_calls;
pub(super) use subst::subst_env;
pub(super) use subst::{resolve_locals, subst_var};

use super::{AssignTarget, BinOp, IrExpr, IrProg, IrStmt, VarName};
use crate::catalog::{ParamMode, RoutineCatalog, RoutineKind};
use crate::types::SpinType;
use crate::value::SpinValue;

/// 例程体摘要；只读证明面，不执行任何 SQL。
#[derive(Debug, Clone)]
pub(crate) enum Summary {
    /// 体仅 Nop / 裸 RETURN，无任何可观察效果（限过程）。
    NoOp,
    /// 纯表达式及仍需证明的槽位安装边界。
    PureExpr(InlinedCall),
    /// 目录未命中：保留原分派与报错时机。
    Unregistered,
    /// 其余一律保守拒绝。
    Impure,
}

/// 按目录名取摘要；未登记返回 Unregistered。
pub(crate) fn summarize(catalog: &RoutineCatalog, name: &str) -> Summary {
    summarize_inner(catalog, name, &mut vec![name.to_ascii_lowercase()])
}

/// visiting 携带当前纯判定链上的例程名（小写），FuncCall 命中即环 → 拒绝，
/// 保证自递归/互递归绝不进入无限展开（设计 §4.3）。
fn summarize_inner(catalog: &RoutineCatalog, name: &str, visiting: &mut Vec<String>) -> Summary {
    let Some(r) = catalog.lookup(name) else {
        return Summary::Unregistered;
    };
    if matches!(r.kind, RoutineKind::Procedure) {
        let mut ret: Option<Option<IrExpr>> = None;
        for st in &r.prog.code {
            match st {
                IrStmt::Nop => {}
                IrStmt::Return { value } => {
                    if ret.is_some() {
                        return Summary::Impure;
                    }
                    ret = Some(value.clone());
                }
                _ => return Summary::Impure,
            }
        }
        return match ret {
            None | Some(None) => Summary::NoOp,
            _ => Summary::Impure,
        };
    }
    match fold_body(catalog, &r.prog, visiting) {
        Some(call) if pure_expr(catalog, &call.expr, visiting) => Summary::PureExpr(call),
        _ => Summary::Impure,
    }
}

/// 纯表达式判定：Const/Var/Bin/SqlFn/Case/Not/IsNull，以及无环嵌套纯调用
/// （FuncCall 递归判纯，visiting 环检测）。排除 Field/CursorAttr（帧态）。
fn pure_expr(catalog: &RoutineCatalog, e: &IrExpr, visiting: &mut Vec<String>) -> bool {
    match e {
        IrExpr::Const(_) | IrExpr::Var(_) => true,
        IrExpr::Bin { l, r, .. } => {
            pure_expr(catalog, l, visiting) && pure_expr(catalog, r, visiting)
        }
        IrExpr::SqlFn { args, prepared, .. } => {
            prepared.is_ok() && args.iter().all(|a| pure_expr(catalog, a, visiting))
        }
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand
                .as_deref()
                .is_none_or(|x| pure_expr(catalog, x, visiting))
                && whens.iter().all(|(c, v)| {
                    pure_expr(catalog, c, visiting) && pure_expr(catalog, v, visiting)
                })
                && else_expr
                    .as_deref()
                    .is_none_or(|x| pure_expr(catalog, x, visiting))
        }
        IrExpr::Not(x) | IrExpr::IsNull(x) => pure_expr(catalog, x, visiting),
        IrExpr::Lookup { args, default, .. } => {
            args.iter().all(|(_, a)| pure_expr(catalog, a, visiting))
                && pure_expr(catalog, default, visiting)
        }
        IrExpr::FuncCall {
            name,
            args,
            prepared,
            ..
        } => {
            let key = name.to_ascii_lowercase();
            if !args.iter().all(|a| pure_expr(catalog, a, visiting)) {
                return false;
            }
            // 未注册名 = 内建/内核函数（解释器下沉 SELECT f(...) 求值），无 PL
            // 副作用，判纯；可渲染性由 inline_safe 白名单另行把关。
            if catalog.lookup(name).is_none() {
                return prepared.is_ok();
            }
            if visiting.contains(&key) {
                return false;
            }
            visiting.push(key);
            let ok = matches!(
                summarize_inner(catalog, name, visiting),
                Summary::PureExpr(_)
            );
            visiting.pop();
            ok
        }
        IrExpr::Field { .. } | IrExpr::CursorAttr { .. } => false,
    }
}

/// 调用点 NoOp 证明：摘要 NoOp + 实参全定义 + VARCHAR 形参实参可证
/// Str/Null（安装恒等）+ OUT/INOUT 写回槽与形参同为 VARCHAR（写回恒等）。
pub(crate) fn call_is_noop(
    catalog: &RoutineCatalog,
    caller: &IrProg,
    name: &str,
    args: &[IrExpr],
    out_targets: &[Option<AssignTarget>],
) -> bool {
    if !matches!(summarize(catalog, name), Summary::NoOp) {
        return false;
    }
    let Some(r) = catalog.lookup(name) else {
        return false;
    };
    if args.len() != r.params.len() || out_targets.len() != r.params.len() {
        return false;
    }
    for ((p, arg), out) in r.params.iter().zip(args).zip(out_targets) {
        let varchar_param = p.ty == Some(SpinType::Varchar);
        if varchar_param {
            if !provably_str_null(arg, caller) {
                return false;
            }
        } else if !arg_total(arg) {
            return false;
        }
        if matches!(p.mode, ParamMode::Out | ParamMode::InOut) {
            match out {
                None => {}
                Some(AssignTarget::Var(v)) => {
                    let slot = caller.var_types.get(&v.0.to_ascii_lowercase());
                    if varchar_param {
                        if slot != Some(&SpinType::Varchar) {
                            return false;
                        }
                    } else if !matches!(slot, Some(t) if *t != SpinType::Varchar) {
                        return false;
                    }
                }
                Some(AssignTarget::Field { .. }) => return false,
            }
        }
    }
    true
}

/// 内联表达式及必须随折叠保留的证明义务。
#[derive(Debug, Clone)]
pub(crate) struct InlinedCall {
    pub expr: IrExpr,
    pub checks: CallChecks,
}

/// 槽位安装与被消去求值的证明义务。
#[derive(Debug, Clone, Default)]
pub(crate) struct CallChecks {
    pub varchar_inputs: Vec<IrExpr>,
    pub evaluations: Vec<IrExpr>,
}

fn bind_body(
    body: InlinedCall,
    params: &[crate::catalog::RoutineParam],
    args: &[IrExpr],
    checks: &mut CallChecks,
) -> IrExpr {
    checks.varchar_inputs.extend(
        body.checks
            .varchar_inputs
            .into_iter()
            .map(|expr| subst::subst_params(expr, params, args)),
    );
    checks.evaluations.extend(
        body.checks
            .evaluations
            .into_iter()
            .map(|expr| subst::subst_params(expr, params, args)),
    );
    subst::subst_params(body.expr, params, args)
}

fn installed_args(
    catalog: &RoutineCatalog,
    name: &str,
    args: &[IrExpr],
    varchar_inputs: &mut Vec<IrExpr>,
) -> Option<Vec<IrExpr>> {
    let routine = catalog.lookup(name)?;
    if args.len() != routine.params.len() {
        return None;
    }
    routine
        .params
        .iter()
        .zip(args)
        .map(|(param, arg)| match arg {
            IrExpr::Const(value) => crate::value::coerce_for_slot(
                value,
                param.ty.as_ref(),
                catalog.sql_rules().implicit_assignment_loose,
            )
            .ok()
            .map(IrExpr::Const),
            _ => {
                if param.ty == Some(SpinType::Varchar) {
                    varchar_inputs.push(arg.clone());
                }
                Some(arg.clone())
            }
        })
        .collect()
}

/// 纯函数符号内联（§4.3 A7 PureExpr(SymExpr)）：PureExpr 体 + 全 IN 形参 +
/// 实参全定义 + 元数匹配，将形参变量替换为实参表达式，返回内联后的 IrExpr
/// 供批写投影渲染。非常量纯函数（如日期算术）由此进入集合 SQL，而非折叠。
pub(super) fn inline_pure_call(
    catalog: &RoutineCatalog,
    name: &str,
    args: &[IrExpr],
) -> Option<InlinedCall> {
    let Summary::PureExpr(body) = summarize(catalog, name) else {
        return None;
    };
    let r = catalog.lookup(name)?;
    if r.params.iter().any(|p| !matches!(p.mode, ParamMode::In)) {
        return None;
    }
    if args.len() != r.params.len() || !args.iter().all(arg_total) {
        return None;
    }
    let mut checks = CallChecks::default();
    let args = installed_args(catalog, name, args, &mut checks.varchar_inputs)?;
    let expr = bind_body(body, &r.params, &args, &mut checks);
    let expr = expand_calls(
        catalog,
        expr,
        &mut vec![name.to_ascii_lowercase()],
        0,
        &mut checks,
    )?;
    expand_checks(catalog, name, &mut checks)?;
    Some(InlinedCall { expr, checks })
}

fn expand_checks(catalog: &RoutineCatalog, name: &str, checks: &mut CallChecks) -> Option<()> {
    let (mut inputs, mut evaluations) = (0, 0);
    while inputs < checks.varchar_inputs.len() || evaluations < checks.evaluations.len() {
        let is_input = inputs < checks.varchar_inputs.len();
        let expr = if is_input {
            checks.varchar_inputs[inputs].clone()
        } else {
            checks.evaluations[evaluations].clone()
        };
        let expanded = expand_calls(
            catalog,
            expr,
            &mut vec![name.to_ascii_lowercase()],
            0,
            checks,
        )?;
        if is_input {
            checks.varchar_inputs[inputs] = expanded;
            inputs += 1;
        } else {
            checks.evaluations[evaluations] = expanded;
            evaluations += 1;
        }
    }
    Some(())
}

/// 实参全定义：常量/变量/记录字段/拼接在合法帧上求值不抛错。
fn arg_total(expr: &IrExpr) -> bool {
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => true,
        IrExpr::Bin {
            op: BinOp::Concat,
            l,
            r,
        } => arg_total(l) && arg_total(r),
        _ => false,
    }
}

/// 可证 Str/Null：VARCHAR 槽安装 coerce_for_slot 恒等，与 loose 开关无关。
/// 拼接含非空字符串字面量一侧时结果恒 Str（双 NULL 才 NULL）。
fn provably_str_null(expr: &IrExpr, caller: &IrProg) -> bool {
    match expr {
        IrExpr::Const(SpinValue::Str(_) | SpinValue::Null) => true,
        IrExpr::Var(v) => {
            // 隐式错误帧变量 __sqlerrm：解释器进入处理器前写入错误消息（恒
            // Str），未声明于 var_types；VARCHAR 槽安装恒等，NoOp 日志桩须可消除。
            v.0.eq_ignore_ascii_case("__sqlerrm")
                || caller.var_types.get(&v.0.to_ascii_lowercase()) == Some(&SpinType::Varchar)
        }
        IrExpr::Bin {
            op: BinOp::Concat,
            l,
            r,
        } => {
            non_null_str(l)
                || non_null_str(r)
                || (provably_str_null(l, caller) && provably_str_null(r, caller))
        }
        _ => false,
    }
}

fn non_null_str(expr: &IrExpr) -> bool {
    matches!(expr, IrExpr::Const(SpinValue::Str(s)) if !s.is_empty())
}
