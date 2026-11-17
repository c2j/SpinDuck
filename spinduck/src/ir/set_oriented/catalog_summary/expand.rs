//! 嵌套纯调用的递归展开、参数安装证明与环检测。

use super::{summarize, IrExpr, ParamMode, RoutineCatalog, Summary};

const MAX_INLINE_DEPTH: usize = 8;

pub(super) fn expand_calls(
    catalog: &RoutineCatalog,
    expr: IrExpr,
    visiting: &mut Vec<String>,
    depth: usize,
    checks: &mut super::CallChecks,
) -> Option<IrExpr> {
    Expander {
        catalog,
        visiting,
        checks,
    }
    .expand(expr, depth)
}

struct Expander<'a> {
    catalog: &'a RoutineCatalog,
    visiting: &'a mut Vec<String>,
    checks: &'a mut super::CallChecks,
}

impl Expander<'_> {
    fn expand(&mut self, expr: IrExpr, depth: usize) -> Option<IrExpr> {
        if depth > MAX_INLINE_DEPTH {
            return None;
        }
        match expr {
            IrExpr::FuncCall {
                name,
                args,
                prepared,
            } => {
                let args = args
                    .into_iter()
                    .map(|a| self.expand(a, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                if self.catalog.lookup(&name).is_none() {
                    Some(IrExpr::FuncCall {
                        name,
                        args,
                        prepared,
                    })
                } else {
                    self.expand_call(&name, args, depth)
                }
            }
            IrExpr::Bin { op, l, r } => Some(IrExpr::Bin {
                op,
                l: Box::new(self.expand(*l, depth)?),
                r: Box::new(self.expand(*r, depth)?),
            }),
            IrExpr::SqlFn {
                name,
                args,
                prepared,
            } => Some(IrExpr::SqlFn {
                name,
                args: args
                    .into_iter()
                    .map(|a| self.expand(a, depth))
                    .collect::<Option<Vec<_>>>()?,
                prepared,
            }),
            IrExpr::Case {
                operand,
                whens,
                else_expr,
            } => self.expand_case(operand, whens, else_expr, depth),
            IrExpr::Not(x) => Some(IrExpr::Not(Box::new(self.expand(*x, depth)?))),
            IrExpr::IsNull(x) => Some(IrExpr::IsNull(Box::new(self.expand(*x, depth)?))),
            IrExpr::Lookup {
                query,
                args,
                default,
            } => self.expand_lookup(query, args, *default, depth),
            IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => Some(expr),
            IrExpr::CursorAttr { .. } => None,
        }
    }

    fn expand_lookup(
        &mut self,
        query: String,
        args: Vec<(String, IrExpr)>,
        default: IrExpr,
        depth: usize,
    ) -> Option<IrExpr> {
        Some(IrExpr::Lookup {
            query,
            args: args
                .into_iter()
                .map(|(ph, arg)| Some((ph, self.expand(arg, depth)?)))
                .collect::<Option<Vec<_>>>()?,
            default: Box::new(self.expand(default, depth)?),
        })
    }

    fn expand_call(&mut self, name: &str, args: Vec<IrExpr>, depth: usize) -> Option<IrExpr> {
        let key = name.to_ascii_lowercase();
        if self.visiting.contains(&key) {
            return None;
        }
        let Summary::PureExpr(body) = summarize(self.catalog, name) else {
            return None;
        };
        let routine = self.catalog.lookup(name)?;
        if routine
            .params
            .iter()
            .any(|p| !matches!(p.mode, ParamMode::In))
        {
            return None;
        }
        self.checks
            .evaluations
            .extend(args.iter().filter(|arg| !super::arg_total(arg)).cloned());
        let args =
            super::installed_args(self.catalog, name, &args, &mut self.checks.varchar_inputs)?;
        let body = super::bind_body(body, &routine.params, &args, self.checks);
        self.visiting.push(key);
        let out = self.expand(body, depth + 1);
        self.visiting.pop();
        out
    }

    fn expand_case(
        &mut self,
        operand: Option<Box<IrExpr>>,
        whens: Vec<(IrExpr, IrExpr)>,
        else_expr: Option<Box<IrExpr>>,
        depth: usize,
    ) -> Option<IrExpr> {
        Some(IrExpr::Case {
            operand: match operand {
                Some(x) => Some(Box::new(self.expand(*x, depth)?)),
                None => None,
            },
            whens: whens
                .into_iter()
                .map(|(c, v)| Some((self.expand(c, depth)?, self.expand(v, depth)?)))
                .collect::<Option<Vec<_>>>()?,
            else_expr: match else_expr {
                Some(x) => Some(Box::new(self.expand(*x, depth)?)),
                None => None,
            },
        })
    }
}
