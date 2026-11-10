//! 计划的消费面（design §6.2 / §7 A2-3）：把计划里的清单装进一个内核。
//!
//! 本模块只做两件事：把计划中的相对路径解析成**已核对存在**的文件清单，以及按
//! `verify::deploy` 的同一条下发口径逐文件部署。建表/建视图/注册的实现**一律不在这里**
//! ——重写一份就等于第二套真相，`verify` 与 `deploy` 迟早漂移。

use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::RunPlan;
use crate::project::ProjectConfig;
use crate::verify::{
    deploy_ddl_file, deploy_view_file, ensure_engine_provided_tables, register_routine_files,
};
use std::path::{Path, PathBuf};

/// 已解析、已核对存在的部署清单（`deploy` 先拿这份，再开内核连接）。
///
/// 分两步是刻意的：打开 DuckDB 文件即创建它，清单核对放在后面就会留下半成品库——
/// 而"建了一半的库"比"没建"更危险（下一个使用者看不出来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// 表/序列 DDL（`<ddl>/views/` 之外的清单条目）。
    pub ddl: Vec<PathBuf>,
    /// 视图文件：走"视图体优先、内核不收则退占位"的 `deploy_view` 口径。
    pub views: Vec<PathBuf>,
    /// 例程源文件。
    pub routines: Vec<PathBuf>,
}

/// 计划 → 部署清单：路径按项目根解析，任一文件缺失即 ENV-0030 拒绝。
///
/// 视图与表的分界沿用目录发现时的同一判据（`[paths].ddl` 下的 `views/`），
/// 不另立规则——`plan` 生成的条目本就来自这三个目录。
pub fn resolve(project: &ProjectConfig, plan: &RunPlan) -> Result<Deployment, EngineError> {
    let views_dir = project.ddl_dir.join("views");
    let mut ddl = Vec::new();
    let mut views = Vec::new();
    for rel in &plan.ddl_tables {
        let path = existing(&project.root, rel)?;
        if path.starts_with(&views_dir) {
            views.push(path);
        } else {
            ddl.push(path);
        }
    }
    let mut routines = Vec::new();
    for rel in &plan.ddl_routines {
        routines.push(existing(&project.root, rel)?);
    }
    Ok(Deployment {
        ddl,
        views,
        routines,
    })
}

/// 逐文件下发：表/序列 → 视图 → 引擎自建对象 → 例程（顺序同 `deploy_project`）。
pub fn apply(eng: &mut Engine, deployment: &Deployment) -> Result<(), EngineError> {
    let funcs = eng.rules.functions.clone();
    for file in &deployment.ddl {
        deploy_ddl_file(eng, file, &funcs)?;
    }
    for file in &deployment.views {
        deploy_view_file(eng, file, &funcs)?;
    }
    ensure_engine_provided_tables(eng)?;
    register_routine_files(eng, &deployment.routines)
}

/// 相对项目根解析并要求存在；缺失的诊断同时给出计划里的相对路径与解析后的绝对路径。
fn existing(root: &Path, rel: &str) -> Result<PathBuf, EngineError> {
    let path = root.join(rel);
    if path.is_file() {
        return Ok(path);
    }
    Err(EngineError::env(
        30,
        format!("计划引用的文件不存在: {rel}（解析为 {}）", path.display()),
    ))
}
