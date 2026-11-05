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
        // 记录字段、游标属性依赖运行期帧状态，SQL 文本无对应物；
        // SqlFn（@变换标记）在宏登记场景无内核变换可用 → 拒绝宏化（PL 上下文
        // 由解释器 SqlFn 下沉路径承担）
        IrExpr::Field { .. } | IrExpr::CursorAttr { .. } | IrExpr::SqlFn { .. } => None,
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

pub(crate) fn const_to_sql(v: &SpinValue) -> Option<String> {
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

/// 渲染 IR 表达式为 SQL 文本并收集变量/记录字段绑定（供 [`crate::ir::IrExpr::SqlFn`]
/// 运行期下沉使用）。与 [`expr_to_sql`] 的差异：变量输出命名占位符而非裸名。
/// 游标属性等帧态节点返回 `None`。
pub fn expr_to_sql_binds(e: &IrExpr) -> Option<(String, Vec<(String, crate::ir::BindSrc)>)> {
    let mut binds = Vec::new();
    let text = render_binds(e, &mut binds)?;
    Some((text, binds))
}

fn render_binds(e: &IrExpr, binds: &mut Vec<(String, crate::ir::BindSrc)>) -> Option<String> {
    use crate::ir::BindSrc;
    match e {
        IrExpr::Const(v) => const_to_sql(v),
        IrExpr::Var(v) => {
            let ph = BindSrc::placeholder(&v.0, None);
            binds.push((ph.clone(), BindSrc::Var(v.clone())));
            Some(ph)
        }
        IrExpr::Field { rec, field } => {
            let ph = BindSrc::placeholder(&rec.0, Some(field));
            binds.push((
                ph.clone(),
                BindSrc::Field {
                    rec: rec.clone(),
                    field: field.to_ascii_lowercase(),
                },
            ));
            Some(ph)
        }
        IrExpr::Bin { op, l, r } => {
            let ls = render_binds(l, binds)?;
            let rs = render_binds(r, binds)?;
            // Oracle 语义：`日期 ± 数字 n` = 加/减 n 天。渲染期对"一侧为日期
            // 表达式、另一侧为纯数值绑定/字面量"的加减，将数字侧包 to_days()
            // 以满足 DuckDB +(TIMESTAMP, INTERVAL)（语料 step4 日期算术形态）
            if matches!(op, crate::ir::BinOp::Add | crate::ir::BinOp::Sub) {
                let dateish = |x: &str| x.contains("strptime(") || x.contains("to_date(");
                let plain_num = |x: &str| x.starts_with("$v_") || x.parse::<i128>().is_ok();
                // 绑定占位符经 DECIMAL 推断，to_days 需要 INTEGER → 显式 CAST
                let as_days = |x: &str| {
                    if x.starts_with("$v_") {
                        format!("to_days(CAST({x} AS INTEGER))")
                    } else {
                        format!("to_days({x})")
                    }
                };
                if dateish(&ls) && plain_num(&rs) {
                    return Some(format!("({ls} {} {})", bin_op_sql(*op), as_days(&rs)));
                }
                if *op == crate::ir::BinOp::Add && plain_num(&ls) && dateish(&rs) {
                    return Some(format!("({} + {rs})", as_days(&ls)));
                }
            }
            Some(format!("({ls} {} {rs})", bin_op_sql(*op)))
        }
        IrExpr::Not(x) => Some(format!("(NOT {})", render_binds(x, binds)?)),
        IrExpr::IsNull(x) => Some(format!("({} IS NULL)", render_binds(x, binds)?)),
        IrExpr::FuncCall { name, args } | IrExpr::SqlFn { name, args } => {
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(render_binds(a, binds)?);
            }
            let inner = parts.join(", ");
            if name.is_empty() {
                Some(inner) // 仅参数列表形态（e_sqlfn_args 包装）
            } else {
                Some(format!("{}({})", name.to_ascii_lowercase(), inner))
            }
        }
        IrExpr::CursorAttr { .. } => None,
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
