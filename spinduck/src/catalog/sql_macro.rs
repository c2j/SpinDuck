//! 纯表达式宏的静态判定与 SQL 渲染。

use super::{CompiledRoutine, ParamMode, RoutineCatalog, RoutineKind};
use crate::ir::IrExpr;
use crate::rules::RuleRegistry;
use std::collections::BTreeSet;

pub(crate) fn render(
    routine: &CompiledRoutine,
    rules: &RuleRegistry,
    catalog: &RoutineCatalog,
    macros: &BTreeSet<String>,
) -> Option<String> {
    if routine.kind != RoutineKind::Function
        || routine.name.contains('.')
        || routine.params.iter().any(|p| p.mode != ParamMode::In)
    {
        return None;
    }
    let expr = routine.prog.single_return_expr()?;
    let body = crate::sql::expr_to_sql(expr, rules)?;
    if references_unmacroized_routine(expr, catalog, macros) {
        return None;
    }
    let params = routine
        .params
        .iter()
        .map(|p| p.name.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "CREATE OR REPLACE MACRO {}({}) AS ({})",
        routine.name.to_ascii_lowercase(),
        params,
        body
    ))
}

fn references_unmacroized_routine(
    e: &IrExpr,
    catalog: &RoutineCatalog,
    macros: &BTreeSet<String>,
) -> bool {
    match e {
        IrExpr::FuncCall { name, args, .. } => {
            let key = name.to_ascii_lowercase();
            (catalog.lookup(name).is_some() && !macros.contains(&key))
                || args
                    .iter()
                    .any(|a| references_unmacroized_routine(a, catalog, macros))
        }
        IrExpr::Bin { l, r, .. } => {
            references_unmacroized_routine(l, catalog, macros)
                || references_unmacroized_routine(r, catalog, macros)
        }
        IrExpr::SqlFn { args, .. } => args
            .iter()
            .any(|a| references_unmacroized_routine(a, catalog, macros)),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand
                .as_deref()
                .is_some_and(|x| references_unmacroized_routine(x, catalog, macros))
                || whens.iter().any(|(c, v)| {
                    references_unmacroized_routine(c, catalog, macros)
                        || references_unmacroized_routine(v, catalog, macros)
                })
                || else_expr
                    .as_deref()
                    .is_some_and(|x| references_unmacroized_routine(x, catalog, macros))
        }
        IrExpr::Not(x) | IrExpr::IsNull(x) => references_unmacroized_routine(x, catalog, macros),
        IrExpr::Lookup { .. } => true,
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } | IrExpr::CursorAttr { .. } => {
            false
        }
    }
}
