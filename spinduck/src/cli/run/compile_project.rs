//! 项目态 `compile`：一次 codeweb 导出，按入口落计划，再对闭包源做 IR 体检。
//!
//! 不调用 `plan::closure::build`。二进制在导出之前解析；找不到就返回，不退回词法闭包。

use super::compile::rules_for_paths;
use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::plan::{write_plan, PlanWrite};
use crate::cli::verify::locate;
use crate::cli::{read_source_strict, NoopExecutor};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::codeweb::{plans_from_export, resolve_codeweb_bin, CodewebExport, CommandExport};
use crate::plan::RunPlan;
use crate::project::{qualified_routine, ProjectConfig};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `compile` 的项目旗标。路径模式不走这里。
pub(crate) struct ProjectCompile {
    pub dir: Option<PathBuf>,
    pub schema: Option<String>,
    pub entries: Vec<String>,
    pub codeweb_bin: Option<PathBuf>,
    pub codeweb_project: Option<PathBuf>,
}

pub(super) fn compile_project_cmd(
    opts: ProjectCompile,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let start = match &opts.dir {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(|e| EngineError::env(12, format!("cwd: {e}")))?,
    };
    let mut project = locate(&start)?;
    if let Some(dir) = opts.codeweb_project {
        project.codeweb_dir = Some(dir);
    }
    let entries = resolve_entries(&project, &opts.entries)?;
    let env = std::env::var("SPINDUCK_CODEWEB").ok();
    let path_var = std::env::var_os("PATH");
    let bin = resolve_codeweb_bin(
        opts.codeweb_bin.as_deref(),
        env.as_deref(),
        path_var.as_deref(),
    )?;
    let export = CommandExport::new(bin);
    compile_with_export(&project, &entries, opts.schema, &export, out)
}

/// 计划落盘与退出码。`export` 由调用方注入，测试不启动 codeweb。
pub(crate) fn compile_with_export(
    project: &ProjectConfig,
    entries: &[String],
    schema: Option<String>,
    export: &impl CodewebExport,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let plans = plans_from_export(project, entries, schema, export)?;
    let written = write_plans(project, &plans)?;
    render_plans(&plans, &written, out)?;
    let failed = ir_failures(project, &plans)?;
    for (rel, err) in &failed {
        writeln!(out, "FAIL {rel}  [{}] {}", err.code, err.message).map_err(io_err)?;
    }
    let missing: usize = plans.iter().map(|plan| plan.missing.len()).sum();
    let dynamic: usize = plans.iter().map(|plan| plan.dynamic.len()).sum();
    writeln!(
        out,
        "compile: entries={} missing={} failed={} dynamic={}",
        plans.len(),
        missing,
        failed.len(),
        dynamic
    )
    .map_err(io_err)?;
    Ok(if missing == 0 && failed.is_empty() {
        EXIT_OK
    } else {
        EXIT_FAIL
    })
}

fn resolve_entries(
    project: &ProjectConfig,
    explicit: &[String],
) -> Result<Vec<String>, EngineError> {
    if !explicit.is_empty() {
        return explicit
            .iter()
            .map(|name| qualified_routine(name))
            .collect();
    }
    if project.entries.is_empty() {
        return Err(EngineError::env(
            30,
            "项目模式需要至少一个入口（--entry 或 [[entry]]）",
        ));
    }
    Ok(project.entries.iter().map(|e| e.routine.clone()).collect())
}

fn write_plans(project: &ProjectConfig, plans: &[RunPlan]) -> Result<Vec<PlanWrite>, EngineError> {
    let mut written = Vec::with_capacity(plans.len());
    for plan in plans {
        written.push(write_plan(project, plan)?);
    }
    Ok(written)
}

fn render_plans(
    plans: &[RunPlan],
    written: &[PlanWrite],
    out: &mut dyn Write,
) -> Result<(), EngineError> {
    for (plan, slot) in plans.iter().zip(written.iter()) {
        render_one(plan, slot, out)?;
    }
    Ok(())
}

fn render_one(plan: &RunPlan, written: &PlanWrite, out: &mut dyn Write) -> Result<(), EngineError> {
    let state = if written.changed {
        "updated"
    } else {
        "unchanged"
    };
    writeln!(out, "plan: {} ({state})", written.path.display()).map_err(io_err)?;
    writeln!(
        out,
        "plan: target={} routines={} tables={} missing={}",
        plan.target,
        plan.ddl_routines.len(),
        plan.ddl_tables.len(),
        plan.missing.len()
    )
    .map_err(io_err)?;
    for miss in &plan.missing {
        writeln!(out, "MISS {} <- {}", miss.object, miss.from).map_err(io_err)?;
    }
    for r in &plan.refs {
        writeln!(
            out,
            "REF {}:{} {} <- {} @ {}",
            r.kind, r.object, r.role, r.from, r.at
        )
        .map_err(io_err)?;
    }
    for dyn_call in &plan.dynamic {
        writeln!(
            out,
            "DYN {} <- {} @ {}",
            dyn_call.raw, dyn_call.from, dyn_call.at
        )
        .map_err(io_err)?;
    }
    Ok(())
}

/// 闭包源文件共用一个 Engine。失败不改计划，只在摘要之后按体检的 FAIL 行打出。
fn ir_failures(
    project: &ProjectConfig,
    plans: &[RunPlan],
) -> Result<Vec<(String, EngineError)>, EngineError> {
    let rels: BTreeSet<&str> = plans
        .iter()
        .flat_map(|plan| plan.ddl_routines.iter().map(String::as_str))
        .collect();
    if rels.is_empty() {
        return Ok(Vec::new());
    }
    let paths: Vec<PathBuf> = rels.iter().map(|rel| project.root.join(rel)).collect();
    let rules = rules_for_paths(&paths)?;
    let mut eng = Engine::new(Box::new(NoopExecutor), rules);
    let mut failed = Vec::new();
    for rel in rels {
        if let Some(err) = register_file(&mut eng, &project.root.join(rel)) {
            failed.push((rel.to_string(), err));
        }
    }
    Ok(failed)
}

fn register_file(eng: &mut Engine, path: &Path) -> Option<EngineError> {
    match read_source_strict(path) {
        Ok(text) => eng.register(&text).err(),
        Err(err) => Some(err),
    }
}
