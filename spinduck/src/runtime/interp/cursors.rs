//! 游标类 IR 语句执行（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::exec::{BindMap, TraceEvent};
use crate::ir::{AssignTarget, CursorSource, IrExpr, VarName};
use crate::runtime::cursor::CursorState;
use crate::runtime::eval::{eval, EvalEnv};
use crate::runtime::interp::binds::resolve_binds;
use crate::runtime::interp::dynamic::{materialize_fn_sites, prepare_dynamic_sql};
use crate::runtime::interp::{assign, Env};
use crate::value::SpinValue;

/// OPEN 游标：静态源沿用改写产物，动态源与 EXECUTE IMMEDIATE 同一条管线。
pub(super) fn open(
    env: &mut Env<'_>,
    cursor: &VarName,
    source: &CursorSource,
) -> Result<(), EngineError> {
    let (sql, binds) = match source {
        CursorSource::Static {
            sql,
            binds,
            fn_sites,
        } => {
            materialize_fn_sites(fn_sites, env)?;
            let m = resolve_binds(binds, env.frame)?;
            (sql.clone(), m)
        }
        CursorSource::Dynamic { query_expr } => {
            // 动态 OPEN FOR：与 EXECUTE IMMEDIATE 同一条管线（统一收口）
            let v = eval_text(env, query_expr)?;
            let (sql, _) = prepare_dynamic_sql(&v.as_str_loose(), env)?;
            (sql, BindMap::new())
        }
    };
    let stream = env.ex.query(&sql, &binds)?;
    env.frame.open_cursor(cursor, CursorState::open(stream));
    let schema = env.frame.cursor_schema(cursor);
    env.sink.on_event(TraceEvent::CursorOpen { sql, schema });
    Ok(())
}

/// FETCH INTO：位置对位写槽；流尾不覆盖目标；FOR-record 形态整体建 RECORD。
pub(super) fn fetch(
    env: &mut Env<'_>,
    cursor: &VarName,
    targets: &[AssignTarget],
    record: &bool,
) -> Result<(), EngineError> {
    let fetch_schema = env.frame.cursor_schema(cursor);
    let row = env.frame.cursor_mut(cursor)?.fetch()?;
    match row {
        Some(r) if *record => {
            // FOR rec IN <query>：整行按 schema 建 RECORD（GaussDB 语义）
            let fields = fetch_schema
                .iter()
                .cloned()
                .zip(r.iter().cloned())
                .collect::<Vec<_>>();
            let target = targets.first().ok_or_else(|| {
                EngineError::invariant(47, "FOR-record fetch requires one INTO target")
            })?;
            assign(env.frame, target, SpinValue::Record(fields))?;
            env.sink.on_event(TraceEvent::Fetch { row: Some(1) });
        }
        Some(r) => {
            // 位置对位写槽；INTO 目标多于列 → 报错
            if targets.len() > r.len() {
                return Err(EngineError::business(
                    126,
                    format!(
                        "FETCH INTO targets ({}) exceed columns ({})",
                        targets.len(),
                        r.len()
                    ),
                ));
            }
            // %ROWTYPE 记录目标：按结果 schema 建 RECORD（首取时，§7.3.2）
            for t in targets {
                if let crate::ir::AssignTarget::Field { rec, .. } = t {
                    env.frame.ensure_record(rec, &fetch_schema);
                }
            }
            for (i, target) in targets.iter().enumerate() {
                assign(env.frame, target, r[i].clone())?;
            }
            env.sink.on_event(TraceEvent::Fetch { row: Some(1) });
        }
        None => {
            env.sink.on_event(TraceEvent::Fetch { row: None });
        }
    }
    Ok(())
}

/// CLOSE 游标。
pub(super) fn close(env: &mut Env<'_>, cursor: &VarName) -> Result<(), EngineError> {
    env.frame.close_cursor(cursor)?;
    Ok(())
}

/// 表达式求值（游标动态文本用）。
fn eval_text(env: &mut Env<'_>, expr: &IrExpr) -> Result<SpinValue, EngineError> {
    let mut env_ = EvalEnv {
        frame: env.frame,
        ex: env.ex,
        cat: env.cat,
        depth: env.depth,
        sink: &mut *env.sink,
    };
    eval(expr, &mut env_)
}
