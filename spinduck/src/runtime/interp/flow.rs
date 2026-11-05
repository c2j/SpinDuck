//! 控制流与事务类 IR 语句执行（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::exec::BindMap;
use crate::ir::{AssignTarget, IrExpr};
use crate::runtime::eval::{eval, eval_cond, EvalEnv};
use crate::runtime::interp::{assign, Env, RunOutcome};
use crate::value::SpinValue;

/// COMMIT / ROLLBACK：即时作用于当前事务并重开（最外层统一边界见 `engine::call`）；
/// 不再是空操作——异常处理器中的 ROLLBACK 由此具备撤销已执行 DML 的语义。
pub(super) fn tx(env: &mut Env<'_>, commit: bool) -> Result<(), EngineError> {
    env.ex
        .execute(if commit { "COMMIT" } else { "ROLLBACK" }, &BindMap::new())?;
    env.ex.execute("BEGIN TRANSACTION", &BindMap::new())?;
    Ok(())
}

/// `:=` 赋值：求值后写槽。
pub(super) fn assign_step(
    env: &mut Env<'_>,
    target: &AssignTarget,
    expr: &IrExpr,
) -> Result<(), EngineError> {
    let v = eval_expr(env, expr)?;
    assign(env.frame, target, v)
}

/// `PERFORM <expr>`：求值并丢弃。
pub(super) fn perform(env: &mut Env<'_>, expr: &IrExpr) -> Result<(), EngineError> {
    eval_expr(env, expr)?;
    Ok(())
}

/// 三值逻辑条件求值（JumpIf / JumpIfNot 共用）。
pub(super) fn cond(env: &Env<'_>, expr: &IrExpr) -> Result<Option<bool>, EngineError> {
    eval_cond(expr, &eval_env(env))
}

/// `RAISE <message>`：构造业务错误（BIZ-0200）交回主循环的 trap 展开。
pub(super) fn raise(env: &Env<'_>, message: &IrExpr) -> Result<EngineError, EngineError> {
    let v = eval_expr(env, message)?;
    Ok(EngineError::business(200, v.as_str_loose()))
}

/// `RETURN [<expr>]`。
pub(super) fn return_step(
    env: &Env<'_>,
    value: &Option<IrExpr>,
) -> Result<RunOutcome, EngineError> {
    let v = match value {
        Some(e) => Some(eval_expr(env, e)?),
        None => None,
    };
    Ok(RunOutcome::Returned(v))
}

/// 表达式求值（帧的不可变借用与写槽的可变借用分离，故收敛为单一入口）。
fn eval_expr(env: &Env<'_>, expr: &IrExpr) -> Result<SpinValue, EngineError> {
    eval(expr, &eval_env(env))
}

/// 求值环境（帧 + 接缝 + 目录 + 深度）。
fn eval_env<'e>(env: &'e Env<'_>) -> EvalEnv<'e> {
    EvalEnv {
        frame: env.frame,
        ex: env.ex,
        cat: env.cat,
        depth: env.depth,
    }
}
