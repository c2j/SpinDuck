//! 项目目录扫描与配置文件读取（大小上限、后缀匹配、排序）。

use crate::error::EngineError;
use std::path::{Path, PathBuf};

/// 目录内单次读取上限（防止误把大文件当配置）。
const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// 目录内的项目声明文件：手写 `spinduck.toml` 优先，其次 `compile --sync-manifest`
/// 落下的运行态快照 `conf/spinduck.runtime.toml`（§11.1 `conf/` 分区）。
pub(crate) fn project_file(dir: &Path) -> Option<PathBuf> {
    let dev = dir.join("spinduck.toml");
    if dev.is_file() {
        return Some(dev);
    }
    let snapshot = dir.join("conf").join(super::manifest::SNAPSHOT_FILE);
    snapshot.is_file().then_some(snapshot)
}

pub(crate) fn str_list(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn cfg_err(what: &str, e: toml::de::Error) -> EngineError {
    EngineError::env(30, format!("{what} 解析失败: {e}"))
}

pub(crate) fn read_text(path: &Path) -> Result<String, EngineError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", path.display())))?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(EngineError::env(
            30,
            format!("{} 超过配置读取上限（1MiB）", path.display()),
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", path.display())))
}

/// 目录内 `*.sql`（非递归，按文件名排序；目录不存在返回空）。
pub(crate) fn sql_files(dir: &Path, _rec: bool) -> Result<Vec<PathBuf>, EngineError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = list_dir_suffix(dir, "sql")?;
    files.sort();
    Ok(files)
}

pub(crate) fn list_dir_suffix(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>, EngineError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let rd = std::fs::read_dir(dir)
        .map_err(|e| EngineError::env(30, format!("read_dir {}: {e}", dir.display())))?;
    let mut out = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| EngineError::env(30, format!("dir entry: {e}")))?;
        let p = ent.path();
        let name = p.file_name().and_then(|x| x.to_str()).unwrap_or_default();
        // 后缀按文件名结尾匹配：支持多段后缀（`.case.toml`）与简单扩展名（`sql`）
        let matched = if suffix.starts_with('.') {
            name.ends_with(suffix)
        } else {
            name.ends_with(&format!(".{suffix}"))
        };
        if p.is_file() && matched {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}
