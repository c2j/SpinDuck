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
use std::path::{Path, PathBuf};

/// deploy 的开库前准备产物（batch 共享连接复用同一份准备结果）。
pub struct DeployPrep {
    target: String,
    deployment: plan::deploy::Deployment,
    rules: RuleRegistry,
    ddl_files: usize,
    routine_files: usize,
    db: PathBuf,
}

impl DeployPrep {
    /// batch 引擎缓存键用：例程源文件清单（内容哈希在调用方算）。
    pub(crate) fn routine_sources(&self) -> &[PathBuf] {
        &self.deployment.routines
    }
    /// batch 引擎缓存键用：生效规则（项目 rules_files 已叠加）。
    pub(crate) fn rules(&self) -> &RuleRegistry {
        &self.rules
    }
}

/// `spinduck deploy <计划文件.toml> --db <本地 DuckDB 文件>`。
pub fn deploy_cmd(plan_path: &Path, db: &Path, out: &mut dyn Write) -> Result<i32, EngineError> {
    let Some(prep) = deploy_prepare(plan_path, db, out)? else {
        return Ok(EXIT_FAIL);
    };
    #[cfg(feature = "native")]
    {
        let ex = crate::sql::DuckExecutor::open(&prep.db.to_string_lossy())?;
        let mut eng = Engine::new(Box::new(ex), prep.rules.clone());
        deploy_apply(&prep, &mut eng, out)
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = prep;
        Err(EngineError::env(
            12,
            format!(
                "deploy 到 {} 需要 feature `native`（部署要把资产装进内核）",
                db.display()
            ),
        ))
    }
}

/// 读计划 → 缺件门 → 物化清单核对 → 规则装配。全部先于开库（`open` 即建文件）。
/// 返回 `None` = 已打印 MISS 行并判拒（调用方退出 1）。
pub fn deploy_prepare(
    plan_path: &Path,
    db: &Path,
    out: &mut dyn Write,
) -> Result<Option<DeployPrep>, EngineError> {
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
        return Ok(None);
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
        return Ok(None);
    }

    let rules = RuleRegistry::default().with_project_rules(&project.rules_files)?;
    Ok(Some(DeployPrep {
        target: plan.target,
        deployment,
        rules,
        ddl_files,
        routine_files,
        db: db.to_path_buf(),
    }))
}

/// 在既有引擎上执行部署清单并渲染结果行（单步 CLI 与 batch 共用此尾段）。
pub fn deploy_apply(
    prep: &DeployPrep,
    eng: &mut Engine,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    plan::deploy::apply(eng, &prep.deployment)?;
    writeln!(
        out,
        "deploy: target={} ddl_files={} routine_files={} db={}",
        prep.target,
        prep.ddl_files,
        prep.routine_files,
        prep.db.display()
    )
    .map_err(io_err)?;
    Ok(EXIT_OK)
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}
