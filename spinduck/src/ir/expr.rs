//! 表达式编译与原子量（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;
use ogsql_parser::ast::Expr;

impl Compiler {
    pub(crate) fn expr(&mut self, e: &Expr) -> Result<IrExpr, EngineError> {
        match e {
            Expr::Literal(l) => Ok(IrExpr::Const(lit(l))),
            Expr::PlVariable(name) => {
                let path = ident_path(name);
                let up = path.to_ascii_uppercase();
                if up == "SQLERRM" || up == "SQLCODE" {
                    return Ok(IrExpr::Var(VarName(format!(
                        "__{}",
                        up.to_ascii_lowercase()
                    ))));
                }
                Ok(IrExpr::Var(VarName(path)))
            }
            Expr::ColumnRef(name) => {
                if name.len() == 2 {
                    Ok(IrExpr::Field {
                        rec: VarName(ident0(name)),
                        field: ident1(name),
                    })
                } else {
                    let path = ident_path(name);
                    let up = path.to_ascii_uppercase();
                    if up == "SQLERRM" || up == "SQLCODE" {
                        return Ok(IrExpr::Var(VarName(format!(
                            "__{}",
                            up.to_ascii_lowercase()
                        ))));
                    }
                    // 单段标识符在 PL 表达式中仍按变量解释（§3.2 消歧同源）
                    Ok(IrExpr::Var(VarName(path)))
                }
            }
            Expr::BinaryOp { left, op, right } => Ok(IrExpr::Bin {
                op: binop(op)?,
                l: Box::new(self.expr(left)?),
                r: Box::new(self.expr(right)?),
            }),
            Expr::UnaryOp { op, expr } if op.eq_ignore_ascii_case("NOT") => {
                Ok(IrExpr::Not(Box::new(self.expr(expr)?)))
            }
            Expr::IsNull { expr, negated } => {
                let inner = self.expr(expr)?;
                Ok(if *negated {
                    IrExpr::Not(Box::new(IrExpr::IsNull(Box::new(inner))))
                } else {
                    IrExpr::IsNull(Box::new(inner))
                })
            }
            Expr::CursorAttribute { cursor, attribute } => {
                let cname = match cursor.as_ref() {
                    Expr::PlVariable(n) | Expr::ColumnRef(n) => VarName(ident_path(n)),
                    other => {
                        return Err(EngineError::invariant(
                            46,
                            format!("unsupported cursor reference: {other:?}"),
                        ));
                    }
                };
                let kind = match attribute {
                    ogsql_parser::ast::CursorAttributeKind::NotFound => CursorAttrKind::NotFound,
                    ogsql_parser::ast::CursorAttributeKind::Found => CursorAttrKind::Found,
                    ogsql_parser::ast::CursorAttributeKind::IsOpen => CursorAttrKind::IsOpen,
                    ogsql_parser::ast::CursorAttributeKind::RowCount => CursorAttrKind::RowCount,
                    ogsql_parser::ast::CursorAttributeKind::BulkExceptions => {
                        return Err(EngineError::invariant(
                            47,
                            "BULK_EXCEPTIONS cursor attribute is not supported",
                        ));
                    }
                };
                Ok(IrExpr::CursorAttr {
                    cursor: cname,
                    kind,
                })
            }
            Expr::FunctionCall { name, args, .. } => {
                // 规则改名（如 NVL→coalesce）：只对非 `@` 变换标记生效；`@` 标记的
                // 解析级变换在内建函数下沉时由 apply_ast_transforms 处理
                let raw = ident_path(name);
                let upper = raw.to_ascii_uppercase();
                let mapped = self
                    .rules
                    .functions
                    .get(&upper)
                    .filter(|v| !v.starts_with('@'))
                    .cloned()
                    .unwrap_or_else(|| raw.clone());
                let compiled: Vec<IrExpr> = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Result<Vec<_>, _>>()?;
                if self
                    .rules
                    .functions
                    .get(&upper)
                    .map(|v| v.starts_with('@'))
                    .unwrap_or(false)
                {
                    // @变换标记（@to_char/@to_date/@intertonum…）：SQL-first 下沉
                    let prepared = prepare_scalar_sql(&mapped, &compiled);
                    return Ok(IrExpr::SqlFn {
                        name: mapped,
                        args: compiled,
                        prepared,
                    });
                }
                Ok(IrExpr::function_call(mapped, compiled))
            }
            Expr::Parenthesized(inner) => self.expr(inner),
            Expr::UnaryOp { op, expr } if op == "-" => Ok(IrExpr::Bin {
                op: BinOp::Sub,
                l: Box::new(IrExpr::Const(SpinValue::Int(0))),
                r: Box::new(self.expr(expr)?),
            }),
            Expr::UnaryOp { op, expr } if op == "+" => self.expr(expr),
            // BETWEEN 闭区间（§3.2 求值口径与 SQL 一致）：NOT BETWEEN 为区间外
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let x = self.expr(expr)?;
                let lo = self.expr(low)?;
                let hi = self.expr(high)?;
                let ge = IrExpr::Bin {
                    op: BinOp::Ge,
                    l: Box::new(x.clone()),
                    r: Box::new(lo),
                };
                let le = IrExpr::Bin {
                    op: BinOp::Le,
                    l: Box::new(x.clone()),
                    r: Box::new(hi),
                };
                if *negated {
                    let lt = IrExpr::Bin {
                        op: BinOp::Lt,
                        l: Box::new(x.clone()),
                        r: Box::new(self.expr(low)?),
                    };
                    let gt = IrExpr::Bin {
                        op: BinOp::Gt,
                        l: Box::new(x),
                        r: Box::new(self.expr(high)?),
                    };
                    Ok(or_chain(vec![lt, gt]))
                } else {
                    Ok(and_chain(vec![ge, le]))
                }
            }
            // IN 列表：等式析取（NOT IN 为全部不等）
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let x = self.expr(expr)?;
                // 空表语义（Oracle）：x IN () = FALSE，x NOT IN () = TRUE
                if list.is_empty() {
                    return Ok(IrExpr::Const(SpinValue::Bool(*negated)));
                }
                let mut parts = Vec::with_capacity(list.len());
                for item in list {
                    let eq = IrExpr::Bin {
                        op: if *negated { BinOp::Ne } else { BinOp::Eq },
                        l: Box::new(x.clone()),
                        r: Box::new(self.expr(item)?),
                    };
                    parts.push(eq);
                }
                Ok(if *negated {
                    and_chain(parts)
                } else {
                    or_chain(parts)
                })
            }
            Expr::Case {
                operand,
                whens,
                else_expr,
            } => Ok(IrExpr::Case {
                operand: operand
                    .as_ref()
                    .map(|e| self.expr(e).map(Box::new))
                    .transpose()?,
                whens: whens
                    .iter()
                    .map(|w| Ok((self.expr(&w.condition)?, self.expr(&w.result)?)))
                    .collect::<Result<_, EngineError>>()?,
                else_expr: else_expr
                    .as_ref()
                    .map(|e| self.expr(e).map(Box::new))
                    .transpose()?,
            }),
            // 其余表达式（LIKE/子查询…）M2 不支持；SQL 侧表达式的
            // 大多数场景发生在 SQL 文本内（M3 改写管线），PL 侧按需逐里程碑扩展
            other => Err(EngineError::invariant(
                26,
                format!("PL expression not yet supported in M2 evaluator: {other:?}"),
            )),
        }
    }
}

fn prepare_scalar_sql(name: &str, args: &[IrExpr]) -> Result<ScalarSql, EngineError> {
    let mut parts = Vec::with_capacity(args.len());
    let mut binds = Vec::new();
    for arg in args {
        let (text, sources) = crate::sql::render::expr_to_sql_binds(arg).ok_or_else(|| {
            EngineError::invariant(64, "SqlFn argument not renderable (cursor attribute?)")
        })?;
        parts.push(text);
        binds.extend(sources);
    }
    let sql = format!("SELECT {}({})", name.to_ascii_lowercase(), parts.join(", "));
    Ok(ScalarSql {
        sql: crate::sql::apply_ast_transforms(&sql)?,
        binds,
    })
}

/// 条件链左结合组合（OR）：`a OR b OR c`。
pub(crate) fn or_chain(parts: Vec<IrExpr>) -> IrExpr {
    fold_bin(BinOp::Or, parts)
}

/// 条件链左结合组合（AND）：`a AND b AND c`。
pub(crate) fn and_chain(parts: Vec<IrExpr>) -> IrExpr {
    fold_bin(BinOp::And, parts)
}

pub(crate) fn fold_bin(op: BinOp, parts: Vec<IrExpr>) -> IrExpr {
    let mut it = parts.into_iter();
    // 空链在 InList 脱糖处已先行短路；此处兜底为中性值（ENG 防御，不 panic）
    let Some(first) = it.next() else {
        return IrExpr::Const(SpinValue::Bool(op == BinOp::And));
    };
    it.fold(first, |acc, next| IrExpr::Bin {
        op,
        l: Box::new(acc),
        r: Box::new(next),
    })
}

pub(crate) fn lit(l: &ogsql_parser::ast::Literal) -> SpinValue {
    use ogsql_parser::ast::Literal as L;
    match l {
        L::Integer(i) => SpinValue::Int(*i),
        // 小数按十进制字面量承载（禁止降级为字符串：`x * 0.0008` 曾因字符串
        // 常量在 DuckDB 侧类型推断失败）
        L::Float(f) => match parse_decimal_literal(f) {
            Some(v) => v,
            None => SpinValue::Str(f.clone()),
        },
        L::String(f) | L::EscapeString(f) | L::NationalString(f) => SpinValue::Str(f.clone()),
        L::Boolean(b) => SpinValue::Bool(*b),
        L::Null => SpinValue::Null,
        L::BitString(s) | L::HexString(s) | L::DollarString { body: s, .. } => {
            SpinValue::Str(s.clone())
        }
    }
}

/// 十进制字面量 → `SpinValue::Decimal`（非标度值 + scale）；指数形式不支持时返回
/// `None`（调用方降级为字符串，行为显式可测）。
pub(crate) fn parse_decimal_literal(text: &str) -> Option<SpinValue> {
    let t = text.trim();
    if t.contains('e') || t.contains('E') {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (ip, fp) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if ip.is_empty() && fp.is_empty() {
        return None;
    }
    if !ip.chars().all(|c| c.is_ascii_digit()) || !fp.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scale = fp.len().min(18) as u8;
    let digits = format!("{ip}{fp}");
    let unscaled: i128 = digits.parse().ok()?;
    let signed = if neg { -unscaled } else { unscaled };
    Some(SpinValue::Decimal(signed, 38, scale))
}

pub(crate) fn binop(op: &str) -> Result<BinOp, EngineError> {
    Ok(match op.to_ascii_uppercase().as_str() {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "||" => BinOp::Concat,
        "=" => BinOp::Eq,
        "<>" | "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        "AND" => BinOp::And,
        "OR" => BinOp::Or,
        _ => {
            return Err(EngineError::invariant(
                27,
                format!("unsupported operator: {op}"),
            ))
        }
    })
}

pub(crate) fn ident_path(name: &ogsql_parser::ast::ObjectName) -> String {
    name.iter()
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}

pub(crate) fn ident0(name: &ogsql_parser::ast::ObjectName) -> String {
    name.first().map(|i| i.value.clone()).unwrap_or_default()
}

pub(crate) fn ident1(name: &ogsql_parser::ast::ObjectName) -> String {
    name.get(1).map(|i| i.value.clone()).unwrap_or_default()
}
