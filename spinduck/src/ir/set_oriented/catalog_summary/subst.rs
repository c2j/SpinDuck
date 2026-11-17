//! 形参及局部符号表达式代入。

use super::{IrExpr, VarName};

/// 同时替换全部形参，避免后续形参捕获先前代入的调用方变量。
pub(super) fn subst_params(
    expr: IrExpr,
    params: &[crate::catalog::RoutineParam],
    args: &[IrExpr],
) -> IrExpr {
    subst_with(expr, &|var| {
        params
            .iter()
            .zip(args)
            .find_map(|(param, arg)| var.0.eq_ignore_ascii_case(&param.name).then(|| arg.clone()))
    })
}

/// 将表达式中的形参变量（大小写不敏感）替换为实参表达式。
pub(in super::super) fn subst_var(e: IrExpr, param: &str, arg: &IrExpr) -> IrExpr {
    subst_with(e, &|v: &VarName| {
        v.0.eq_ignore_ascii_case(param).then(|| arg.clone())
    })
}

/// 将表达式中的局部变量替换为折叠环境中的已代入表达式（单遍、幂等）。
pub(in super::super) fn subst_env(e: IrExpr, env: &[(String, IrExpr)]) -> IrExpr {
    subst_with(e, &|v: &VarName| {
        let key = v.0.to_ascii_lowercase();
        env.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, val)| val.clone())
    })
}

/// 链式局部内联（§4.3 A7）：内联表达式可能引用先前赋值的其他内联变量
/// （如 v_x := f(v_n)，v_n := g(...)）。inline_pure_call 只代入函数形参，
/// 不做局部变量传递代入；此处把这类 Var 替换为其内联表达式，迭代至不动点
/// （赋值无环故收敛，上限 = 内联表长 + 1），使批写投影只含叶子构造。
pub(in super::super) fn resolve_locals(inlined: &mut [(VarName, IrExpr)]) {
    let changed = std::cell::Cell::new(false);
    for _ in 0..inlined.len() + 1 {
        let snapshot: Vec<(VarName, IrExpr)> = inlined.to_vec();
        changed.set(false);
        for (_, expr) in inlined.iter_mut() {
            *expr = subst_with(expr.clone(), &|v: &VarName| match snapshot
                .iter()
                .find(|(iv, _)| iv == v)
            {
                Some((_, ie)) => {
                    changed.set(true);
                    Some(ie.clone())
                }
                None => None,
            });
        }
        if !changed.get() {
            break;
        }
    }
}

/// 通用变量代入：`lookup` 命中即替换，否则保留原变量；递归遍历全部表达式形态。
fn subst_with(e: IrExpr, lookup: &dyn Fn(&VarName) -> Option<IrExpr>) -> IrExpr {
    match e {
        IrExpr::Var(v) => lookup(&v).unwrap_or(IrExpr::Var(v)),
        IrExpr::Bin { op, l, r } => IrExpr::Bin {
            op,
            l: Box::new(subst_with(*l, lookup)),
            r: Box::new(subst_with(*r, lookup)),
        },
        IrExpr::SqlFn {
            name,
            args,
            prepared,
        } => IrExpr::SqlFn {
            name,
            args: args.into_iter().map(|a| subst_with(a, lookup)).collect(),
            prepared,
        },
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => IrExpr::Case {
            operand: operand.map(|x| Box::new(subst_with(*x, lookup))),
            whens: whens
                .into_iter()
                .map(|(c, v)| (subst_with(c, lookup), subst_with(v, lookup)))
                .collect(),
            else_expr: else_expr.map(|x| Box::new(subst_with(*x, lookup))),
        },
        IrExpr::Not(x) => IrExpr::Not(Box::new(subst_with(*x, lookup))),
        IrExpr::IsNull(x) => IrExpr::IsNull(Box::new(subst_with(*x, lookup))),
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => IrExpr::FuncCall {
            name,
            args: args.into_iter().map(|a| subst_with(a, lookup)).collect(),
            prepared,
        },
        IrExpr::Lookup {
            query,
            args,
            default,
        } => IrExpr::Lookup {
            query,
            args: args
                .into_iter()
                .map(|(ph, a)| (ph, subst_with(a, lookup)))
                .collect(),
            default: Box::new(subst_with(*default, lookup)),
        },
        other => other,
    }
}
