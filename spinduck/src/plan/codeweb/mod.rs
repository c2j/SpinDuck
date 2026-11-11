//! codeweb 0.9.7 JSON 的瘦投影与闭包纯函数。
//!
//! 不启进程、不读盘：调用方给出已解析的图、SpinDuck 项目根，以及表/视图/序列
//! 归一名到项目内相对 DDL 路径的索引。生产 `build` 不调用这里。

mod parse;
mod walk;

use std::path::Path;

pub use parse::{parse_codeweb_json, CodewebGraph};
pub use walk::closure_from_graph;

/// 闭包求值的项目侧输入：根用于剥路径，索引只回答「这个关系名的 CREATE 在哪」。
#[derive(Debug, Clone, Copy)]
pub struct ClosureContext<'a> {
    /// SpinDuck 项目根。JSON `file` 是绝对路径时按字符串前缀剥成相对路径。
    pub project_root: &'a Path,
    /// 归一名（及尾段）→ 相对 DDL 路径。键由调用方准备，本函数不扫磁盘。
    pub ddl_files: &'a std::collections::BTreeMap<String, String>,
}

/// 一条 `[[ref]]` 行。`object` 是不带 `relation:` / `sequence:` / `routine:` 前缀的名字。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClosureRef {
    /// 例程用目录键；表/视图/序列用 `normalize` 后的归一名。
    pub object: String,
    /// `table` | `view` | `sequence` | `routine`。
    pub kind: String,
    /// `read` | `write` | `type` | `call`。
    pub role: String,
    /// 边的源：例程目录键，或视图归一名。
    pub from: String,
    /// `<相对路径>:<JSON line>`，行号原样拷贝（含 0）。绝不写绝对路径。
    pub at: String,
}

/// 一条动态调用。不进入缺件。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClosureDynamic {
    /// codeweb 的 Debug 文本，例如 `PlVariable(["v_sql"])`。
    pub raw: String,
    /// 发出这条边的例程目录键。
    pub from: String,
    /// 同 [`ClosureRef::at`]。
    pub at: String,
}

/// 一条缺件。`object` 保留 `relation:` / `sequence:` / `routine:` 前缀。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClosureMissing {
    /// 带前缀的对象标识。
    pub object: String,
    /// 引用它的例程目录键或视图归一名。
    pub from: String,
    /// 同 [`ClosureRef::at`]。缺件沿用边的位置，不扫描 SQL。
    pub at: String,
}

/// 一个入口在瘦图上的闭包。清单已按元组排序去重。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphClosure {
    /// 入口的目录键（小写，不经 `normalize`）。
    pub target: String,
    /// 关系、序列与被调例程。
    pub refs: Vec<ClosureRef>,
    /// `dynamic` 边。
    pub dynamic: Vec<ClosureDynamic>,
    /// 缺件。
    pub missing: Vec<ClosureMissing>,
    /// 闭包内、且源文件落在项目根下的例程相对路径。
    pub ddl_routines: Vec<String>,
    /// 项目 DDL 索引命中的表/视图/序列相对路径。
    pub ddl_tables: Vec<String>,
}

/// JSON `file` 落在 `root` 下时返回剥掉根之后的相对路径（`/` 分隔）。
pub(super) fn project_rel(root: &Path, file: &str) -> Option<String> {
    let root = root.to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return None;
    }
    let file = file.replace('\\', "/");
    let prefix = format!("{root}/");
    let rest = file.strip_prefix(&prefix)?;
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    Some(rest.to_string())
}

/// 边的 `at`。根下的绝对路径剥成相对路径；已是相对路径且没有 `..` 段时原样保留。
/// 只有绝对路径且不在项目根下时才退回文件名，避免把绝对路径写进计划。
pub(super) fn at_text(root: &Path, file: &str, line: u64) -> String {
    let rel = if let Some(stripped) = project_rel(root, file) {
        stripped
    } else {
        let norm = file.replace('\\', "/");
        let bytes = norm.as_bytes();
        let absolute = norm.starts_with('/')
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && bytes[2] == b'/');
        let escapes = norm.split('/').any(|seg| seg == "..");
        if !norm.is_empty() && !absolute && !escapes {
            norm
        } else {
            file_only(file)
        }
    };
    format!("{rel}:{line}")
}

fn file_only(file: &str) -> String {
    let norm = file.replace('\\', "/");
    match norm.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests;
