//! 研发态项目结构（§12.1 `spinduck.toml` 项目模式）：项目声明 + 用例声明的识别。
//!
//! 引擎与 CLI 只通过本模块定位业务资产（DDL/例程/桩/数据集/基线），
//! 路径与用例定义一律来自 `spinduck.toml` / `*.case.toml`，代码零硬编码。
//! 黄金对比的 harness（tests/golden_*）即本结构的首个消费者。
//!
//! 规模拆分（AGENTS §3，单文件 ≤500 行）：`case`（用例声明）、
//! `fs`（目录扫描与配置读取）；`tests` 为本模块单测。

mod case;
mod entry;
mod fs;

#[cfg(test)]
mod tests;

pub use case::{AssertKind, CaseAssert, CaseConfig, CaseKind, Scenario};
pub use entry::EntryConfig;

use crate::error::EngineError;
use crate::project::fs::{cfg_err, list_dir_suffix, read_text, sql_files};
use std::path::{Path, PathBuf};

/// 项目声明（`spinduck.toml` 中引擎消费的字段；未识别的段落忽略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    /// 目标内核版本（M1 锚定；与规则文件 probe_duckdb 一致性由 rules probe 校验）
    pub kernel: String,
    pub ddl_dir: PathBuf,
    pub routines_dir: PathBuf,
    pub stubs_dirs: Vec<PathBuf>,
    pub rules_files: Vec<PathBuf>,
    pub probes_dir: PathBuf,
    pub datasets_dir: PathBuf,
    pub baselines_dir: PathBuf,
    pub home_dir: PathBuf,
    /// `[cases] include` 的条目（如 `cases/*.case.toml`）
    pub case_includes: Vec<String>,
    /// `[[entry]]` 声明：本项目可被调用的存过全名（可多个；链路校验起点）
    pub entries: Vec<EntryConfig>,
    /// 项目根（相对路径基准）
    pub root: PathBuf,
}

impl ProjectConfig {
    /// 读取项目声明；相对路径以项目根为基准解析。
    ///
    /// 声明文件两处（§11.1/§12.1）：研发态 `<dir>/spinduck.toml`（手写真源，优先）、
    /// 运行态 `<dir>/conf/spinduck.toml`（`verify` 生成的快照，路径已绝对化，
    /// 故运行态目录自带可加载声明）。
    pub fn load(dir: &Path) -> Result<Self, EngineError> {
        let file = fs::project_file(dir).ok_or_else(|| {
            EngineError::env(
                30,
                format!(
                    "未在 {} 找到项目声明（spinduck.toml 或 conf/spinduck.toml）",
                    dir.display()
                ),
            )
        })?;
        let text = read_text(&file)?;
        Self::from_toml_str(&text, dir)
    }

    /// 从 `start` 逐级向上查找项目根（含 `spinduck.toml` 或运行态快照的目录）。
    ///
    /// 用途：CLI 编译/校验语料目录时继承所在项目的规则与入口声明，
    /// 避免"口径靠调用参数临时拼装"（§12.1）。找不到返回 `None`（回落内置种子）；
    /// 命中但解析失败同样返回 `None`——显式错误路径由 `ProjectConfig::load` 承担
    /// （调用方需要严格语义时直接传项目根）。
    pub fn discover(start: &Path) -> Option<Self> {
        start
            .ancestors()
            .find(|d| fs::project_file(d).is_some())
            .and_then(|root| Self::load(root).ok())
    }

    /// 从 TOML 文本解析（root 为相对路径基准）。
    pub fn from_toml_str(text: &str, root: &Path) -> Result<Self, EngineError> {
        let v: toml::Value = toml::from_str(text).map_err(|e| cfg_err("spinduck.toml", e))?;
        let project = v
            .get("project")
            .ok_or_else(|| EngineError::env(30, "spinduck.toml: 缺少 [project] 段"))?;
        let name = project
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| EngineError::env(30, "spinduck.toml: [project].name 必填"))?;
        let kernel = project
            .get("kernel")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        let paths = v
            .get("paths")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        let get_path = |key: &str, default: &str| -> PathBuf {
            paths
                .get(key)
                .and_then(|x| x.as_str())
                .map(|s| root.join(s))
                .unwrap_or_else(|| root.join(default))
        };
        let stubs_dirs = match paths.get("stubs") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| root.join(s))
                .collect(),
            Some(toml::Value::String(s)) => vec![root.join(s)],
            _ => Vec::new(),
        };
        let rules_files = match paths.get("rules") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| root.join(s))
                .collect(),
            Some(toml::Value::String(s)) => vec![root.join(s)],
            _ => Vec::new(),
        };
        let cases = v
            .get("cases")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        let case_includes = match cases.get("include") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(String::from)
                .collect(),
            _ => vec!["cases/*.case.toml".to_string()],
        };
        let mut entries = Vec::new();
        for e in v
            .get("entry")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default()
        {
            entries.push(EntryConfig::from_toml_value(&e)?);
        }

        Ok(Self {
            name: name.to_string(),
            kernel: kernel.to_string(),
            ddl_dir: get_path("ddl", "sql/ddl"),
            routines_dir: get_path("routines", "sql/routines"),
            stubs_dirs,
            rules_files,
            probes_dir: get_path("probes", "probes"),
            datasets_dir: get_path("datasets", "cases/datasets"),
            baselines_dir: get_path("baselines", "cases/baselines"),
            home_dir: get_path("home", ".spinduck"),
            case_includes,
            entries,
            root: root.to_path_buf(),
        })
    }

    /// 表/序列 DDL（`sql/ddl/*.sql` 与 `sql/ddl/tables/*.sql`，排序合并）。
    ///
    /// 约定：表在 `sql/ddl/` 或 `sql/ddl/tables/`（两侧都建）；
    /// 视图在 `sql/ddl/views/`（两侧都建，被测侧走同一改写管线）。
    pub fn ddl_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        let mut out = sql_files(&self.ddl_dir, false)?;
        out.extend(sql_files(&self.ddl_dir.join("tables"), false)?);
        out.sort();
        Ok(out)
    }

    /// 视图源码（`sql/ddl/views/*.sql`）：只用于参照侧部署。
    pub fn view_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        sql_files(&self.ddl_dir.join("views"), false)
    }

    /// 例程源码（`sql/routines/*.sql`，排序）。
    pub fn routine_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        sql_files(&self.routines_dir, false)
    }

    /// 数据集种子（`cases/datasets/*.sql`，排序）。
    pub fn dataset_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        sql_files(&self.datasets_dir, false)
    }

    /// 桩包源码（`[paths].stubs` 各目录的 `*.sql`，按目录顺序拼接）。
    pub fn stub_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        let mut out = Vec::new();
        for d in &self.stubs_dirs {
            out.extend(sql_files(d, false)?);
        }
        Ok(out)
    }

    /// 用例定义（`[cases].include` 中 `*.case.toml`；当前仅支持 `dir/*.suffix` 形态）。
    pub fn case_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        let mut out = Vec::new();
        for pat in &self.case_includes {
            let Some((dir_part, file_pat)) = pat.rsplit_once('/') else {
                continue;
            };
            let dir = self.root.join(dir_part);
            let suffix = file_pat.trim_start_matches('*');
            let mut files = list_dir_suffix(&dir, suffix)?;
            out.append(&mut files);
        }
        out.sort();
        Ok(out)
    }
}
