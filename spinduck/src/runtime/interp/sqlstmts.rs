//! SQL 类 IR 语句执行（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。
//!
//! 行数语义（SELECT INTO / FETCH INTO 同一口径）：0 行 NO_DATA_FOUND、>1 行 TOO_MANY_ROWS、
//! INTO 目标多于列 BIZ-0126。静态与动态两条路径共用 `write_into`，防止语义再次分叉。

use crate::error::EngineError;
use crate::exec::{BindMap, TraceEvent};
use crate::ir::{AssignTarget, BindSrc, IrExpr};
use crate::runtime::eval::{eval, EvalEnv};
use crate::runtime::interp::binds::resolve_binds;
use crate::runtime::interp::dynamic::{materialize_fn_sites, prepare_dynamic_sql};
use crate::runtime::interp::{assign, Env};
use crate::value::SpinValue;

/// 静态 SELECT INTO。
pub(super) fn query(
    env: &mut Env<'_>,
    sql: &str,
    binds: &[(String, BindSrc)],
    into: &[AssignTarget],
    sites: &[crate::sql::FnSite],
) -> Result<(), EngineError> {
    materialize_fn_sites(sites, env)?;
    let bound = resolve_binds(binds, env.frame)?;
    env.sink.on_event(TraceEvent::SqlExec {
        sql: sql.to_string(),
        binds: bound.clone(),
        elapsed_us: 0,
    });
    let mut stream = env.ex.query(sql, &bound)?;
    let rows = stream.fetch_many(2);
    write_into(env, into, &rows)
}

/// DML/DDL 执行。
pub(super) fn exec(
    env: &mut Env<'_>,
    sql: &str,
    binds: &[(String, BindSrc)],
    sites: &[crate::sql::FnSite],
) -> Result<(), EngineError> {
    materialize_fn_sites(sites, env)?;
    let bound = resolve_binds(binds, env.frame)?;
    env.sink.on_event(TraceEvent::SqlExec {
        sql: sql.to_string(),
        binds: bound.clone(),
        elapsed_us: 0,
    });
    env.ex.execute(sql, &bound)?;
    Ok(())
}

/// EXECUTE IMMEDIATE（可选 INTO：与静态 SELECT INTO 同语义）。
pub(super) fn exec_dynamic(
    env: &mut Env<'_>,
    text_expr: &IrExpr,
    into_targets: &[AssignTarget],
) -> Result<(), EngineError> {
    let v = eval_expr(env, text_expr)?;
    let (sql, _) = prepare_dynamic_sql(&v.as_str_loose(), env)?;
    env.sink.on_event(TraceEvent::SqlExec {
        sql: sql.clone(),
        binds: BindMap::new(),
        elapsed_us: 0,
    });
    if into_targets.is_empty() {
        env.ex.execute(&sql, &BindMap::new())?;
        return Ok(());
    }
    // INTO：与静态 SELECT INTO 同语义（0/1/>1 行）
    let mut stream = env.ex.query(&sql, &BindMap::new())?;
    let rows = stream.fetch_many(2);
    write_into(env, into_targets, &rows)
}

/// 0/1/>1 行 → 异常或写槽（静态与动态共用）。
fn write_into(
    env: &mut Env<'_>,
    into: &[AssignTarget],
    rows: &[Vec<SpinValue>],
) -> Result<(), EngineError> {
    if rows.is_empty() {
        return Err(EngineError::no_data_found());
    }
    // fetch_many(2) 的取行上界是接缝约定：越界属内核异常，显式报错而非 panic（§5）
    if rows.len() > 2 {
        return Err(EngineError::invariant(33, "fetch_many(2) returned >2 rows"));
    }
    if rows.len() > 1 {
        return Err(EngineError::too_many_rows());
    }
    let row = &rows[0];
    if into.len() > row.len() {
        return Err(EngineError::business(
            126,
            format!(
                "INTO targets ({}) exceed columns ({})",
                into.len(),
                row.len()
            ),
        ));
    }
    for (i, target) in into.iter().enumerate() {
        let v = row.get(i).cloned().unwrap_or(SpinValue::Null);
        assign(env, target, v)?;
    }
    Ok(())
}

fn eval_expr(env: &mut Env<'_>, expr: &IrExpr) -> Result<SpinValue, EngineError> {
    let mut env_ = EvalEnv {
        frame: env.frame,
        ex: env.ex,
        cat: env.cat,
        depth: env.depth,
        sink: &mut *env.sink,
    };
    eval(expr, &mut env_)
}
