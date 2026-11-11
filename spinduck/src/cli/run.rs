//! 子命令执行与渲染（纯函数式：写 `out` 并返回退出码，便于契约测试）。

use crate::cli::args::{parse, usage, Command};
use crate::cli::{compile_dir, summarize, FileReport};
use crate::error::EngineError;
use crate::rules::RuleRegistry;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 退出码：0 成功 / 1 业务或编译失败 / 2 用法错误。
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

/// 执行一次 CLI 调用。
pub fn run(argv: &[String], out: &mut dyn Write) -> i32 {
    let cmd = match parse(argv) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            let _ = write!(out, "{}", usage());
            return EXIT_USAGE;
        }
    };
    match exec(cmd, out) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            EXIT_FAIL
        }
    }
}

fn exec(cmd: Command, out: &mut dyn Write) -> Result<i32, EngineError> {
    match cmd {
        Command::Help => {
            write!(out, "{}", usage()).map_err(io_err)?;
            Ok(EXIT_OK)
        }
        Command::Version => {
            writeln!(out, "spinduck {}", crate::version()).map_err(io_err)?;
            Ok(EXIT_OK)
        }
        Command::RulesProbe { path } => {
            let mut rules = RuleRegistry::default();
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| EngineError::env(12, format!("read rules: {e}")))?;
            rules.overlay(&raw)?;
            let rules = rules;
            writeln!(
                out,
                "rules: {} target_duckdb={} probe_duckdb={} match={}",
                path.display(),
                rules.target_duckdb,
                rules.probe_duckdb,
                rules.probe_versions_match()
            )
            .map_err(io_err)?;
            Ok(if rules.probe_versions_match() {
                EXIT_OK
            } else {
                EXIT_FAIL
            })
        }
        Command::Compile { paths } => compile_cmd(&paths, out),
        Command::Explain { file, routine } => explain_cmd(&file, &routine, out),
        Command::Materialize { .. } => materialize_cmd(cmd_materialize(cmd), out),
        Command::Sync { .. } => sync_cmd(cmd_sync(cmd), out),
        Command::Call { .. } => call_cmd(cmd_call(cmd), out),
        Command::Verify { path } => crate::cli::verify::verify_cmd(path.as_deref(), out),
        Command::Plan {
            dir,
            routine,
            schema,
        } => crate::cli::plan::plan_cmd(dir.as_deref(), &routine, schema, out),
        Command::Deploy { plan, db } => crate::cli::deploy::deploy_cmd(&plan, &db, out),
        Command::Query {
            db,
            sql_file,
            out: out_path,
        } => crate::cli::query::query_cmd(&db, &sql_file, &out_path, out),
        Command::Compare { a, b, key } => crate::cli::compare::compare_cmd(&a, &b, &key, out),
    }
}

/// 兼容取包（避免在 match 中重复绑定）。
fn cmd_call(cmd: Command) -> CallParts {
    match cmd {
        Command::Call {
            db,
            rules,
            files,
            target,
            args,
            batch_commit,
            step_budget,
        } => CallParts {
            db,
            rules,
            files,
            target,
            args,
            batch_commit,
            step_budget,
        },
        _ => unreachable!("cmd_call only for Call"),
    }
}

struct MaterializeParts {
    db: PathBuf,
    tables: Vec<String>,
    where_clause: Option<String>,
    jobs: Option<usize>,
}

fn cmd_materialize(cmd: Command) -> MaterializeParts {
    match cmd {
        Command::Materialize {
            db,
            tables,
            where_clause,
            jobs,
        } => MaterializeParts {
            db,
            tables,
            where_clause,
            jobs,
        },
        _ => unreachable!("cmd_materialize only for Materialize"),
    }
}

struct SyncParts {
    db: PathBuf,
    table: String,
    cols: Vec<String>,
    where_clause: Option<String>,
    mode: Option<String>,
    slice: Option<String>,
    key: Option<String>,
}

fn cmd_sync(cmd: Command) -> SyncParts {
    match cmd {
        Command::Sync {
            db,
            table,
            cols,
            where_clause,
            mode,
            slice,
            key,
        } => SyncParts {
            db,
            table,
            cols,
            where_clause,
            mode,
            slice,
            key,
        },
        _ => unreachable!("cmd_sync only for Sync"),
    }
}

struct CallParts {
    db: Option<PathBuf>,
    rules: Option<PathBuf>,
    files: Vec<PathBuf>,
    target: crate::cli::args::CallTarget,
    args: Vec<String>,
    batch_commit: bool,
    step_budget: Option<u64>,
}

/// 对一组路径做项目规则发现：向上查找 `spinduck.toml` `[paths].rules` 并集（§6 只增不删）。
fn discover_rules_files(paths: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    let mut files = std::collections::BTreeSet::new();
    for p in paths {
        if let Some(proj) = crate::project::ProjectConfig::discover(p) {
            files.extend(proj.rules_files);
        }
    }
    files
}

/// 语料目录的规则口径：内置种子为基座 + 各路径向上发现的 `spinduck.toml`
/// `[paths].rules` 叠加（§6 只增不删）。compile/explain 与 golden 用例同一基座，
/// 避免"体检绿、运行红"的口径分裂。
fn rules_for_paths(paths: &[PathBuf]) -> Result<RuleRegistry, EngineError> {
    let files = discover_rules_files(paths);
    RuleRegistry::default().with_project_rules(&files.iter().cloned().collect::<Vec<_>>())
}

fn compile_cmd(paths: &[PathBuf], out: &mut dyn Write) -> Result<i32, EngineError> {
    let rules = rules_for_paths(paths)?;
    let mut all: Vec<FileReport> = Vec::new();
    for p in paths {
        if p.is_dir() {
            for r in compile_dir(p, &rules)? {
                all.push(FileReport {
                    file: format!("{}/{}", p.display(), r.file),
                    ..r
                });
            }
        } else {
            let one = compile_single(p, &rules)?;
            all.push(one);
        }
    }
    let s = summarize(&all);
    for r in &all {
        let mark = if r.non_utf8 { "  (non-utf8)" } else { "" };
        match &r.error {
            None => writeln!(out, "OK   {}  routines={}{}", r.file, r.routines, mark),
            Some(e) => writeln!(out, "FAIL {}  [{}] {}{}", r.file, e.code, e.message, mark),
        }
        .map_err(io_err)?;
    }
    writeln!(
        out,
        "summary: files={} ok={} failed={} routines={}",
        s.files, s.ok, s.failed, s.routines
    )
    .map_err(io_err)?;
    for (code, n) in failure_histogram(&all) {
        writeln!(out, "reason {} x{}", code, n).map_err(io_err)?;
    }
    Ok(if s.failed == 0 { EXIT_OK } else { EXIT_FAIL })
}

fn explain_cmd(file: &Path, routine: &str, out: &mut dyn Write) -> Result<i32, EngineError> {
    let text = crate::cli::read_source_strict(file)?;
    for line in crate::cli::explain::explain_routine(&text, routine)? {
        writeln!(out, "{line}").map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

fn compile_single(path: &Path, rules: &RuleRegistry) -> Result<FileReport, EngineError> {
    let (text, non_utf8) = crate::cli::read_source(path)?;
    let mut eng = crate::engine::Engine::new(Box::new(crate::cli::NoopExecutor), rules.clone());
    Ok(match eng.register(&text) {
        Ok(n) => FileReport {
            file: path.display().to_string(),
            routines: n,
            error: None,
            non_utf8,
        },
        Err(e) => FileReport {
            file: path.display().to_string(),
            routines: 0,
            error: Some(e),
            non_utf8,
        },
    })
}

/// 失败原因直方图（按错误码降序计数），用于优先级排序。
fn failure_histogram(reports: &[FileReport]) -> Vec<(String, usize)> {
    let mut m: BTreeMap<String, usize> = BTreeMap::new();
    for r in reports {
        if let Some(e) = &r.error {
            *m.entry(e.code.to_string()).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

#[cfg(feature = "native")]
fn call_cmd(parts: CallParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    use crate::cli::args::parse_literal;
    use crate::engine::Engine;
    let Some(t) = call_target_and_sources(&parts, out)? else {
        return Ok(EXIT_FAIL);
    };
    // 规则优先级链（F1，与 compile 同口径）：内置种子 < 项目发现 < --rules 手动覆盖。
    // 缺件门排在开库之前不动：先取目标与源清单，再建规则表。
    let rules_files: Vec<PathBuf> = t.rules_files.into_iter().collect();
    let mut rules = RuleRegistry::default().with_project_rules(&rules_files)?;
    let (target, sources) = (t.target, t.sources);
    if let Some(p) = &parts.rules {
        let raw = std::fs::read_to_string(p)
            .map_err(|e| EngineError::env(12, format!("read rules: {e}")))?;
        rules.overlay(&raw)?;
    }
    let ex = match &parts.db {
        Some(p) => crate::sql::DuckExecutor::open(&p.to_string_lossy())?,
        None => crate::sql::DuckExecutor::open_in_memory()?,
    };
    // 内核此刻的对象目录要在开库后、把执行器交给引擎之前取：缺件门的参照面就是它。
    let objects = crate::verify::kernel_objects(&ex)?;
    let mut eng = Engine::new(Box::new(ex), rules);
    eng.batch_commit = parts.batch_commit;
    // 复核应修 C：大业务（100k 级游标循环）默认预算不够，CLI 放大入口
    if let Some(b) = parts.step_budget {
        eng.step_budget = b;
    }
    for f in &sources {
        let text = crate::cli::read_source_strict(f)?;
        eng.register(&text)?;
    }
    if refuse_on_kernel_gaps(&eng, &target, &objects, out)? {
        return Ok(EXIT_FAIL);
    }
    let args: Vec<crate::value::SpinValue> = parts.args.iter().map(|a| parse_literal(a)).collect();
    let res = eng.call(&target, &args)?;
    if res.outs.is_empty() && res.return_value.is_none() {
        writeln!(out, "CALL {target} OK").map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    for (k, v) in &res.outs {
        writeln!(out, "OUT {} = {}", k, v.display()).map_err(io_err)?;
    }
    if let Some(v) = &res.return_value {
        writeln!(out, "RETURN {}", v.display()).map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

/// 执行面的缺件门（design D5）：目标闭包里内核没有的对象 ⇒ 列清单并拒绝。
///
/// BFS 与 `plan` 同一份实现（`src/plan/closure.rs::walk_chain`），只有参照面从"项目 DDL
/// 声明"换成"内核目录"。返回 `true` = 已打印缺件行，调用方直接退出 1。目标未登记时
/// `kernel_gaps` 给 `None`：那是 `BIZ-0150` 的故障域，本门不抢判。
#[cfg(feature = "native")]
fn refuse_on_kernel_gaps(
    eng: &crate::engine::Engine,
    target: &str,
    objects: &std::collections::BTreeSet<crate::verify::ObjectRef>,
    out: &mut dyn Write,
) -> Result<bool, EngineError> {
    let Some((resolved, gaps)) = crate::plan::kernel_gaps(eng, target, objects) else {
        return Ok(false);
    };
    if gaps.is_empty() {
        return Ok(false);
    }
    for m in &gaps {
        writeln!(out, "MISS {} <- {}", m.object, m.from).map_err(io_err)?;
    }
    writeln!(
        out,
        "call: target={resolved} missing={} 未执行（design D5 缺件即红）",
        gaps.len()
    )
    .map_err(io_err)?;
    Ok(true)
}

/// `call` 的目标例程、例程源清单与项目规则文件集（design D5/R3 / F1）。
///
/// 返回 `None` = 计划带缺件、已打印拒绝行，调用方直接退出 1。缺件检查排在开库之前：
/// DuckDB 的 `open` 即建文件，晚一步核对就留下半成品库（口径与 `deploy` 逐字相同——
/// 同一份计划，两个入口必须先拒在同一件事上）。`--file` 在两种形态下都有效：计划给的是
/// 闭包内的源，计划外临时补一个桩包仍要允许。
///
/// 规则文件集（第三返回值）：Routine 形态对各 `--file` 路径向上发现项目声明取并集；
/// Plan 形态直接取计划定位到的项目 `rules_files`（计划就在项目内，精确且不受计划文件
/// 本身软链等影响）。无项目声明时为空集 = 纯种子规则。
/// `call` 目标解析产物（F1）：目标例程 + 例程源清单 + 项目规则文件集。
#[cfg(feature = "native")]
struct CallTargeting {
    target: String,
    sources: Vec<PathBuf>,
    rules_files: std::collections::BTreeSet<PathBuf>,
}

#[cfg(feature = "native")]
fn call_target_and_sources(
    parts: &CallParts,
    out: &mut dyn Write,
) -> Result<Option<CallTargeting>, EngineError> {
    use crate::cli::args::CallTarget;
    match &parts.target {
        CallTarget::Routine(r) => Ok(Some(CallTargeting {
            target: r.clone(),
            sources: parts.files.clone(),
            rules_files: discover_rules_files(&parts.files),
        })),
        CallTarget::Plan(p) => {
            let text = crate::cli::read_source_strict(p)?;
            let plan = crate::plan::RunPlan::from_toml_str(&text)?;
            if !plan.missing.is_empty() {
                for m in &plan.missing {
                    writeln!(out, "MISS {} <- {}", m.object, m.from).map_err(io_err)?;
                }
                writeln!(
                    out,
                    "call: target={} missing={} 拒绝执行（补齐缺件后重跑 plan）",
                    plan.target,
                    plan.missing.len()
                )
                .map_err(io_err)?;
                return Ok(None);
            }
            let project = crate::cli::verify::locate(&plan.project_root(p))?;
            let deployment = crate::plan::deploy::resolve(&project, &plan)?;
            let mut files = deployment.routines;
            files.extend(parts.files.iter().cloned());
            Ok(Some(CallTargeting {
                target: plan.target.clone(),
                sources: files,
                rules_files: project.rules_files.iter().cloned().collect(),
            }))
        }
    }
}

/// GaussDB 连接参数来自环境变量（避免命令行泄露口令，§10.4）。
#[cfg(feature = "gauss")]
fn open_gauss() -> Result<crate::io::GaussConn, EngineError> {
    let host = std::env::var("SPINDUCK_OG_HOST")
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_HOST is required".to_string()))?;
    let port: u16 = std::env::var("SPINDUCK_OG_PORT")
        .unwrap_or_else(|_| "5432".to_string())
        .parse()
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_PORT must be a port number".to_string()))?;
    let user = std::env::var("SPINDUCK_OG_USER").unwrap_or_else(|_| "gaussdb".to_string());
    let password = std::env::var("SPINDUCK_OG_PASSWORD")
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_PASSWORD is required".to_string()))?;
    let dbname = std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".to_string());
    crate::io::GaussConn::connect_parts(&host, port, &user, &password, &dbname)
}

#[cfg(feature = "gauss")]
fn materialize_cmd(parts: MaterializeParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    // 临时 CSV 与目标库同目录（失败诊断现场自包含；成功即删，设计文档 §3.1）
    let workdir = parts
        .db
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    if let [table] = parts.tables.as_slice() {
        // 单表形态：与 V26 语义逐字一致（允许 --where）
        let mut og = open_gauss()?;
        let outcome = og.materialize(&duck, table, parts.where_clause.as_deref(), &workdir)?;
        writeln!(out, "MATERIALIZE {} rows={}", table, outcome.rows).map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    // 多表形态（V27 §3.6）：--jobs 个工作线程，每线程独立 GaussDB 连接 +
    // 独立 DuckDB 连接句柄（同进程同库多连接，MVCC 处理并发写不同表）
    let jobs = parts.jobs.unwrap_or(4).min(parts.tables.len());
    let executors = (0..parts.tables.len())
        .map(|_| {
            let cloned = duck
                .connection()
                .try_clone()
                .map_err(|e| EngineError::env(20, format!("duckdb 连接克隆失败：{e}")))?;
            Ok::<_, EngineError>(crate::sql::DuckExecutor::from_connection(cloned))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let outcomes = crate::io::materialize_parallel(
        jobs,
        parts.tables.clone(),
        &workdir,
        open_gauss,
        executors,
    );
    let mut failed = 0;
    for (table, result) in &outcomes {
        match result {
            Ok(o) => {
                writeln!(out, "MATERIALIZE {} rows={}", table, o.rows).map_err(io_err)?;
            }
            Err(e) => {
                failed += 1;
                // 单表失败不中断其余表（快照批次语义），任一失败整体退出码 1
                writeln!(out, "FAIL {} {}", table, e).map_err(io_err)?;
            }
        }
    }
    if failed > 0 {
        return Ok(EXIT_FAIL);
    }
    Ok(EXIT_OK)
}

#[cfg(not(feature = "gauss"))]
fn materialize_cmd(parts: MaterializeParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "materialize {} -> {} (where: {:?}) requires feature `gauss`",
            parts.tables.join(","),
            parts.db.display(),
            parts.where_clause
        ),
    ))
}

#[cfg(feature = "gauss")]
fn sync_cmd(parts: SyncParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    let mode = crate::io::sync::resolve_mode(
        parts.mode.as_deref(),
        parts.slice.as_deref(),
        parts.key.as_deref(),
        parts.where_clause.as_deref(),
    )?;
    // 临时 CSV 与目标库同目录（失败诊断现场自包含；成功即删，设计 §4.1）
    let workdir = parts
        .db
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let mut og = open_gauss()?;
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    let oc = og.sync_write(&duck, &parts.table, &parts.cols, &mode, &workdir)?;
    // 缺省形态（无 mode/where）保持 V26 契约逐字不变
    if parts.mode.is_none() && parts.where_clause.is_none() {
        writeln!(out, "SYNC {} rows={}", parts.table, oc.rows).map_err(io_err)?;
    } else {
        writeln!(
            out,
            "SYNC {} mode={} rows={}",
            parts.table, oc.mode_label, oc.rows
        )
        .map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

#[cfg(not(feature = "gauss"))]
fn sync_cmd(parts: SyncParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "sync {} ({}) from {} requires feature `gauss`",
            parts.table,
            parts.cols.join(","),
            parts.db.display()
        ),
    ))
}

#[cfg(not(feature = "native"))]
fn call_cmd(parts: CallParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "call {:?} (args: {}, files: {:?}, db: {:?}, rules: {:?}) requires feature `native`",
            parts.target,
            parts.args.join(","),
            parts.files,
            parts.db,
            parts.rules
        ),
    ))
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn run_str(items: &[&str]) -> (i32, String) {
        let mut buf: Vec<u8> = Vec::new();
        let code = run(&argv(items), &mut buf);
        (code, String::from_utf8(buf).unwrap())
    }

    #[test]
    fn should_print_usage_and_exit_2_on_usage_error() {
        let (code, out) = run_str(&["compile"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(out.contains("ENG-0010"));
        assert!(out.contains("compile <path>"));
    }

    #[test]
    fn should_report_compile_summary_and_reason_histogram() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("ok.sql"),
            "CREATE OR REPLACE PROCEDURE p_ok IS BEGIN NULL; END;",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("bad.sql"),
            "CREATE OR REPLACE PROCEDURE p_bad IS BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;",
        )
        .unwrap();
        let (code, out) = run_str(&["compile", &tmp.path().to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(
            out.contains("summary: files=2 ok=1 failed=1 routines=1"),
            "{out}"
        );
        assert!(out.contains("reason ENG-0022 x1"), "{out}");
    }

    #[test]
    fn should_overlay_project_rules_when_compiling() {
        // 语料目录所在项目（spinduck.toml `[paths].rules`）的规则必须参与编译，
        // 否则 compile 体检口径与 call/golden 不一致（评审第三轮 F-编译规则）
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("spinduck.toml"),
            "[project]\nname = \"demo\"\n[paths]\nrules = [\"rules/custom.toml\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("rules")).unwrap();
        std::fs::write(
            root.join("rules/custom.toml"),
            "[functions]\nMYNVL = \"coalesce\"\n",
        )
        .unwrap();
        let corpus = root.join("sql/routines");
        std::fs::create_dir_all(&corpus).unwrap();

        let r = rules_for_paths(std::slice::from_ref(&corpus)).expect("项目规则叠加");
        assert_eq!(r.functions["MYNVL"], "coalesce");
        assert_eq!(r.functions["NVL"], "coalesce", "内置种子仍为基座");
        // 项目声明对其下所有语料目录生效（子目录继承，无需重复声明）
        let nested = root.join("sql/routines/pkg");
        std::fs::create_dir_all(&nested).unwrap();
        let r2 = rules_for_paths(&[nested]).expect("子目录继承项目规则");
        assert_eq!(r2.functions["MYNVL"], "coalesce");
        // 无项目声明的目录回落内置种子（compile 与 call 同口径的基座）
        let outside = tempfile::tempdir().unwrap();
        let plain = rules_for_paths(&[outside.path().to_path_buf()]).expect("无项目声明也可编译");
        assert_eq!(plain.functions.get("MYNVL"), None);
    }

    #[test]
    fn should_pass_rules_probe_when_versions_match() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rules.toml");
        std::fs::write(&p, "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.5.5\"\n").unwrap();
        let (code, out) = run_str(&["rules", "probe", &p.to_string_lossy()]);
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("match=true"), "{out}");
    }

    #[test]
    fn should_fail_rules_probe_on_version_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rules.toml");
        std::fs::write(&p, "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.4.4\"\n").unwrap();
        let (code, out) = run_str(&["rules", "probe", &p.to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(out.contains("match=false"), "{out}");
    }

    #[test]
    fn should_explain_routine_as_stable_ir_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("p.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE PROCEDURE prc_x(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 2; END;",
        )
        .unwrap();
        let (code, out) = run_str(&["explain", "--file", &f.to_string_lossy(), "prc_x"]);
        assert_eq!(code, EXIT_OK, "{out}");
        // 参数由调用方预绑定，IR 只含函数体：唯一一条赋值
        assert_eq!(out, "0\tassign\tp_o := (p_i * 2)\n", "{out}");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_register_file_and_call_routine_with_out_param() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("pkg.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE PROCEDURE prc_calc(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 3; END;",
        )
        .unwrap();
        let (code, out) = run_str(&[
            "call",
            "--file",
            &f.to_string_lossy(),
            "prc_calc",
            "14",
            "null",
        ]);
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("OUT p_o = 42"), "{out}");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_load_project_rules_for_call_routine_form() {
        // F1（验证报告/修复设计 docs/spinduck-cli-修复设计.md）：call 必须与 compile 同口径
        // 发现项目规则——不传 --rules 也不得"体检绿、运行红"（修复前此测试必红）
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("spinduck.toml"),
            "[project]\nname = \"demo\"\n[paths]\nrules = [\"rules/custom.toml\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("rules")).unwrap();
        std::fs::write(
            root.join("rules/custom.toml"),
            "[functions]\nMYNVL = \"coalesce\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("sql")).unwrap();
        let f = root.join("sql/pkg.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE FUNCTION fnc_nv(x IN NUMBER, y IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN MYNVL(x, y); END;",
        )
        .unwrap();
        let (code, out) = run_str(&[
            "call",
            "--file",
            &f.to_string_lossy(),
            "fnc_nv",
            "5",
            "null",
        ]);
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("RETURN 5"), "{out}");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_load_project_rules_for_call_plan_form() {
        // F1：--plan 形态同样发现项目规则（规则文件取计划定位到的 project.rules_files）
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("spinduck.toml"),
            "[project]\nname = \"demo\"\n[paths]\nrules = [\"rules/custom.toml\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("rules")).unwrap();
        std::fs::write(
            root.join("rules/custom.toml"),
            "[functions]\nMYNVL = \"coalesce\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("sql/routines")).unwrap();
        std::fs::write(
            root.join("sql/routines/r.sql"),
            "CREATE OR REPLACE FUNCTION fnc_nv(x IN NUMBER, y IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN MYNVL(x, y); END;",
        )
        .unwrap();
        // plan 会把计划落到项目 .spinduck/plan 下，call --plan 直接消费它
        let (plan_code, plan_out) = run_str(&["plan", "--dir", &root.to_string_lossy(), "fnc_nv"]);
        assert_eq!(plan_code, EXIT_OK, "{plan_out}");
        let plan_path = root.join(".spinduck/plan/fnc_nv.toml");
        assert!(plan_path.is_file(), "plan 落盘缺失");
        let (code, out) = run_str(&["call", "--plan", &plan_path.to_string_lossy(), "5", "null"]);
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("RETURN 5"), "{out}");
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_apply_rules_flag_after_project_rules_for_call() {
        // D1 优先级锁定：项目规则与 --rules 对同一函数名都给映射时，--rules 生效
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("spinduck.toml"),
            "[project]\nname = \"demo\"\n[paths]\nrules = [\"rules/custom.toml\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("rules")).unwrap();
        std::fs::write(
            root.join("rules/custom.toml"),
            "[functions]\nMYX = \"least\"\n",
        )
        .unwrap();
        let override_rules = tmp.path().join("override.toml");
        std::fs::write(&override_rules, "[functions]\nMYX = \"greatest\"\n").unwrap();
        let f = root.join("pkg.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE FUNCTION fnc_x(a IN NUMBER, b IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN MYX(a, b); END;",
        )
        .unwrap();
        let (code, out) = run_str(&[
            "call",
            "--file",
            &f.to_string_lossy(),
            "--rules",
            &override_rules.to_string_lossy(),
            "fnc_x",
            "2",
            "7",
        ]);
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(
            out.contains("RETURN 7"),
            "greatest 生效即 --rules 为链顶: {out}"
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_call_with_seed_rules_when_no_project_declared() {
        // F1 守护：项目外 --file 保持纯种子规则，发现逻辑不得扩大化
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("pkg.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE PROCEDURE prc_calc(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 3; END;",
        )
        .unwrap();
        let (code, out) = run_str(&[
            "call",
            "--file",
            &f.to_string_lossy(),
            "prc_calc",
            "14",
            "null",
        ]);
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("OUT p_o = 42"), "{out}");
    }

    #[test]
    fn should_fail_verify_when_project_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (code, out) = run_str(&["verify", &tmp.path().to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(
            out.contains("spinduck.toml"),
            "诊断需指向缺失的项目声明文件：{out}"
        );
    }
}
