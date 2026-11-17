//! Phase 2 执行分发：按预检序列逐步执行，`DbPool`/`EngineCache` 复用。
//!
//! 纪律：
//! - 缺件门/feature 门/trace 冲突已在 Phase 1 拒完，这里不再重复；
//! - 写步（deploy/materialize/query/sync）后 `invalidate_engines_for`；
//! - `call` 每步前重查 `kernel_objects`（编目新鲜度不依赖缓存状态）；
//! - `--continue-on-error`：步失败打印 `[CODE] msg` 后继续，末尾汇总退出码。

use super::preflight::Step;
use super::session::{rules_hash, source_hash, BatchSession, DbKey, EngineKey};
use crate::cli::args::Command;
use crate::cli::run::call::{self, CallRun};
use crate::cli::run::{self, EXIT_FAIL, EXIT_OK};
use crate::error::EngineError;
use std::io::Write;

/// 顺序执行全部步骤；返回批级退出码（任一步失败 → 1）。
pub fn run_steps(
    steps: &[Step],
    session: &mut BatchSession,
    continue_on_error: bool,
    out: &mut dyn Write,
) -> i32 {
    let mut any_fail = false;
    for (i, step) in steps.iter().enumerate() {
        match run_step(step, i + 1, session, out) {
            Ok(code) if code == EXIT_OK => {}
            Ok(_) => {
                any_fail = true;
                if !continue_on_error {
                    return EXIT_FAIL;
                }
            }
            Err(e) => {
                any_fail = true;
                let _ = writeln!(out, "[{}] step {}: {}", e.code, step.line_no, e.message);
                if !continue_on_error {
                    return EXIT_FAIL;
                }
            }
        }
    }
    if any_fail {
        EXIT_FAIL
    } else {
        EXIT_OK
    }
}

fn run_step(
    step: &Step,
    seq: usize,
    session: &mut BatchSession,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    match &step.cmd {
        Command::Call { .. } => run_call(step, seq, session, out),
        Command::Deploy { plan, db } => run_deploy(plan, db, session, out),
        Command::Query {
            db,
            sql_file,
            out: out_path,
        } => run_query(db, sql_file, out_path, session, out),
        #[cfg(feature = "gauss")]
        Command::Materialize { .. } => run_materialize(step, session, out),
        #[cfg(feature = "gauss")]
        Command::Sync { .. } => run_sync(step, session, out),
        other => run::exec(other.clone(), out),
    }
}

/// `call` 步：db=None → 匿名内存（每步独立）；db=Some → 池化 + EngineCache。
fn run_call(
    step: &Step,
    seq: usize,
    session: &mut BatchSession,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let parts = call::cmd_call(step.cmd.clone());
    let prep = match call::prepare_call(&parts, out)? {
        Some(p) => p,
        None => return Ok(EXIT_FAIL),
    };
    let proc = format!("step{seq:02}:{}", prep.target);
    let trace = parts.trace_spec(proc);

    let Some(db_path) = parts.db_path() else {
        // 匿名 :memory:：独立连接 + 独立引擎（不进任何缓存——跨步不可见）。
        let ex = crate::sql::DuckExecutor::open_in_memory()?;
        let objects = crate::verify::kernel_objects(&ex)?;
        let mut eng = crate::engine::Engine::new(Box::new(ex), prep.rules.clone());
        eng.attach_compiled_cache(session.compiled_cache());
        return call::call_engine(
            &mut eng,
            &prep,
            &CallRun {
                args: parts.args(),
                batch_commit: parts.batch_commit(),
                step_budget: parts.step_budget(),
                register_sources: true,
                source_texts: None,
                objects: &objects,
                trace,
            },
            out,
        );
    };

    let dbkey = DbKey::of(db_path);
    // 源内容哈希（非路径）：call --plan 的 sources 已含计划例程 + --file。
    let mut hashes = Vec::with_capacity(prep.sources.len());
    for f in &prep.sources {
        hashes.push(source_hash(f)?);
    }
    let key = EngineKey::new(dbkey.clone(), rules_hash(&prep.rules), hashes);

    // 内核对象每步重查（deploy/materialize/query 后编目已变；成本是目录读）。
    let objects = {
        let ex = session.canonical(&dbkey)?;
        crate::verify::kernel_objects(ex)?
    };

    if session.engine_hit(&key)? {
        let eng = session
            .engine_mut(&key)
            .ok_or_else(|| EngineError::invariant(20, "engine cache 命中后取不到引擎"))?;
        return call::call_engine(
            eng,
            &prep,
            &CallRun {
                args: parts.args(),
                batch_commit: parts.batch_commit(),
                step_budget: parts.step_budget(),
                register_sources: false,
                source_texts: None,
                objects: &objects,
                trace,
            },
            out,
        );
    }

    // 未命中：新引擎（克隆连接 + 共享编译缓存），call_engine 内注册源；
    // 仅在返回 Ok（含业务性 EXIT_FAIL）后回填——Err 意味注册可能半途，弃引擎。
    let mut eng = session.build_engine(&dbkey, prep.rules.clone())?;
    let code = call::call_engine(
        &mut eng,
        &prep,
        &CallRun {
            args: parts.args(),
            batch_commit: parts.batch_commit(),
            step_budget: parts.step_budget(),
            register_sources: true,
            source_texts: None,
            objects: &objects,
            trace,
        },
        out,
    )?;
    session.insert_engine(key, eng);
    Ok(code)
}

/// `deploy` 步：开库前准备 → 引擎缓存复用（命中跳过编译、DDL 仍每次执行）。
fn run_deploy(
    plan: &std::path::Path,
    db: &std::path::Path,
    session: &mut BatchSession,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let Some(prep) = crate::cli::deploy::deploy_prepare(plan, db, out)? else {
        return Ok(EXIT_FAIL);
    };
    let dbkey = DbKey::of(db);
    let mut hashes = Vec::with_capacity(prep.routine_sources().len());
    for f in prep.routine_sources() {
        hashes.push(source_hash(f)?);
    }
    let key = EngineKey::new(dbkey.clone(), rules_hash(prep.rules()), hashes);

    if session.engine_hit(&key)? {
        let eng = session
            .engine_mut(&key)
            .ok_or_else(|| EngineError::invariant(20, "engine cache 命中后取不到引擎"))?;
        let code = crate::cli::deploy::deploy_apply(&prep, eng, out)?;
        session.invalidate_engines_for(&dbkey);
        return Ok(code);
    }
    let mut eng = session.build_engine(&dbkey, prep.rules().clone())?;
    let code = crate::cli::deploy::deploy_apply(&prep, &mut eng, out)?;
    session.insert_engine(key, eng);
    session.invalidate_engines_for(&dbkey);
    Ok(code)
}

/// `query` 步：读 SQL → 池化连接执行 → 登记 `--out` 产物 + 写后失效。
fn run_query(
    db: &std::path::Path,
    sql_file: &std::path::Path,
    out_path: &std::path::Path,
    session: &mut BatchSession,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let text = crate::cli::read_source_strict(sql_file)?;
    let dbkey = DbKey::of(db);
    let code = {
        let ex = session.canonical(&dbkey)?;
        crate::cli::query::query_with_executor(ex, &text, out_path, out)?
    };
    session.record_artifact(out_path.to_path_buf());
    session.invalidate_engines_for(&dbkey);
    Ok(code)
}

#[cfg(feature = "gauss")]
fn run_materialize(
    step: &Step,
    session: &mut BatchSession,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let parts = run::io::cmd_materialize(step.cmd.clone());
    let dbkey = DbKey::of(&parts.db);
    let ex = session.clone_executor(&dbkey)?;
    let code = run::io::materialize_with_executor(&parts, &ex, out)?;
    session.invalidate_engines_for(&dbkey);
    Ok(code)
}

#[cfg(feature = "gauss")]
fn run_sync(
    step: &Step,
    session: &mut BatchSession,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let parts = run::io::cmd_sync(step.cmd.clone());
    // 顺序契约：resolve_mode + GaussDB env/连接先于 clone_executor
    // ——缺 env/坏 mode 时不得留下空 .duckdb（与单步 sync_cmd 同序）。
    let (mode, mut og) = run::io::sync_prepare(&parts)?;
    let dbkey = DbKey::of(&parts.db);
    let ex = session.clone_executor(&dbkey)?;
    let code = run::io::sync_with_executor(&parts, &mode, &mut og, &ex, out)?;
    session.invalidate_engines_for(&dbkey);
    Ok(code)
}

#[cfg(all(test, feature = "gauss"))]
mod tests {
    use super::*;

    /// `sync` 的 GaussDB 前置（resolve_mode/env）必须先于
    /// 任何 DuckDB open——前置失败时库文件不得落盘（与单步 sync_cmd 同序）。
    #[test]
    fn should_not_create_db_file_when_sync_precheck_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("w.duckdb");
        let step = Step {
            line_no: 1,
            cmd: Command::Sync {
                db: db.clone(),
                table: "t".to_string(),
                cols: vec!["a".to_string()],
                where_clause: None,
                mode: Some("bogus".to_string()), // resolve_mode 即拒，先于 open_gauss
                slice: None,
                key: None,
            },
        };
        let mut session = BatchSession::new();
        let mut out: Vec<u8> = Vec::new();
        let code = run_steps(&[step], &mut session, false, &mut out);
        assert_eq!(code, EXIT_FAIL);
        assert!(
            !db.exists(),
            "sync 前置失败不得创建库文件: {}",
            String::from_utf8_lossy(&out)
        );
    }
}
