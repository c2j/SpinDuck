//! `call` 执行面：目标解析 → 规则优先级链 → 缺件门 → 审计日志 → 渲染。
//!
//! 这里的门禁顺序是契约的一部分（缺件即红必须排在开库与建日志之前），改动前先看
//! `refuse_on_kernel_gaps` 与 `call_target_and_sources` 的文档。

// 执行面（native）独有的依赖：`not(native)` 时只剩拒绝桩，故逐条 gate，不留未用导入。
#[cfg(feature = "native")]
use super::compile::discover_rules_files;
#[cfg(feature = "native")]
use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::args::Command;
use crate::error::EngineError;
#[cfg(feature = "native")]
use crate::rules::RuleRegistry;
use std::io::Write;
use std::path::PathBuf;

/// 兼容取包（避免在 match 中重复绑定）。
pub(super) fn cmd_call(cmd: Command) -> CallParts {
    match cmd {
        Command::Call {
            db,
            rules,
            files,
            target,
            args,
            batch_commit,
            step_budget,
            trace,
            trace_format,
            trace_level,
        } => CallParts {
            db,
            rules,
            files,
            target,
            args,
            batch_commit,
            step_budget,
            trace,
            trace_format,
            trace_level,
        },
        _ => unreachable!("cmd_call only for Call"),
    }
}

pub(super) struct CallParts {
    db: Option<PathBuf>,
    rules: Option<PathBuf>,
    files: Vec<PathBuf>,
    target: crate::cli::args::CallTarget,
    args: Vec<String>,
    batch_commit: bool,
    step_budget: Option<u64>,
    trace: Option<PathBuf>,
    trace_format: crate::trace::TraceFormat,
    trace_level: crate::trace::TraceLevel,
}

#[cfg(feature = "native")]
pub(super) fn call_cmd(parts: CallParts, out: &mut dyn Write) -> Result<i32, EngineError> {
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
    if refuse_on_kernel_gaps(&eng, &target, &objects, t.codeweb_refs.as_deref(), out)? {
        return Ok(EXIT_FAIL);
    }
    let args: Vec<crate::value::SpinValue> = parts.args.iter().map(|a| parse_literal(a)).collect();
    // 审计日志的门禁顺序：缺件已拒 = 一条都没执行，此时不得留下日志文件，否则
    // "有日志"会变成"跑过"的假象（§11.1）。stdout 契约与 --trace 无关，恒不变。
    let mut traced = match &parts.trace {
        Some(p) => Some(crate::cli::trace::TraceRun::open(
            p,
            parts.trace_format,
            parts.trace_level,
            &target,
        )?),
        None => None,
    };
    let res = match &mut traced {
        Some(t) => eng.call_with_sink(&target, &args, t.sink())?,
        None => eng.call(&target, &args)?,
    };
    if res.outs.is_empty() && res.return_value.is_none() {
        writeln!(out, "CALL {target} OK").map_err(io_err)?;
    } else {
        for (k, v) in &res.outs {
            writeln!(out, "OUT {} = {}", k, v.display()).map_err(io_err)?;
        }
        if let Some(v) = &res.return_value {
            writeln!(out, "RETURN {}", v.display()).map_err(io_err)?;
        }
    }
    if let Some(t) = traced {
        t.finish()?;
    }
    Ok(EXIT_OK)
}

/// 执行面的缺件门（design D5）：目标闭包里内核没有的对象 ⇒ 列清单并拒绝。
///
/// 裸 `call` 与旧计划仍走词法 `walk_chain`。`--plan` 且 `generator` 以 `codeweb ` 开头时
/// 只核 `[[ref]]` 里的表/视图/序列，不走词法 BFS。返回 `true` = 已打印缺件行。
/// 目标未登记时两个入口都给 `None`：那是 `BIZ-0150`，本门不抢判。
#[cfg(feature = "native")]
fn refuse_on_kernel_gaps(
    eng: &crate::engine::Engine,
    target: &str,
    objects: &std::collections::BTreeSet<crate::verify::ObjectRef>,
    codeweb_refs: Option<&[crate::plan::PlanRef]>,
    out: &mut dyn Write,
) -> Result<bool, EngineError> {
    let found = match codeweb_refs {
        Some(refs) => crate::plan::kernel_gaps_from_refs(eng, target, objects, refs),
        None => crate::plan::kernel_gaps(eng, target, objects),
    };
    let Some((resolved, gaps)) = found else {
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
    /// `Some` 仅当 `--plan` 且 generator 以 `codeweb ` 开头。
    codeweb_refs: Option<Vec<crate::plan::PlanRef>>,
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
            codeweb_refs: None,
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
            let project = crate::project::ProjectConfig::locate(&plan.project_root(p))?;
            let deployment = crate::plan::deploy::resolve(&project, &plan)?;
            let mut files = deployment.routines;
            files.extend(parts.files.iter().cloned());
            let codeweb_refs = plan
                .generator
                .starts_with("codeweb ")
                .then(|| plan.refs.clone());
            Ok(Some(CallTargeting {
                target: plan.target.clone(),
                sources: files,
                rules_files: project.rules_files.iter().cloned().collect(),
                codeweb_refs,
            }))
        }
    }
}

#[cfg(not(feature = "native"))]
pub(super) fn call_cmd(parts: CallParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
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

#[cfg(test)]
mod tests {
    use crate::cli::run::{run_str, EXIT_OK};

    #[cfg(feature = "native")]
    use crate::error::EngineError;
    #[cfg(feature = "native")]
    use crate::plan::codeweb::CodewebExport;
    #[cfg(feature = "native")]
    use std::io::Read;
    #[cfg(feature = "native")]
    use std::path::Path;

    #[cfg(feature = "native")]
    struct JsonExport(Vec<u8>);

    #[cfg(feature = "native")]
    impl CodewebExport for JsonExport {
        fn export_json(&self, _project: &Path) -> Result<Box<dyn Read + Send>, EngineError> {
            Ok(Box::new(std::io::Cursor::new(self.0.clone())))
        }
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
        // 计划由项目态 compile 写出（generator 为 codeweb）。规则仍从项目声明带进 call。
        std::fs::write(root.join("codeweb.toml"), b"").unwrap();
        let file = root.join("sql/routines/r.sql");
        let file_s = file.to_string_lossy().replace('\\', "/");
        let export = JsonExport(
            serde_json::to_vec(&serde_json::json!({
                "nodes": [{"id": 1, "type": "function", "name": "fnc_nv", "file": file_s}],
                "edges": []
            }))
            .unwrap(),
        );
        let mut plan_buf = Vec::new();
        let opts = crate::cli::ProjectCompile {
            dir: Some(root.to_path_buf()),
            schema: None,
            entries: vec!["fnc_nv".to_string()],
            codeweb_bin: None,
            codeweb_project: None,
            sync_manifest: false,
        };
        let plan_code = crate::cli::compile_project_with_export(opts, &export, &mut plan_buf)
            .unwrap_or_else(|e| panic!("{}", e.message));
        let plan_out = String::from_utf8(plan_buf).unwrap();
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
}
