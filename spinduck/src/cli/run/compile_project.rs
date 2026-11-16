//! 项目态 `compile`：一次取图（进程内嵌 codeweb），按入口落计划，再对闭包源做 IR 体检。
//!
//! 不调用 `plan::closure::build`；取图失败直接返回，不退回词法闭包。
//! `--analyze` 允许在取图前于进程内完成增量分析。

use super::compile::rules_for_paths;
use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::{read_source_strict, NoopExecutor};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::codeweb::{plans_from_source, CodewebSource, EmbeddedCodewebSource};
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
    /// 取图前允许进程内分析（`--analyze`）：store 缺失/过期/源码变动时先补分析。
    pub analyze: bool,
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
    let source = EmbeddedCodewebSource {
        auto_analyze: opts.analyze,
    };
    compile_with_source(&project, &entries, opts.schema, &source, out)
}

/// 注入取图来源的项目态 compile：测试注入内存 JSON，其余口径与嵌入式生产一致。
pub fn compile_project_with_source(
    opts: ProjectCompile,
    source: &impl CodewebSource,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let (project, entries) = locate_project(&opts)?;
    if opts.sync_manifest {
        sync_manifest_line(&project, out)?;
    }
    compile_with_source(&project, &entries, opts.schema, source, out)
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

/// 计划落盘与退出码。`source` 由调用方注入（生产 = 嵌入实现，测试 = JSON 夹具）。
pub(crate) fn compile_with_source(
    project: &ProjectConfig,
    entries: &[String],
    schema: Option<String>,
    source: &impl CodewebSource,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let plans = plans_from_source(project, entries, schema, source)?;
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
///
/// Gate 1B/2A（设计 V4 Stage 2–4）：对闭包内例程+表/视图 DDL 文件建
/// `SchemaSnapshot` 并做对象/列引用检查，发现以 `BIZ-0152` 计入
/// `failed`；与 `RunPlan[[missing]]` 重复的 relation 缺失去重（闭包
/// 域已报的不重复报）。Gate 2B（Stub 库 prepare）二期接入。
fn ir_failures(
    project: &ProjectConfig,
    plans: &[RunPlan],
) -> Result<Vec<(String, EngineError)>, EngineError> {
    let rels: BTreeSet<&str> = plans
        .iter()
        .flat_map(|plan| plan.ddl_routines.iter().map(String::as_str))
        .collect();
    let mut failed = Vec::new();
    // Gate 2B 输入：本文件新注册例程的改写后 SQL 与下沉临时表名
    let mut gate_sqls: Vec<(String, String)> = Vec::new();
    let mut fn_tables: BTreeSet<String> = BTreeSet::new();
    if !rels.is_empty() {
        let paths: Vec<PathBuf> = rels.iter().map(|rel| project.root.join(rel)).collect();
        let rules = rules_for_paths(&paths)?;
        let mut eng = Engine::new(Box::new(NoopExecutor), rules.clone());
        for rel in &rels {
            let before: BTreeSet<String> = eng.catalog.names().into_iter().collect();
            if let Some(err) = register_file(&mut eng, &project.root.join(rel)) {
                failed.push((rel.to_string(), err));
                continue;
            }
            for name in eng
                .catalog
                .names()
                .into_iter()
                .filter(|n| !before.contains(n))
            {
                if let Some(r) = eng.catalog.lookup(&name) {
                    collect_prog_sqls(rel, &r.prog, &mut gate_sqls, &mut fn_tables);
                }
            }
        }
        let (gate_errs, snapshot) = gate_failures(project, plans);
        failed.extend(gate_errs);
        if let Some(snap) = snapshot {
            // `[[missing]]` 已报的 relation 与 Gate 1B 延迟对象同口径抑制，
            // 避免 prepare 对同一缺失重复裁决（KERNEL-0011 vs BIZ-0152）
            for p in plans {
                for m in &p.missing {
                    if let Some(n) = m.object.strip_prefix("relation:") {
                        fn_tables.insert(n.to_string());
                    }
                }
            }
            failed.extend(crate::gate::prepare::prepare_all(
                &snap, &rules, &fn_tables, &gate_sqls,
            ));
        }
    } else {
        failed.extend(gate_failures(project, plans).0);
    }
    failed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(failed)
}

/// 从例程 IR 收集 Gate 2B 输入：静态 SQL 文本（归属文件）与
/// 例程调用下沉临时表名（不建模对象，prepare 报错命中即抑制）。
fn collect_prog_sqls(
    rel: &str,
    prog: &crate::ir::IrProg,
    sqls: &mut Vec<(String, String)>,
    fn_tables: &mut BTreeSet<String>,
) {
    for st in &prog.code {
        let (sql, sites) = match st {
            crate::ir::IrStmt::SqlExec { sql, fn_sites, .. }
            | crate::ir::IrStmt::SqlQuery { sql, fn_sites, .. } => (sql, fn_sites),
            _ => continue,
        };
        sqls.push((rel.to_string(), sql.clone()));
        for s in sites {
            fn_tables.insert(s.table.clone());
        }
    }
}

/// Gate Stage 1–3：闭包内全部 SQL 文件（例程 + 表/视图 DDL）的
/// 解析、快照与引用检查。读失败的文件跳过（例程文件的读错误已由
/// register_file 报出；纯 DDL 文件读错误为边角，V1 不单独归因）。
fn gate_failures(
    project: &ProjectConfig,
    plans: &[RunPlan],
) -> (
    Vec<(String, EngineError)>,
    Option<crate::gate::schema::SchemaSnapshot>,
) {
    let gate_rels: BTreeSet<&str> = plans
        .iter()
        .flat_map(|plan| {
            plan.ddl_routines
                .iter()
                .chain(plan.ddl_tables.iter())
                .map(String::as_str)
        })
        .collect();
    let files: Vec<crate::gate::pipeline::GateFile> = gate_rels
        .iter()
        .filter_map(|rel| {
            read_source_strict(&project.root.join(rel)).ok().map(|sql| {
                crate::gate::pipeline::GateFile {
                    rel: rel.to_string(),
                    sql,
                }
            })
        })
        .collect();
    let run = crate::gate::pipeline::run(&files);
    let mut out = Vec::new();
    if let Some((rel, err)) = run.schema_error {
        out.push((rel, err));
        return (out, None);
    }
    let missing: BTreeSet<&str> = plans
        .iter()
        .flat_map(|p| p.missing.iter())
        .map(|m| m.object.as_str())
        .collect();
    for f in run.findings {
        if covered_by_plan_missing(&f, &missing) {
            continue;
        }
        out.push((
            f.file.clone(),
            EngineError::business(152, format!("missing {} object `{}`", f.kind, f.object)),
        ));
    }
    (out, run.snapshot)
}

/// Gate 1B 对象级发现与 `[[missing]]` 去重：闭包域已报的
/// `relation:<名>` 不重复报；单段名按后缀口径匹配（与
/// `check::known` 的 suffix 规则一致）。列级发现（kind 非
/// `"table"`）不参与去重。
fn covered_by_plan_missing(
    f: &crate::gate::pipeline::GateFinding,
    missing: &BTreeSet<&str>,
) -> bool {
    if f.kind != "table" {
        return false;
    }
    missing.iter().any(|m| {
        let Some(name) = m.strip_prefix("relation:") else {
            return false;
        };
        name == f.object
            || (!f.object.contains('.') && name.rsplit('.').next() == Some(f.object.as_str()))
    })
}

fn register_file(eng: &mut Engine, path: &Path) -> Option<EngineError> {
    match read_source_strict(path) {
        Ok(text) => eng.register(&text).err(),
        Err(err) => Some(err),
    }
}
