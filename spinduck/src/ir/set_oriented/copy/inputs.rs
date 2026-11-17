//! VARCHAR 形参安装恒等证明，禁止把字符串比较变成数值比较。

use super::{inline::ProjEnv, CopyLoop, Name, SkipReason};
use crate::ir::{BinOp, IrExpr};
use crate::value::SpinValue;

pub(super) fn prove(region: &CopyLoop<'_>, env: &ProjEnv<'_>) -> Result<(), SkipReason> {
    if region
        .varchar_inputs
        .iter()
        .all(|expr| string_value(expr, region, env))
    {
        Ok(())
    } else {
        Err(SkipReason::UnsupportedCall)
    }
}

fn string_value(expr: &IrExpr, region: &CopyLoop<'_>, env: &ProjEnv<'_>) -> bool {
    match expr {
        IrExpr::Const(value) => matches!(value, SpinValue::Str(_) | SpinValue::Null),
        IrExpr::Field { rec, field } => rec == region.record && string_field(field, env),
        IrExpr::Var(var) => {
            if let Some((_, value)) = region.folded.iter().find(|(v, _)| v == var) {
                return matches!(value, SpinValue::Str(_) | SpinValue::Null);
            }
            env.lookups
                .iter()
                .find(|lookup| &lookup.var == var)
                .is_some_and(|lookup| {
                    let hit = match &lookup.case_sink {
                        Some(case) => {
                            string_field(&case.else_field.0, env)
                                && case
                                    .whens
                                    .iter()
                                    .all(|(_, field)| string_field(&field.0, env))
                        }
                        None => lookup.kind == "varchar",
                    };
                    hit && (lookup.filter_on_miss || string_literal(&lookup.render.default_sql))
                })
        }
        IrExpr::Bin {
            op: BinOp::Concat, ..
        } => true,
        IrExpr::SqlFn { name, prepared, .. } => {
            prepared.is_ok() && name.trim_start_matches('@').eq_ignore_ascii_case("to_char")
        }
        IrExpr::Case {
            whens, else_expr, ..
        } => {
            whens
                .iter()
                .all(|(_, value)| string_value(value, region, env))
                && else_expr
                    .as_deref()
                    .is_none_or(|value| string_value(value, region, env))
        }
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => {
            prepared.is_ok()
                && matches!(name.to_ascii_lowercase().as_str(), "substr" | "substring")
                && matches!(args.as_slice(), [value, IrExpr::Const(SpinValue::Int(start)), IrExpr::Const(SpinValue::Int(len))]
                    if string_value(value, region, env) && (0..=i32::MAX as i64).contains(start) && (0..=i32::MAX as i64).contains(len))
        }
        IrExpr::Bin { .. }
        | IrExpr::Not(_)
        | IrExpr::IsNull(_)
        | IrExpr::Lookup { .. }
        | IrExpr::CursorAttr { .. } => false,
    }
}

pub(super) fn total(expr: &IrExpr, region: &CopyLoop<'_>, env: &ProjEnv<'_>) -> bool {
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => true,
        IrExpr::Bin {
            op: BinOp::Concat,
            l,
            r,
        } => total(l, region, env) && total(r, region, env),
        IrExpr::FuncCall { args, .. } => {
            string_value(expr, region, env) && args.iter().all(|arg| total(arg, region, env))
        }
        _ => false,
    }
}

fn string_field(field: &str, env: &ProjEnv<'_>) -> bool {
    super::source_column(env.select, env.source, &Name(field.into()))
        .is_ok_and(|column| column.kind == "varchar")
}

fn string_literal(sql: &str) -> bool {
    sql.eq_ignore_ascii_case("NULL") || (sql.starts_with('\'') && sql.ends_with('\''))
}
