//! 例程调用（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::catalog::ParamMode;
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::ir::{AssignTarget, IrExpr, VarName};
use crate::runtime::eval::EvalEnv;
use crate::runtime::frame::Frame;
use crate::runtime::interp::{run_with_catalog, Env, RunOutcome};
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
    if args.len() != routine.params.len() {
        return Err(EngineError::business(
            151,
            format!(
                "{} expects {} args, got {}",
                name,
                routine.params.len(),
                args.len()
            ),
        ));
    }
    let mut callee = Frame::new();
    for (p, ir_a) in routine.params.iter().zip(args) {
        let v = {
            let env_ = EvalEnv {
                frame: env.frame,
                ex: env.ex,
                cat: env.cat,
                depth: env.depth,
            };
            crate::runtime::eval::eval(ir_a, &env_)?
        };
        callee.set(&VarName(p.name.to_ascii_lowercase()), v);
    }
    let mut noop = crate::exec::TraceNoopSink;
    let out = run_with_catalog(
        &routine.prog,
        &mut callee,
        env.ex,
        &mut noop,
        env.cat,
        env.depth + 1,
    )?;
    // OUT/INOUT 写回：按参数序对应 out_targets（None = 丢弃）
    for (i, p) in routine.params.iter().enumerate() {
        if p.mode == ParamMode::In {
            continue;
        }
        if let Some(Some(t)) = out_targets.get(i) {
            if let Some(v) = callee.get(&VarName(p.name.to_ascii_lowercase())) {
                crate::runtime::interp::assign(env.frame, t, v.clone())?;
            }
        }
    }
    // 过程的 RETURN <expr> 在过程语境下不产生调用方值（v1）
    let _ = out;
    Ok(())
}

/// 按值调用已注册函数（SQL 上下文下沉用；OUT 写回不适用）。
pub(super) fn call_routine_value(
    cat: &crate::catalog::RoutineCatalog,
    name: &str,
    args: &[SpinValue],
    ex: &dyn SqlExecutor,
    depth: usize,
) -> Result<SpinValue, EngineError> {
    let routine = cat
        .lookup(name)
        .ok_or_else(|| EngineError::business(150, format!("routine {name} not registered")))?
        .clone();
    if args.len() != routine.params.len() {
        return Err(EngineError::business(
            151,
            format!(
                "{} expects {} args, got {}",
                name,
                routine.params.len(),
                args.len()
            ),
        ));
    }
    let mut callee = Frame::new();
    for (p, v) in routine.params.iter().zip(args) {
        callee.set(&VarName(p.name.to_ascii_lowercase()), v.clone());
    }
    let mut noop = crate::exec::TraceNoopSink;
    let out = run_with_catalog(&routine.prog, &mut callee, ex, &mut noop, cat, depth + 1)?;
    Ok(match out {
        RunOutcome::Returned(Some(v)) => v,
        _ => SpinValue::Null,
    })
}
