//! `deploy` 子命令壳（design §6.2 / §7 A2-3）：读计划 → 拒缺件 → 把清单装进落盘内核。
//!
//! 清单解析与逐文件下发全在 `crate::plan::deploy`；本文件只做壳（§1），
//! 退出码与渲染归这里。缺件表非空时**连内核都不开**：建了一半的库会被下一个
//! 使用者当成干净环境，比不建更危险。

use crate::cli::run::{EXIT_FAIL, EXIT_OK};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::{self, RunPlan};
use crate::rules::RuleRegistry;
use std::io::Write;
use std::path::Path;

/// `spinduck deploy <计划文件.toml> --db <本地 DuckDB 文件>`。
pub fn deploy_cmd(plan_path: &Path, db: &Path, out: &mut dyn Write) -> Result<i32, EngineError> {
    let text = crate::cli::read_source_strict(plan_path)?;
    let plan = RunPlan::from_toml_str(&text)?;
    if !plan.missing.is_empty() {
        for m in &plan.missing {
            writeln!(out, "MISS {} <- {}", m.object, m.from).map_err(io_err)?;
        }
        writeln!(
            out,
            "deploy: target={} missing={} 拒绝部署（补齐缺件后重跑 plan）",
            plan.target,
            plan.missing.len()
        )
        .map_err(io_err)?;
        return Ok(EXIT_FAIL);
    }

    let project = crate::project::ProjectConfig::locate(&plan.project_root(plan_path))?;
    // 清单核对先于开库：DuckDB 的 open 即建文件，晚一步就留下半成品
    let deployment = plan::deploy::resolve(&project, &plan)?;
    let ddl_files = deployment.ddl.len() + deployment.views.len();
    let routine_files = deployment.routines.len();

    // V27 物化清单守卫（评审 2026-11-14 F2）：`[sources].materialize` 的每张表
    // 必须被本项目 DDL 建出。核对读 DDL 名字集合，**先于 open_engine**——失败时
    // 不落半成品库；名称归一与比较同一口径，不把外部输入拼进 SQL。
    let covered = plan::deploy::deployed_table_names(&deployment)?;
    let mut missing_materialize = Vec::new();
    for t in &plan.materialize {
        if !covered.contains(&plan::deploy::materialize_key(t)) {
            missing_materialize.push(t);
        }
    }
    if !missing_materialize.is_empty() {
        for t in &missing_materialize {
            writeln!(
                out,
                "MISS relation:{t} <- materialize 清单（项目 DDL 未覆盖：请补 sql/ddl 或从清单移除）"
            )
            .map_err(io_err)?;
        }
        writeln!(
            out,
            "deploy: target={} materialize 清单缺表 {} 张，拒绝部署",
            plan.target,
            missing_materialize.len()
        )
        .map_err(io_err)?;
        return Ok(EXIT_FAIL);
    }

    let rules = RuleRegistry::default().with_project_rules(&project.rules_files)?;
    let mut eng = open_engine(db, rules)?;
    plan::deploy::apply(&mut eng, &deployment)?;

    writeln!(
        out,
        "deploy: target={} ddl_files={} routine_files={} db={}",
        plan.target,
        ddl_files,
        routine_files,
        db.display()
    )
    .map_err(io_err)?;
    Ok(EXIT_OK)
}

#[cfg(feature = "native")]
fn open_engine(db: &Path, rules: RuleRegistry) -> Result<Engine, EngineError> {
    let ex = crate::sql::DuckExecutor::open(&db.to_string_lossy())?;
    Ok(Engine::new(Box::new(ex), rules))
}

#[cfg(not(feature = "native"))]
fn open_engine(db: &Path, _rules: RuleRegistry) -> Result<Engine, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "deploy 到 {} 需要 feature `native`（部署要把资产装进内核）",
            db.display()
        ),
    ))
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}
