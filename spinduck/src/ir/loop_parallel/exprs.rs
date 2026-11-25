//! 表达式/语句的读写位置抽取与纯度审计（门禁 1 表达式级 + 门禁 2 定值分析共用）。
//!
//! `Loc` 是字段粒度位置：`Var(v)` 整变量，`Field{rec,field}` 记录字段。
//! `Var(record)` 对记录变量的读/写按设计保守口径处理——读整记录视作
//! 读取其全部字段（见 `proof.rs` 的新鲜度判定）。

use crate::catalog::{ParamMode, RoutineCatalog};
use crate::ir::{AssignTarget, BindSrc, IrExpr, IrProg, IrStmt, VarName};
use std::collections::BTreeSet;

/// 帧位置（定值分析粒度）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Loc {
    Var(VarName),
    Field { rec: VarName, field: String },
}

impl Loc {
    pub(crate) fn var(v: &VarName) -> Self {
        Loc::Var(v.clone())
    }

    pub(crate) fn field(rec: &VarName, field: &str) -> Self {
        Loc::Field {
            rec: rec.clone(),
            field: field.to_string(),
        }
    }
}

/// 语句的写位置（定值）。`into`/`targets`/`out_targets` 与赋值目标同口径。
///
/// `CallRoutine` 的 `out_targets` 是**语法位**（`ir/stmt.rs` 对每个可写
/// 实参都填位，编译期不知 callee 签名），而运行时 `interp/routine.rs`
/// 只对 `ParamMode::Out`/`InOut` 形参写回。此处按 catalog 形参 mode
/// 过滤，与运行时口径对齐；callee 未注册或形参表短于实参位时保守全计。
pub(crate) fn stmt_defs(stmt: &IrStmt, catalog: &RoutineCatalog) -> Vec<Loc> {
    if let IrStmt::CallRoutine {
        name, out_targets, ..
    } = stmt
    {
        return out_targets
            .iter()
            .enumerate()
            .filter(|(i, t)| {
                t.is_some()
                    && catalog
                        .lookup(name)
                        .and_then(|r| r.params.get(*i))
                        .is_none_or(|p| p.mode != ParamMode::In)
            })
            .filter_map(|(_, t)| t.as_ref())
            .map(target_loc)
            .collect();
    }
    let targets: Vec<&AssignTarget> = match stmt {
        IrStmt::Assign { target, .. } => vec![target],
        IrStmt::SqlQuery { into, .. } => into.iter().collect(),
        IrStmt::SqlExecDynamic { into_targets, .. } => into_targets.iter().collect(),
        IrStmt::Fetch { targets, .. } => targets.iter().collect(),
        IrStmt::FoldedQuery { target, .. } => vec![target],
        _ => Vec::new(),
    };
    targets.iter().map(|t| target_loc(t)).collect()
}

/// `AssignTarget` → `Loc`。
pub(crate) fn target_loc(t: &AssignTarget) -> Loc {
    match t {
        AssignTarget::Var(v) => Loc::var(v),
        AssignTarget::Field { rec, field } => Loc::field(rec, field),
    }
}

/// 语句的读位置（定值分析用；表达式内 `FuncCall`/`SqlFn` 参数同样计入）。
pub(crate) fn stmt_uses(stmt: &IrStmt) -> Vec<Loc> {
    let mut out = Vec::new();
    match stmt {
        IrStmt::Assign { expr, .. } | IrStmt::Perform { expr } => {
            expr_uses(expr, &mut out);
        }
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => {
            expr_uses(cond, &mut out);
        }
        IrStmt::Raise {
            message: Some(expr),
        }
        | IrStmt::Return { value: Some(expr) } => {
            expr_uses(expr, &mut out);
        }
        IrStmt::SqlQuery { binds, .. } | IrStmt::SqlExec { binds, .. } => {
            for (_, src) in binds {
                bind_uses(src, &mut out);
            }
        }
        IrStmt::SqlExecDynamic { text_expr, .. } => expr_uses(text_expr, &mut out),
        IrStmt::CallRoutine { args, .. } => {
            for a in args {
                expr_uses(a, &mut out);
            }
        }
        IrStmt::FoldedQuery { expr, .. } => expr_uses(expr, &mut out),
        _ => {}
    }
    out
}

/// 表达式读位置：`Var`/`Field` 直接计；`FuncCall`/`SqlFn`/`Lookup` 的参数
/// 子表达式递归计入（函数本身的帧读写由更保守的路径处理）。
fn expr_uses(expr: &IrExpr, out: &mut Vec<Loc>) {
    match expr {
        IrExpr::Const(_) => {}
        IrExpr::Var(v) => out.push(Loc::var(v)),
        IrExpr::Field { rec, field } => out.push(Loc::field(rec, field)),
        IrExpr::Bin { l, r, .. } => {
            expr_uses(l, out);
            expr_uses(r, out);
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_uses(e, out),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(e) = operand {
                expr_uses(e, out);
            }
            for (cond, value) in whens {
                expr_uses(cond, out);
                expr_uses(value, out);
            }
            if let Some(e) = else_expr {
                expr_uses(e, out);
            }
        }
        IrExpr::FuncCall { args, .. } | IrExpr::SqlFn { args, .. } => {
            for a in args {
                expr_uses(a, out);
            }
        }
        IrExpr::CursorAttr { .. } => {}
        IrExpr::Lookup { args, default, .. } => {
            for (_, a) in args {
                expr_uses(a, out);
            }
            expr_uses(default, out);
        }
    }
}

fn bind_uses(src: &BindSrc, out: &mut Vec<Loc>) {
    match src {
        BindSrc::Var(v) => out.push(Loc::var(v)),
        BindSrc::Field { rec, field } => out.push(Loc::field(rec, field)),
        // `Expr` 绑定不含帧变量读（lookup 内部文法，解释器不产生）。
        BindSrc::Expr { .. } => {}
    }
}

/// 表达式是否含 `CursorAttr`（任意游标、任意属性种类）。
pub(crate) fn expr_has_cursor_attr(expr: &IrExpr) -> bool {
    match expr {
        IrExpr::CursorAttr { .. } => true,
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => false,
        IrExpr::Bin { l, r, .. } => expr_has_cursor_attr(l) || expr_has_cursor_attr(r),
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_has_cursor_attr(e),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_deref().is_some_and(expr_has_cursor_attr)
                || whens
                    .iter()
                    .any(|(c, v)| expr_has_cursor_attr(c) || expr_has_cursor_attr(v))
                || else_expr.as_deref().is_some_and(expr_has_cursor_attr)
        }
        IrExpr::FuncCall { args, .. } | IrExpr::SqlFn { args, .. } => {
            args.iter().any(expr_has_cursor_attr)
        }
        IrExpr::Lookup { args, default, .. } => {
            args.iter().any(|(_, a)| expr_has_cursor_attr(a)) || expr_has_cursor_attr(default)
        }
    }
}

/// 语句的表达式部分是否含 `CursorAttr`（Assign/Perform/JumpIf/Raise/
/// Return/SqlExecDynamic/FoldedQuery/CallRoutine 参数均覆盖）。
pub(crate) fn stmt_has_cursor_attr(stmt: &IrStmt) -> bool {
    match stmt {
        IrStmt::Assign { expr, .. } | IrStmt::Perform { expr } => expr_has_cursor_attr(expr),
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => expr_has_cursor_attr(cond),
        IrStmt::Raise {
            message: Some(expr),
        }
        | IrStmt::Return { value: Some(expr) } => expr_has_cursor_attr(expr),
        IrStmt::SqlExecDynamic { text_expr, .. } => expr_has_cursor_attr(text_expr),
        IrStmt::CallRoutine { args, .. } => args.iter().any(expr_has_cursor_attr),
        IrStmt::FoldedQuery { expr, .. } => expr_has_cursor_attr(expr),
        _ => false,
    }
}

/// 收集全程序中出现的记录变量名（`Field{rec}` 的 rec + 显式记录目标），
/// 用于门禁 2 的整记录保守口径。
pub(crate) fn record_vars(prog: &IrProg) -> BTreeSet<VarName> {
    let mut out = BTreeSet::new();
    for stmt in &prog.code {
        match stmt {
            IrStmt::Assign {
                target: AssignTarget::Field { rec, .. },
                ..
            } => {
                out.insert(rec.clone());
            }
            IrStmt::Assign { expr, .. }
            | IrStmt::Perform { expr }
            | IrStmt::FoldedQuery { expr, .. } => expr_field_recs(expr, &mut out),
            IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => {
                expr_field_recs(cond, &mut out);
            }
            _ => {}
        }
    }
    out
}

fn expr_field_recs(expr: &IrExpr, out: &mut BTreeSet<VarName>) {
    match expr {
        IrExpr::Field { rec, .. } => {
            out.insert(rec.clone());
        }
        IrExpr::Bin { l, r, .. } => {
            expr_field_recs(l, out);
            expr_field_recs(r, out);
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_field_recs(e, out),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(e) = operand {
                expr_field_recs(e, out);
            }
            for (c, v) in whens {
                expr_field_recs(c, out);
                expr_field_recs(v, out);
            }
            if let Some(e) = else_expr {
                expr_field_recs(e, out);
            }
        }
        IrExpr::FuncCall { args, .. } | IrExpr::SqlFn { args, .. } => {
            for a in args {
                expr_field_recs(a, out);
            }
        }
        IrExpr::Lookup { args, default, .. } => {
            for (_, a) in args {
                expr_field_recs(a, out);
            }
            expr_field_recs(default, out);
        }
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::CursorAttr { .. } => {}
    }
}
