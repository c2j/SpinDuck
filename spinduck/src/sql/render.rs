//! IR 表达式 → DuckDB SQL 文本（用于把纯表达式 PL 函数登记为内核宏，§8.2 路径③）。
//!
//! 只覆盖"可无副作用表达"的 IR 子集；记录字段、游标属性等上下文相关节点返回
//! `None`（调用方据此拒绝宏登记，绝不静默降级）。

use crate::ir::{BinOp, IrExpr};
use crate::rules::RuleRegistry;
use crate::value::SpinValue;

/// 渲染失败原因（调用方据此决定拒绝或走其它分派路径）。
pub fn expr_to_sql(e: &IrExpr, rules: &RuleRegistry) -> Option<String> {
    match e {
        IrExpr::Const(v) => const_to_sql(v),
        IrExpr::Var(v) => Some(v.0.clone()),
        IrExpr::Bin { op, l, r } => {
            let ls = expr_to_sql(l, rules)?;
            let rs = expr_to_sql(r, rules)?;
            Some(format!("({ls} {} {rs})", bin_op_sql(*op)))
        }
        IrExpr::Not(x) => Some(format!("(NOT {})", expr_to_sql(x, rules)?)),
        IrExpr::IsNull(x) => Some(format!("({} IS NULL)", expr_to_sql(x, rules)?)),
        IrExpr::FuncCall { name, args } => {
            let target = rules
                .functions
                .get(&name.to_ascii_uppercase())
                .map(|s| s.as_str())
                .unwrap_or(name);
            // `@xxx` 是解析级变换标记，不是 SQL 函数名 → 不可直接渲染
            if target.starts_with('@') {
                return None;
            }
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(expr_to_sql(a, rules)?);
            }
            Some(format!(
                "{}({})",
                target.to_ascii_lowercase(),
                parts.join(", ")
            ))
        }
        // 记录字段、游标属性依赖运行期帧状态，SQL 文本无对应物
        IrExpr::Field { .. } | IrExpr::CursorAttr { .. } => None,
    }
}

fn bin_op_sql(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Concat => "||",
        BinOp::Eq => "=",
        BinOp::Ne => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
    }
}

fn const_to_sql(v: &SpinValue) -> Option<String> {
    match v {
        SpinValue::Null => Some("NULL".to_string()),
        SpinValue::Bool(b) => Some(if *b { "TRUE" } else { "FALSE" }.to_string()),
        SpinValue::Int(i) => Some(i.to_string()),
        SpinValue::Decimal(unscaled, _precision, scale) => Some(decimal_literal(*unscaled, *scale)),
        SpinValue::Str(s) => Some(format!("'{}'", s.replace('\'', "''"))),
        // BLOB/RECORD 无字面量语法 → 不可渲染
        SpinValue::Blob(_) | SpinValue::Record(_) => None,
    }
}

/// i128 非标度值 → 十进制字面量（scale=0 时无小数点）。
fn decimal_literal(unscaled: i128, scale: u8) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let neg = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let s = scale as usize;
    let body = if digits.len() <= s {
        format!("0.{}{}", "0".repeat(s - digits.len()), digits)
    } else {
        let split = digits.len() - s;
        format!("{}.{}", &digits[..split], &digits[split..])
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::VarName;

    fn var(name: &str) -> IrExpr {
        IrExpr::Var(VarName(name.to_string()))
    }

    fn bin(op: BinOp, l: IrExpr, r: IrExpr) -> IrExpr {
        IrExpr::Bin {
            op,
            l: Box::new(l),
            r: Box::new(r),
        }
    }

    #[test]
    fn should_render_int_literal() {
        let rules = RuleRegistry::default();
        assert_eq!(
            expr_to_sql(&IrExpr::Const(SpinValue::Int(42)), &rules).as_deref(),
            Some("42")
        );
    }

    #[test]
    fn should_escape_single_quote_in_string_literal() {
        let rules = RuleRegistry::default();
        let e = IrExpr::Const(SpinValue::Str("a'b".to_string()));
        assert_eq!(expr_to_sql(&e, &rules).as_deref(), Some("'a''b'"));
    }

    #[test]
    fn should_render_decimal_literal_with_scale() {
        let rules = RuleRegistry::default();
        let e = IrExpr::Const(SpinValue::Decimal(123456, 10, 2));
        assert_eq!(expr_to_sql(&e, &rules).as_deref(), Some("1234.56"));
        let small = IrExpr::Const(SpinValue::Decimal(5, 10, 3));
        assert_eq!(expr_to_sql(&small, &rules).as_deref(), Some("0.005"));
        let neg = IrExpr::Const(SpinValue::Decimal(-5, 10, 3));
        assert_eq!(expr_to_sql(&neg, &rules).as_deref(), Some("-0.005"));
    }

    #[test]
    fn should_render_bin_op_with_parentheses() {
        let rules = RuleRegistry::default();
        let e = bin(BinOp::Mul, var("x"), IrExpr::Const(SpinValue::Int(2)));
        assert_eq!(expr_to_sql(&e, &rules).as_deref(), Some("(x * 2)"));
    }

    #[test]
    fn should_render_concat_and_comparison_as_sql_operators() {
        let rules = RuleRegistry::default();
        let concat = bin(
            BinOp::Or,
            bin(
                BinOp::Concat,
                IrExpr::Const(SpinValue::Str("a".into())),
                var("b"),
            ),
            IrExpr::IsNull(Box::new(var("c"))),
        );
        assert_eq!(
            expr_to_sql(&concat, &rules).as_deref(),
            Some("(('a' || b) OR (c IS NULL))")
        );
    }

    #[test]
    fn should_render_function_call_with_rules_rename() {
        let mut rules = RuleRegistry::default();
        rules
            .functions
            .insert("NVL".to_string(), "coalesce".to_string());
        let e = IrExpr::FuncCall {
            name: "nvl".to_string(),
            args: vec![var("a"), IrExpr::Const(SpinValue::Int(0))],
        };
        assert_eq!(expr_to_sql(&e, &rules).as_deref(), Some("coalesce(a, 0)"));
    }

    #[test]
    fn should_reject_transform_marker_function() {
        let mut rules = RuleRegistry::default();
        rules
            .functions
            .insert("DECODE".to_string(), "@case".to_string());
        let e = IrExpr::FuncCall {
            name: "decode".to_string(),
            args: vec![var("a")],
        };
        assert_eq!(expr_to_sql(&e, &rules), None);
    }

    #[test]
    fn should_reject_record_field() {
        let rules = RuleRegistry::default();
        let field = IrExpr::Field {
            rec: VarName("r".to_string()),
            field: "cjsl".to_string(),
        };
        assert_eq!(expr_to_sql(&field, &rules), None);
    }
}
