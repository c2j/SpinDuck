//! 哑表投影仅折叠同类型字面量 CASE，保留关系存在性与行数检查。

use super::{AssignTarget, Compiler, IrExpr, IrStmt};
use ogsql_parser::ast::{Expr, Literal, SelectStatement, SelectTarget, Spanned, Statement};

pub(super) fn emit(compiler: &mut Compiler, sel: &SelectStatement, into: &[AssignTarget]) -> bool {
    let [target] = into else {
        return false;
    };
    let Some((sql, expr)) = fold(compiler, sel) else {
        return false;
    };
    compiler.code.push(IrStmt::FoldedQuery {
        sql,
        target: target.clone(),
        expr,
    });
    true
}

fn fold(compiler: &mut Compiler, sel: &SelectStatement) -> Option<(String, IrExpr)> {
    let [SelectTarget::Expr(expr, _)] = sel.targets.as_slice() else {
        return None;
    };
    constant_kind(expr)?;
    let mut probe = sel.clone();
    probe.into_targets = None;
    probe.targets = vec![SelectTarget::Expr(Expr::Literal(Literal::Integer(1)), None)];
    let sql = ogsql_parser::formatter::SqlFormatter::new()
        .format_statement(&Statement::Select(Spanned::new(probe, None)));
    // 白名单匹配完整格式化语句，任何附加关系、别名或子句均保留原查询。
    if !sql.eq_ignore_ascii_case("SELECT 1 FROM sys_dummy")
        && !sql.eq_ignore_ascii_case("SELECT 1 FROM dual")
    {
        return None;
    }
    Some((sql, compiler.expr(expr).ok()?))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConstantKind {
    Null,
    Bool,
    Int,
    Text,
}

fn constant_kind(expr: &Expr) -> Option<ConstantKind> {
    use ConstantKind::{Bool, Int, Null, Text};
    match expr {
        Expr::Literal(Literal::Null) => Some(Null),
        Expr::Literal(Literal::Boolean(_)) => Some(Bool),
        Expr::Literal(Literal::Integer(_)) => Some(Int),
        Expr::Literal(Literal::String(_)) => Some(Text),
        Expr::Parenthesized(inner) => constant_kind(inner),
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            let mut selector = match operand {
                Some(expr) => Some(constant_kind(expr)?),
                None => None,
            };
            let mut result = match else_expr {
                Some(expr) => constant_kind(expr)?,
                None => Null,
            };
            for when in whens {
                let condition = constant_kind(&when.condition)?;
                if let Some(kind) = selector {
                    let common = common_kind(kind, condition)?;
                    // 文本比较受数据库 collation 影响，不能用 PL 字符串比较替代。
                    if common == Text {
                        return None;
                    }
                    selector = Some(common);
                } else if !matches!(condition, Bool | Null) {
                    return None;
                }
                result = common_kind(result, constant_kind(&when.result)?)?;
            }
            Some(result)
        }
        // 未证明的 AST 形态一律留在 SQL，新增变体也不得自动获得折叠资格。
        _ => None,
    }
}

fn common_kind(left: ConstantKind, right: ConstantKind) -> Option<ConstantKind> {
    match (left, right) {
        (ConstantKind::Null, other) | (other, ConstantKind::Null) => Some(other),
        (left, right) if left == right => Some(left),
        _ => None,
    }
}
