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
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

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
    // Gate 2B 需要 native 内核；compile 子命令当前由 cli（⊇native）承载，
    // 此分支为未来非 native 编译路径预留的显式降级标记。
    let gate2b = if cfg!(feature = "native") {
        ""
    } else {
        " gate2b=skipped(no-native)"
    };
    writeln!(
        out,
        "compile: entries={} missing={} failed={} dynamic={}{}",
        plans.len(),
        missing,
        failed.len(),
        dynamic,
        gate2b
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
    // Gate 2B 输入：本文件新注册例程的改写后 SQL（逐语句携带
    // 下沉临时表与按例程归属的 [[missing]] relation 抑制名）
    let mut gate_sqls: Vec<crate::gate::prepare::PrepareStmt> = Vec::new();
    // `[[missing]]` 的 relation 按 `from`（引用例程全名）归属收窄：
    // 只抑制引用方例程的 prepare，不把全计划缺失套到每条语句。
    let mut missing_by_from: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in plans {
        for m in &p.missing {
            if let Some(n) = m.object.strip_prefix("relation:") {
                missing_by_from
                    .entry(m.from.to_ascii_lowercase())
                    .or_default()
                    .insert(n.to_string());
            }
        }
    }
    // 已失败文件：pipeline 不再重复报同一文件的 Gate 2A 拒绝
    let mut failed_rels: BTreeSet<String> = BTreeSet::new();
    if !rels.is_empty() {
        let paths: Vec<PathBuf> = rels.iter().map(|rel| project.root.join(rel)).collect();
        let rules = rules_for_paths(&paths)?;
        // 预读全部源文本：IR 缓存 stamp 需要全量输入，注册也复用同一份文本
        let mut texts: Vec<(String, String)> = Vec::with_capacity(rels.len());
        for rel in &rels {
            match read_source_strict(&project.root.join(rel)) {
                Ok(text) => texts.push((rel.to_string(), text)),
                Err(err) => {
                    failed_rels.insert(rel.to_string());
                    failed.push((rel.to_string(), err));
                }
            }
        }
        let stamp = crate::ir_cache::CompileGateStamp::compute(&texts, &rules);
        let cache_dir = project.root.join(&project.home_dir).join("ir-cache");
        let mut eng = Engine::new(Box::new(NoopExecutor), rules.clone());
        eng.ir_cache = Some(crate::ir_cache::IrCache::open(&cache_dir, &stamp));
        for (rel, text) in &texts {
            let before: BTreeSet<String> = eng.catalog.names().into_iter().collect();
            if let Err(err) = eng.register(text) {
                failed_rels.insert(rel.clone());
                failed.push((rel.clone(), err));
                continue;
            }
            for name in eng
                .catalog
                .names()
                .into_iter()
                .filter(|n| !before.contains(n))
            {
                if let Some(r) = eng.catalog.lookup(&name) {
                    collect_prog_sqls(rel, &name, &r.prog, &missing_by_from, &mut gate_sqls);
                }
            }
        }
        let gate = gate_failures(project, plans, &failed_rels);
        failed.extend(gate.findings);
        failed.extend(gate.rejected);
        if let Some(snap) = gate.snapshot {
            failed.extend(crate::gate::prepare::prepare_all(
                &snap, &rules, &gate_sqls,
            )?);
        }
    } else {
        let gate = gate_failures(project, plans, &failed_rels);
        failed.extend(gate.findings);
        failed.extend(gate.rejected);
    }
    failed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(failed)
}

/// 从例程 IR 收集 Gate 2B 输入：全部静态 SQL 文本（含游标
/// `OpenCursor::Static` 与 `FoldedQuery`）。每条语句携带归属文件、
/// 绑定、变量类型与专属抑制名（本语句 fn_sites 下沉临时表 +
/// `[[missing]]` 中由本例程引用的 relation）。
fn collect_prog_sqls(
    rel: &str,
    routine: &str,
    prog: &crate::ir::IrProg,
    missing_by_from: &BTreeMap<String, BTreeSet<String>>,
    sqls: &mut Vec<crate::gate::prepare::PrepareStmt>,
) {
    use crate::gate::prepare::PrepareStmt;
    use crate::ir::{CursorSource, IrStmt};
    let push = |sqls: &mut Vec<PrepareStmt>,
                sql: &str,
                binds: &[(String, crate::ir::BindSrc)],
                sites: &[crate::sql::FnSite]| {
        let mut skip = missing_by_from
            .get(&routine.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default();
        for s in sites {
            skip.insert(s.table.clone());
        }
        sqls.push(PrepareStmt {
            file: rel.to_string(),
            sql: sql.to_string(),
            binds: binds.to_vec(),
            var_types: prog.var_types.clone(),
            skip,
        });
    };
    for st in &prog.code {
        match st {
            IrStmt::SqlExec {
                sql,
                binds,
                fn_sites,
            } => push(sqls, sql, binds, fn_sites),
            IrStmt::SqlQuery {
                sql,
                binds,
                fn_sites,
                ..
            } => push(sqls, sql, binds, fn_sites),
            IrStmt::FoldedQuery { sql, .. } => push(sqls, sql, &[], &[]),
            IrStmt::OpenCursor {
                source:
                    CursorSource::Static {
                        sql,
                        binds,
                        fn_sites,
                    },
                ..
            } => push(sqls, sql, binds, fn_sites),
            _ => {}
        }
    }
}

/// `gate_failures` 返回值：引用诊断 + Stage 2 快照 + Gate 2A 拒绝。
struct GateOutcome {
    findings: Vec<(String, EngineError)>,
    snapshot: Option<crate::gate::schema::SchemaSnapshot>,
    rejected: Vec<(String, EngineError)>,
}

/// Gate Stage 1–4：闭包内全部 SQL 文件（例程 + 表/视图 DDL）的
/// 解析、Gate 2A 拒绝、快照与引用检查。`skip_reject` 为 register
/// 已失败的文件——其顶层 Gate 2A 拒绝不重复收集，但文件仍参与
/// 快照与引用检查（例程体编译失败不吞掉同文件有效 DDL，后续
/// ALTER/DROP 才能看到先定义的 CREATE）。文件应用顺序 = 计划顺序
/// （例程在前、DDL 在后，逐计划去重保序）——SchemaSnapshot 的
/// ALTER/DROP 演化依赖该顺序；ALTER 目标缺失时 pipeline 内部
/// 延迟重试，不再依赖文件名排序猜测部署序。快照供 Gate 2B
/// stub 使用。读失败的文件跳过（例程文件的读错误已由 register
/// 报出；纯 DDL 文件读错误为边角，V1 不单独归因）。
fn gate_failures(
    project: &ProjectConfig,
    plans: &[RunPlan],
    skip_reject: &BTreeSet<String>,
) -> GateOutcome {
    let mut seen = BTreeSet::new();
    let mut gate_rels: Vec<&str> = Vec::new();
    for plan in plans {
        for rel in plan
            .ddl_routines
            .iter()
            .chain(plan.ddl_tables.iter())
            .map(String::as_str)
        {
            if seen.insert(rel) {
                gate_rels.push(rel);
            }
        }
    }
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
    let run = crate::gate::pipeline::run(&files, skip_reject);
    let mut out = Vec::new();
    if let Some((rel, err)) = run.schema_error {
        out.push((rel, err));
        return GateOutcome {
            findings: out,
            snapshot: None,
            rejected: run.rejected,
        };
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
    GateOutcome {
        findings: out,
        snapshot: run.snapshot,
        rejected: run.rejected,
    }
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
