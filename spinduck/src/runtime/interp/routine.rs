//! 例程调用（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::catalog::ParamMode;
use crate::error::EngineError;
use crate::ir::{AssignTarget, IrExpr, VarName};
use crate::runtime::eval::EvalEnv;
use crate::runtime::frame::Frame;
use crate::runtime::interp::{run_with_catalog, Env, RunLimits, RunOutcome, DEFAULT_STEP_BUDGET};
use crate::value::SpinValue;

/// CALL 例程：形参按名入被调帧，OUT/INOUT 按 `out_targets` 位置写回调用方。
pub(super) fn call(
    env: &mut Env<'_>,
    name: &str,
    args: &[IrExpr],
    out_targets: &[Option<AssignTarget>],
) -> Result<(), EngineError> {
    if env.depth > 100 {
        return Err(EngineError::invariant(63, "call depth exceeded (100)"));
    }
    let routine = env
        .cat
        .lookup(name)
        .ok_or_else(|| EngineError::business(150, format!("routine {name} not registered")))?
        .clone();
    crate::catalog::check_arity(name, &routine.params, args.len())?;
    let callee_args = eval_args(env, &routine.params, args)?;
    let mut callee = Frame::new();
    callee.install_params(
        &routine.params,
        &callee_args,
        env.cat.sql_rules().implicit_assignment_loose,
    )?;
    let out = run_with_catalog(
        &routine.prog,
        &mut callee,
        env.ex,
        &mut *env.sink,
        env.cat,
        env.depth + 1,
        RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: env.batch,
        },
    )?;
    // OUT/INOUT 写回：按参数序对应 out_targets（None = 丢弃）
    for (i, p) in routine.params.iter().enumerate() {
        if p.mode == ParamMode::In {
            continue;
        }
        if let Some(Some(t)) = out_targets.get(i) {
            if let Some(v) = callee.get(&VarName(p.name.to_ascii_lowercase())) {
                crate::runtime::interp::assign(env, t, v.clone())?;
            }
        }
    }
    // 过程的 RETURN <expr> 在过程语境下不产生调用方值（v1）
    let _ = out;
    Ok(())
}

/// 位置实参求值。省略的尾部实参取声明默认值（元数校验已排除其余形态）。
fn eval_args(
    env: &mut Env<'_>,
    params: &[crate::catalog::RoutineParam],
    args: &[IrExpr],
) -> Result<Vec<SpinValue>, EngineError> {
    let mut out = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        let v = match args.get(i) {
            Some(ir_a) => {
                let mut env_ = EvalEnv {
                    frame: env.frame,
                    ex: env.ex,
                    cat: env.cat,
                    depth: env.depth,
                    sink: &mut *env.sink,
                };
                crate::runtime::eval::eval(ir_a, &mut env_)?
            }
            None => p.default.clone().unwrap_or(SpinValue::Null),
        };
        out.push(v);
    }
    Ok(out)
}

/// 按值调用已注册函数（SQL 上下文下沉用；OUT 写回不适用）。
///
/// 环境整体传入而非拆成四个接缝参数：被下沉例程的埋点必须沿父链写回同一个汇。
pub(super) fn call_routine_value(
    env: &mut Env<'_>,
    name: &str,
    args: &[SpinValue],
) -> Result<SpinValue, EngineError> {
    let routine = env
        .cat
        .lookup(name)
        .ok_or_else(|| EngineError::business(150, format!("routine {name} not registered")))?
        .clone();
    crate::catalog::check_arity(name, &routine.params, args.len())?;
    let mut callee = Frame::new();
    callee.install_params(
        &routine.params,
        args,
        env.cat.sql_rules().implicit_assignment_loose,
    )?;
    let out = run_with_catalog(
        &routine.prog,
        &mut callee,
        env.ex,
        &mut *env.sink,
        env.cat,
        env.depth + 1,
        RunLimits::default(),
    )?;
    Ok(match out {
        RunOutcome::Returned(Some(v)) => v,
        _ => SpinValue::Null,
    })
}
