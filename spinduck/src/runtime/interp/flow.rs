//! 控制流与事务类 IR 语句执行（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::exec::BindMap;
use crate::ir::{AssignTarget, IrExpr};
use crate::runtime::eval::{eval, eval_cond, EvalEnv};
use crate::runtime::interp::{assign, Env, RunOutcome};
use crate::value::SpinValue;

/// 处理器进入前的会话复位：内核语句失败会作废整个显式事务（DuckDB 无 SAVEPOINT），
/// 不复位则处理器体内的 SQL 全部连锁报 "Current transaction is aborted"。
/// 与 Oracle 语句级回滚的已知差异：上次 COMMIT 之后的本段 DML 一并撤销。
pub(super) fn reset_session(env: &mut Env<'_>) -> Result<(), EngineError> {
    if let Err(e) = env.ex.execute("ROLLBACK", &BindMap::new()) {
        // 无活动事务（不经 engine::call 直接 run）时的回滚失败可忽略
        if !e.message.contains("no transaction is active") {
            return Err(e);
        }
    }
    env.ex.execute("BEGIN TRANSACTION", &BindMap::new())?;
    Ok(())
}

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
    assign(env, target, v)
}

/// `PERFORM <expr>`：求值并丢弃。
pub(super) fn perform(env: &mut Env<'_>, expr: &IrExpr) -> Result<(), EngineError> {
    eval_expr(env, expr)?;
    Ok(())
}

/// 三值逻辑条件求值（JumpIf / JumpIfNot 共用）。
pub(super) fn cond(env: &mut Env<'_>, expr: &IrExpr) -> Result<Option<bool>, EngineError> {
    let mut e = eval_env(env);
    eval_cond(expr, &mut e)
}

/// `RAISE <message>`：构造业务错误（BIZ-0200）交回主循环的 trap 展开。
pub(super) fn raise(
    env: &mut Env<'_>,
    message: &Option<IrExpr>,
) -> Result<EngineError, EngineError> {
    match message {
        Some(m) => Ok(EngineError::business(
            200,
            eval_expr(env, m)?.as_str_loose(),
        )),
        // 裸 RAISE;：原样重抛处理器当前正在处理的错误（错误码/故障域/消息全保留）。
        // 处理器外没有当前错误可抛 → 显式报错，与 Oracle ORA-00438 同口径。
        None => env
            .current_error
            .clone()
            .ok_or_else(|| EngineError::business(201, "RAISE statement with no active exception")),
    }
}

/// `RETURN [<expr>]`。
pub(super) fn return_step(
    env: &mut Env<'_>,
    value: &Option<IrExpr>,
) -> Result<RunOutcome, EngineError> {
    let v = match value {
        Some(e) => Some(eval_expr(env, e)?),
        None => None,
    };
    Ok(RunOutcome::Returned(v))
}

/// 表达式求值（帧的不可变借用与写槽的可变借用分离，故收敛为单一入口）。
fn eval_expr(env: &mut Env<'_>, expr: &IrExpr) -> Result<SpinValue, EngineError> {
    let mut e = eval_env(env);
    eval(expr, &mut e)
}

/// 求值环境（帧 + 接缝 + 目录 + 深度）。
fn eval_env<'e>(env: &'e mut Env<'_>) -> EvalEnv<'e> {
    EvalEnv {
        frame: env.frame,
        ex: env.ex,
        cat: env.cat,
        depth: env.depth,
        sink: &mut *env.sink,
    }
}
