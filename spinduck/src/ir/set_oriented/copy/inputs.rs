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
            if env
                .lookups
                .iter()
                .find(|lookup| &lookup.var == var)
                .is_some_and(|lookup| lookup_string(lookup, env, 0))
            {
                return true;
            }
            // 声明类型 Varchar 的槽位（含 IN 形参，`remember_param_types`
            // 已并入 var_types）写入时强制 coerce，值域恒为 str/NULL。
            region.var_types.get(&var.0) == Some(&crate::types::SpinType::Varchar)
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

/// 查找结果变量的字符串值证明：命中侧按 kind/case_sink 判定；
/// 兜底侧字面量直接判，查找变量兜底递归到内层查找（嵌套 Lookup
/// 兜底，依赖拓扑已防环，depth 仅为防御上限）。
fn lookup_string(lookup: &super::lookup::ResolvedLookup, env: &ProjEnv<'_>, depth: usize) -> bool {
    let hit = match &lookup.case_sink {
        Some(case) => {
            matches!(&case.else_value, super::lookup::CaseElse::Field(f) if string_field(&f.0, env))
                && case
                    .whens
                    .iter()
                    .all(|(_, field)| string_field(&field.0, env))
        }
        None => lookup.kind == "varchar",
    };
    hit && (lookup.filter_on_miss
        || match &lookup.default_var {
            Some(dv) if depth < 8 => env
                .lookups
                .iter()
                .find(|d| &d.var == dv)
                .is_some_and(|d| lookup_string(d, env, depth + 1)),
            Some(_) => false,
            None => string_literal(&lookup.render.default_sql),
        })
}

fn string_field(field: &str, env: &ProjEnv<'_>) -> bool {
    super::source_column(env.select, env.source, &Name(field.into()))
        .is_ok_and(|column| column.kind == "varchar")
}

fn string_literal(sql: &str) -> bool {
    sql.eq_ignore_ascii_case("NULL") || (sql.starts_with('\'') && sql.ends_with('\''))
}
