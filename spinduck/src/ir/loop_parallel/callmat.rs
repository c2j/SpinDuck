//! E2 确定性调用物化的静态证明（V8 详设 `loop-parallel-v8/e2`）。
//!
//! `FuncCall`/`SqlFn` 表达式节点在满足下列条件时可进入 `G_pure`：
//! Worker 不执行例程本体，而是按 `(name, 实参值)` / `(sql, 绑定值)`
//! 探测主线程物化的 `CallSnapshot`；未命中经 `NeedKeys` 交权主线程
//! 补执行后重派（任意键依赖按需展开，无需静态 key_dag）。
//!
//! 准入双条件：
//! 1. **例程体纯度**（`routine_materializable`）：callee 传递闭包内
//!    只含无写、无事务、无游标、无动态 SQL、无 `Raise` 的指令；
//!    `SqlQuery`/`FoldedQuery` 只读放行（物化期在主线程实时执行，
//!    无需可快照化），`fn_sites` 非空保守拒绝。
//! 2. **表达式全物化**（`expr_materializable`）：`FuncCall` 仅当
//!    catalog 命中 `Function` 且例程可物化，或未命中且 `prepared`
//!    SQL 就绪（内建下沉）；`SqlFn` 要求 `prepared` 就绪；`Lookup`
//!    一律不物化。嵌套调用递归同规则。
//!
//! 事务锚定/非确定性审计由门禁 5（`proof::scan_*_tx`）在 analyze
//! 后续阶段覆盖——本模块只证"可物化"，不重复其判定。

use crate::catalog::{RoutineCatalog, RoutineKind};
use crate::ir::{IrExpr, IrProg, IrStmt};

/// 例程物化递归深度上限（防 catalog 环）。
const MAX_MAT_DEPTH: usize = 8;

/// 表达式是否可在 Worker 经 `CallSnapshot` 物化求值。
/// `Pure` 叶子（Const/Var/Field/Bin/Case/Not/IsNull）恒真；
/// `CursorAttr` 由 stmt 级审计单独整域拒绝，不在此判定。
pub(crate) fn expr_materializable(expr: &IrExpr, catalog: &RoutineCatalog, depth: usize) -> bool {
    if depth > MAX_MAT_DEPTH {
        return false;
    }
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => true,
        IrExpr::Bin { l, r, .. } => {
            expr_materializable(l, catalog, depth) && expr_materializable(r, catalog, depth)
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_materializable(e, catalog, depth),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand
                .as_deref()
                .is_none_or(|e| expr_materializable(e, catalog, depth))
                && whens.iter().all(|(c, v)| {
                    expr_materializable(c, catalog, depth) && expr_materializable(v, catalog, depth)
                })
                && else_expr
                    .as_deref()
                    .is_none_or(|e| expr_materializable(e, catalog, depth))
        }
        IrExpr::SqlFn { prepared, args, .. } => {
            // eval_sqlfn 只用 prepared.binds（args 不参与求值），
            // 但 args 内的调用同样不会被执行——保守仍要求可物化，
            // 防止未来语义变化引入未审计副作用。
            prepared.is_ok()
                && args
                    .iter()
                    .all(|a| expr_materializable(a, catalog, depth + 1))
        }
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => {
            let args_ok = args
                .iter()
                .all(|a| expr_materializable(a, catalog, depth + 1));
            if !args_ok {
                return false;
            }
            match catalog.lookup(name) {
                // 目录命中 Function：例程体须可物化；prepared 仅
                // 影响"目录未命中"分派，但例程路径不使用它——
                // 保留 prepared 检查无害且与 builtin 分支对齐。
                Some(r) if r.kind == RoutineKind::Function => {
                    prepared.is_ok() && routine_materializable(&r.prog, catalog, depth + 1)
                }
                // Procedure 或其他类别：表达式语境运行时报错，
                // 交权保留原生错误语义。
                Some(_) => false,
                // 内建下沉：prepared SQL 就绪即可（名/SQL 的
                // 事务锚定审计归门禁 5）。
                None => prepared.is_ok(),
            }
        }
        // Lookup（记录字段回退查询）未纳入物化模型。
        IrExpr::Lookup { .. } | IrExpr::CursorAttr { .. } => false,
    }
}

/// 语句的表达式位是否全部可物化（与原 `exprs::stmt_needs_yield_expr`
/// 同位覆盖：Assign/Perform/JumpIf*/Raise/Return/SqlExecDynamic/
/// FoldedQuery/CallRoutine 参数）。
pub(crate) fn stmt_exprs_materializable(
    stmt: &IrStmt,
    catalog: &RoutineCatalog,
    depth: usize,
) -> bool {
    let exprs: Vec<&IrExpr> = match stmt {
        IrStmt::Assign { expr, .. }
        | IrStmt::Perform { expr }
        | IrStmt::FoldedQuery { expr, .. } => vec![expr],
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => vec![cond],
        IrStmt::Raise {
            message: Some(expr),
        }
        | IrStmt::Return { value: Some(expr) } => vec![expr],
        IrStmt::SqlExecDynamic { text_expr, .. } => vec![text_expr],
        IrStmt::CallRoutine { args, .. } => args.iter().collect(),
        _ => Vec::new(),
    };
    exprs.iter().all(|e| expr_materializable(e, catalog, depth))
}

/// 例程体是否可物化：指令白名单 + 表达式递归 + CallRoutine 传递闭包。
/// 物化在主线程预热期实时执行 callee 全文——`SqlQuery` 等只读指令
/// 放行（其确定性由门禁 5 覆盖），一切写/事务/游标/动态/异常外抛
/// 一律拒绝。
pub(crate) fn routine_materializable(
    prog: &IrProg,
    catalog: &RoutineCatalog,
    depth: usize,
) -> bool {
    if depth > MAX_MAT_DEPTH {
        return false;
    }
    prog.code
        .iter()
        .all(|stmt| stmt_materializable(stmt, catalog, depth))
}

fn stmt_materializable(stmt: &IrStmt, catalog: &RoutineCatalog, depth: usize) -> bool {
    match stmt {
        IrStmt::Nop
        | IrStmt::Assign { .. }
        | IrStmt::Perform { .. }
        | IrStmt::Jump(_)
        | IrStmt::JumpIf { .. }
        | IrStmt::JumpIfNot { .. }
        | IrStmt::PushTrap { .. }
        | IrStmt::PopTrap
        | IrStmt::Return { .. } => stmt_exprs_materializable(stmt, catalog, depth),
        // 只读 SQL：物化期主线程实时执行；fn_sites 物化写临时表，
        // 逐键重复执行改变 sd_* 生命周期——保守拒绝。
        IrStmt::SqlQuery { fn_sites, .. } => {
            fn_sites.is_empty() && stmt_exprs_materializable(stmt, catalog, depth)
        }
        IrStmt::FoldedQuery { .. } => stmt_exprs_materializable(stmt, catalog, depth),
        // 嵌套例程调用：callee 须同样可物化（OUT 写回发生在
        // callee 帧内，主线程物化时按参数序完整捕获）。
        IrStmt::CallRoutine { name, .. } => catalog
            .lookup(name)
            .is_some_and(|r| routine_materializable(&r.prog, catalog, depth + 1)),
        // 写/事务/游标/动态/显式异常：一律不可物化。
        IrStmt::SqlExec { .. }
        | IrStmt::SqlExecDynamic { .. }
        | IrStmt::TxCommit
        | IrStmt::TxCommitIfWritten
        | IrStmt::TxRollback
        | IrStmt::OpenCursor { .. }
        | IrStmt::Fetch { .. }
        | IrStmt::CloseCursor { .. }
        | IrStmt::Raise { .. } => false,
    }
}
