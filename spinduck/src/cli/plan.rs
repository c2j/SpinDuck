//! `plan` 子命令壳（design §6.1 / §7 A2）：定位项目 → 求闭包 → 落盘 → 渲染与退出码。
//!
//! 闭包与渲染口径全在 `crate::plan`；本文件只做壳（§1）。缺件即判红：
//! 计划里写了 `[[missing]]` 就代表 `deploy` 必然建不出可执行环境，不留"生成了但跑不通"的中间态。

use crate::cli::run::{EXIT_FAIL, EXIT_OK};
use crate::cli::verify::locate;
use crate::error::EngineError;
use crate::plan::codeweb::CodewebExport;
use crate::plan::{self, RunPlan};
use crate::project::ProjectConfig;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 落盘结果（路径 + 是否改写），标注口径同运行态快照。
pub(crate) struct PlanWrite {
    pub(crate) path: PathBuf,
    pub(crate) changed: bool,
}

/// `spinduck plan [--dir <项目目录>] [--schema <名>] <routine>`。
pub fn plan_cmd(
    dir: Option<&Path>,
    routine: &str,
    schema: Option<String>,
    export: &impl CodewebExport,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let start = match dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| EngineError::env(12, format!("cwd: {e}")))?,
    };
    let project = locate(&start)?;
    let plan = plan::build(&project, routine, schema, export)?;
    // 落盘名取自目录命中后的例程全名：解析器给出的名字不含路径分隔符，无需再清洗
    let written = write_plan(&project, &plan)?;
    render(&plan, &written, out)
}

pub(crate) fn write_plan(
    project: &ProjectConfig,
    plan: &RunPlan,
) -> Result<PlanWrite, EngineError> {
    let path = plan::plan_dir(project).join(format!("{}.toml", plan.target));
    let text = plan.to_toml_string();
    let dir = path
        .parent()
        .ok_or_else(|| EngineError::env(30, format!("非法计划路径 {}", path.display())))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| EngineError::env(30, format!("创建 {}: {e}", dir.display())))?;
    let previous = std::fs::read_to_string(&path).unwrap_or_default();
    let changed = previous != text;
    if changed {
        std::fs::write(&path, &text)
            .map_err(|e| EngineError::env(30, format!("写 {}: {e}", path.display())))?;
    }
    Ok(PlanWrite { path, changed })
}

fn render(plan: &RunPlan, written: &PlanWrite, out: &mut dyn Write) -> Result<i32, EngineError> {
    writeln!(
        out,
        "plan: {} ({})",
        written.path.display(),
        if written.changed {
            "updated"
        } else {
            "unchanged"
        }
    )
    .map_err(io_err)?;
    writeln!(
        out,
        "plan: target={} routines={} tables={} missing={}",
        plan.target,
        plan.ddl_routines.len(),
        plan.ddl_tables.len(),
        plan.missing.len()
    )
    .map_err(io_err)?;
    for m in &plan.missing {
        writeln!(out, "MISS {} <- {}", m.object, m.from).map_err(io_err)?;
    }
    Ok(if plan.missing.is_empty() {
        EXIT_OK
    } else {
        EXIT_FAIL
    })
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}
