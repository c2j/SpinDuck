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
    sql_units, tokens, Tok, ENGINE_PROVIDED_TABLES,
};
use std::collections::BTreeSet;
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

/// 本次部署会建出的表名（无限定、ASCII 小写），供 `deploy` 在**开库前**核验
/// `[sources].materialize` 是否被项目 DDL 覆盖。
///
/// 只读 `deployment.ddl` 里的 `CREATE … TABLE`：视图与序列不覆盖同名表；引擎自建表
/// （[`ENGINE_PROVIDED_TABLES`]）算覆盖，因为部署同样会建出它们。解析走
/// [`sql_units`] + 词法器，注释与字符串字面量不参与——不执行 DDL，也不创建库文件。
pub(crate) fn deployed_table_names(
    deployment: &Deployment,
) -> Result<BTreeSet<String>, EngineError> {
    let mut names = BTreeSet::new();
    for file in &deployment.ddl {
        let text = crate::source::read_text(file)?;
        for unit in sql_units(&text) {
            if let Some(name) = create_table_name(&unit) {
                names.insert(name);
            }
        }
    }
    names.extend(
        ENGINE_PROVIDED_TABLES
            .iter()
            .map(|t| t.to_ascii_lowercase()),
    );
    Ok(names)
}

/// 物化清单项的比对键：与 ENV-0015 同口径的 ASCII 小写原名。
///
/// 这里**不做** schema/引号归一：ENV-0015 用 `lower($t) = lower(table_name)`
/// 比对，前置守卫若更宽松，就会放行一个 materialize 自己会拒绝的名字，把缺表
/// 重新推回错误的层。`bigfund.t_a` 与 `"T_A"` 都不覆盖 `t_a`（与 ENV-0015 一致）。
pub(crate) fn materialize_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// 从一条 DDL 语句提取 `CREATE [OR REPLACE] [GLOBAL] … TABLE` 的表名。
///
/// 只认 `create` 之后紧邻的修饰词与 `table` 关键字；`CREATE VIEW` / `CREATE
/// SEQUENCE` / `CREATE INDEX` 都不匹配。名称允许带引号与 `schema.` 前缀。
fn create_table_name(unit: &str) -> Option<String> {
    let toks = tokens(&unit.to_ascii_lowercase());
    let mut i = 0usize;
    while i < toks.len() {
        if !is_word(toks.get(i), "create") {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < toks.len() {
            match &toks[j] {
                Tok::Word(w)
                    if matches!(
                        w.as_str(),
                        "or" | "replace"
                            | "global"
                            | "local"
                            | "temporary"
                            | "temp"
                            | "unlogged"
                            | "transient"
                    ) =>
                {
                    j += 1;
                }
                Tok::Word(w) if w.as_str() == "table" => break,
                _ => break,
            }
        }
        if !is_word(toks.get(j), "table") {
            i += 1;
            continue;
        }
        j += 1;
        if is_word(toks.get(j), "if")
            && is_word(toks.get(j + 1), "not")
            && is_word(toks.get(j + 2), "exists")
        {
            j += 3;
        }
        let mut name = String::new();
        while j < toks.len() {
            match &toks[j] {
                Tok::Word(w) if matches!(w.as_str(), "as" | "using" | "if") => break,
                Tok::Word(w) => name.push_str(w),
                Tok::Punct('.') => name.push('.'),
                // 引号只界定标识符，不进入名称（`"T_A"` → `t_a`）
                Tok::Punct('"') => {}
                _ => break,
            }
            j += 1;
        }
        if !name.is_empty() {
            // `schema.t` 在 DuckDB 登记为 `table_name = t`（ENV-0015 比的是尾段）
            return Some(bare_table_name(&name));
        }
        i += 1;
    }
    None
}

fn is_word(tok: Option<&Tok>, expected: &str) -> bool {
    matches!(tok, Some(Tok::Word(w)) if w == expected)
}

/// DDL 名取 DuckDB `duckdb_tables.table_name` 的形态：无限定尾段、ASCII 小写。
fn bare_table_name(name: &str) -> String {
    name.rsplit('.')
        .find(|part| !part.is_empty())
        .unwrap_or(name)
        .to_ascii_lowercase()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_extract_create_table_names_and_ignore_other_objects() {
        assert_eq!(
            create_table_name("CREATE TABLE t_a (id INT)").as_deref(),
            Some("t_a")
        );
        assert_eq!(
            create_table_name("CREATE TABLE IF NOT EXISTS \"T_B\" (id INT)").as_deref(),
            Some("t_b")
        );
        assert_eq!(
            create_table_name("CREATE TABLE bigfund.t_c AS SELECT 1").as_deref(),
            Some("t_c")
        );
        assert_eq!(
            create_table_name("CREATE GLOBAL TEMPORARY TABLE t_d (id INT)").as_deref(),
            Some("t_d")
        );
        assert_eq!(create_table_name("CREATE VIEW v AS SELECT 1"), None);
        assert_eq!(create_table_name("CREATE SEQUENCE s START WITH 1"), None);
        assert_eq!(create_table_name("CREATE INDEX ix ON t_a (id)"), None);
        // 注释与字符串字面量里的关键字不参与判定
        assert_eq!(
            create_table_name("-- CREATE TABLE fake\nCREATE VIEW v AS SELECT 'CREATE TABLE x'"),
            None
        );
    }

    #[test]
    fn should_compare_materialize_names_as_env_0015_does() {
        // ENV-0015 只做 lower($t) 比对；守卫不得比它宽松，否则缺表会推迟到
        // materialize 层才红（正是本守卫要消灭的形态）。
        assert_eq!(materialize_key("T_A"), "t_a");
        assert_eq!(materialize_key("bigfund.t_a"), "bigfund.t_a");
        assert_eq!(materialize_key("\"T_A\""), "\"t_a\"");
        assert_eq!(bare_table_name("bigfund.t_a"), "t_a");
    }
}
