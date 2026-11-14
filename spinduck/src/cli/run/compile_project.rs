//! 项目态 `compile`：一次 codeweb 导出，按入口落计划，再对闭包源做 IR 体检。
//!
//! 不调用 `plan::closure::build`。二进制在导出里惰性解析（项目发现失败不必先有
//! codeweb）；找不到就返回，不退回词法闭包。

use super::compile::rules_for_paths;
use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::{read_source_strict, NoopExecutor};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::codeweb::{plans_from_export, AmbientCommandExport, CodewebExport};
use crate::plan::{write_plan, PlanWrite, RunPlan};
use crate::project::ProjectConfig;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `compile` 的项目旗标。路径模式不走这里。
pub struct ProjectCompile {
    /// 项目目录；缺省为当前工作目录。
    pub dir: Option<PathBuf>,
    /// 只写入计划的 schema；不参与入口匹配、不参与 `deploy`。
    pub schema: Option<String>,
    /// 显式入口（可重复）；为空时取项目声明的全部 `[[entry]]`。
    pub entries: Vec<String>,
    /// codeweb 二进制路径（`--codeweb-bin`），优先于环境变量与 PATH。
    pub codeweb_bin: Option<PathBuf>,
    /// codeweb 项目根（`--codeweb-project`），覆盖 `[paths].codeweb` 与向上发现。
    pub codeweb_project: Option<PathBuf>,
    /// 同步运行态快照（`conf/spinduck.runtime.toml`），默认关。
    pub sync_manifest: bool,
}

pub(super) fn compile_project_cmd(
    opts: ProjectCompile,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let (project, entries) = locate_project(&opts)?;
    if opts.sync_manifest {
        sync_manifest_line(&project, out)?;
    }
    let export = AmbientCommandExport {
        flag: opts.codeweb_bin,
    };
    compile_with_export(&project, &entries, opts.schema, &export, out)
}

/// 注入导出的项目态 compile：测试不启动 codeweb，其余口径与真二进制一致。
pub fn compile_project_with_export(
    opts: ProjectCompile,
    export: &impl CodewebExport,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let (project, entries) = locate_project(&opts)?;
    if opts.sync_manifest {
        sync_manifest_line(&project, out)?;
    }
    compile_with_export(&project, &entries, opts.schema, export, out)
}

fn locate_project(opts: &ProjectCompile) -> Result<(ProjectConfig, Vec<String>), EngineError> {
    let start = match &opts.dir {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(|e| EngineError::env(12, format!("cwd: {e}")))?,
    };
    let mut project = ProjectConfig::locate(&start)?;
    if let Some(dir) = &opts.codeweb_project {
        project.codeweb_dir = Some(dir.clone());
    }
    let entries = resolve_entries(&project, &opts.entries)?;
    Ok((project, entries))
}

/// 运行态快照同步（§11.1 `conf/`），渲染为 `manifest:` 行。
///
/// 只在研发态目录（含手写 `spinduck.toml`）写入：从运行态目录执行时不回写，
/// 快照的来源标注因此始终指向真源。没有 `[[entry]]` 声明就不写快照：
/// 运行态不能声称一个没被点名的可调用面。
pub(crate) fn sync_manifest_line(
    project: &ProjectConfig,
    out: &mut dyn Write,
) -> Result<(), EngineError> {
    if project.loaded_as_runtime || !project.root.join("spinduck.toml").is_file() {
        let path = project
            .home_dir
            .join("conf")
            .join(crate::project::SNAPSHOT_FILE);
        writeln!(
            out,
            "manifest: {} (未改写：本目录即运行态快照)",
            path.display()
        )
        .map_err(io_err)?;
        return Ok(());
    }
    project_entries(project)?;
    let s = crate::project::sync_runtime_manifest(project)?;
    writeln!(
        out,
        "manifest: {} ({})",
        s.path.display(),
        if s.changed { "updated" } else { "unchanged" }
    )
    .map_err(io_err)?;
    if let Some(legacy) = s.removed_legacy {
        writeln!(out, "manifest: 已删除旧名快照 {}", legacy.display()).map_err(io_err)?;
    }
    Ok(())
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

/// 项目声明里的全部入口。`--sync-manifest` 与不带 `--entry` 的项目态 compile 共用。
pub(crate) fn project_entries(project: &ProjectConfig) -> Result<Vec<String>, EngineError> {
    resolve_entries(project, &[])
}

fn resolve_entries(
    project: &ProjectConfig,
    explicit: &[String],
) -> Result<Vec<String>, EngineError> {
    if !explicit.is_empty() {
        // 显式 --entry 只做归一（trim + 小写，与 walk.rs 的入口处理同口径）：
        // 独立例程用裸名，包内例程用全名。包限定校验只属于 [[entry]] 声明。
        return Ok(explicit
            .iter()
            .map(|name| name.trim().to_ascii_lowercase())
            .collect());
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
