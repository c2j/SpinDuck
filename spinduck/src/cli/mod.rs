//! CLI 命令层（可测逻辑；`src/bin/spinduck.rs` 只做参数转交与退出码）。
//!
//! 子命令与退出码契约见 AGENTS.md §8 与 docs/architecture.md §11。

pub mod args;
mod compare;
mod deploy;
pub mod explain;
mod plan;
mod query;
pub mod run;
mod verify;

use crate::engine::Engine;
use crate::error::EngineError;
use crate::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use crate::rules::RuleRegistry;
use std::path::{Path, PathBuf};

/// 编译期"无副作用执行器"：只做例程编译，不触碰内核（宏登记被丢弃）。
///
/// 用于语料批量编译体检：编译结果只取决于解析/IR 编译，与内核无关，
/// 因此不引入 DuckDB 依赖即可覆盖 198 个语料文件。
#[derive(Debug, Default)]
pub struct NoopExecutor;

impl SqlExecutor for NoopExecutor {
    fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        Ok(RowStream::new(Vec::new(), Vec::new()))
    }
    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        Ok(ExecOutcome { rows_affected: 0 })
    }
}

/// 单文件编译结果。
#[derive(Debug, Clone)]
pub struct FileReport {
    /// 相对扫描根的路径（斜杠统一为 `/`，跨平台可比）。
    pub file: String,
    pub routines: usize,
    pub error: Option<EngineError>,
    /// 源文件非 UTF-8（按 GBK 语料现实：以有损 UTF-8 解码，仅注释可能变形）
    pub non_utf8: bool,
}

/// 读取源码文本：非 UTF-8 时按有损 UTF-8 解码（SQL 关键字/标识符为 ASCII，
/// 只有注释与字符串字面量可能被替换字符污染），并标记 `non_utf8`。
pub fn read_source(path: &std::path::Path) -> Result<(String, bool), EngineError> {
    let bytes = std::fs::read(path)
        .map_err(|e| EngineError::env(10, format!("read {}: {e}", path.display())))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok((text, false)),
        Err(e) => Ok((String::from_utf8_lossy(e.as_bytes()).into_owned(), true)),
    }
}

/// 严格读取源码文本：UTF-8 → GBK 两把判定，都不成立即红（design §3.5 / D9①②）。
///
/// 与 [`read_source`] 的分工：`compile` 是语料体检面，保留有损解码 + `(non-utf8)` 标记；
/// `call`/`explain`/`plan`/`deploy`/`query`/`compare` 是执行与比对面，同一份源码只可能因
/// 编码不同**报错**或被**正确还原**，不可能返回不同的值。实现收口在 [`crate::source`]，
/// 诊断仍是 `ENV-0030` 形状。
pub fn read_source_strict(path: &std::path::Path) -> Result<String, EngineError> {
    crate::source::read_text(path)
}

/// 汇总。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub ok: usize,
    pub failed: usize,
    pub routines: usize,
}

/// 递归收集 `*.sql`（排序保证可重复）。
pub fn scan_sql_files(root: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), EngineError> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| EngineError::env(10, format!("read_dir {}: {e}", dir.display())))?;
    for ent in rd {
        let ent = ent.map_err(|e| EngineError::env(10, format!("dir entry: {e}")))?;
        let p = ent.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else if p.extension().and_then(|x| x.to_str()) == Some("sql") {
            out.push(p);
        }
    }
    Ok(())
}

/// 逐文件编译（每个文件独立 Engine，避免跨文件例程名相互污染）。
pub fn compile_dir(root: &Path, rules: &RuleRegistry) -> Result<Vec<FileReport>, EngineError> {
    let files = scan_sql_files(root)?;
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let rel = f
            .strip_prefix(root)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        let mut eng = Engine::new(Box::new(NoopExecutor), rules.clone());
        let report = match read_source(&f) {
            Ok((text, non_utf8)) => match eng.register(&text) {
                Ok(n) => FileReport {
                    file: rel,
                    routines: n,
                    error: None,
                    non_utf8,
                },
                Err(e) => FileReport {
                    file: rel,
                    routines: 0,
                    error: Some(e),
                    non_utf8,
                },
            },
            Err(e) => FileReport {
                file: rel,
                routines: 0,
                error: Some(e),
                non_utf8: false,
            },
        };
        out.push(report);
    }
    Ok(out)
}

/// 汇总统计。
pub fn summarize(reports: &[FileReport]) -> Summary {
    let mut s = Summary {
        files: reports.len(),
        ..Default::default()
    };
    for r in reports {
        match r.error {
            Some(_) => s.failed += 1,
            None => {
                s.ok += 1;
                s.routines += r.routines;
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn should_scan_sql_files_recursively_in_stable_order() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        write(tmp.path(), "b.sql", "SELECT 1");
        write(tmp.path(), "a.sql", "SELECT 1");
        write(&sub, "c.sql", "SELECT 1");
        write(tmp.path(), "ignored.txt", "x");

        let files = scan_sql_files(tmp.path()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.sql", "b.sql", "c.sql"]);
    }

    #[test]
    fn should_report_registered_routine_count_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "ok.sql",
            "CREATE OR REPLACE PROCEDURE p_ok IS BEGIN NULL; END;",
        );
        write(tmp.path(), "empty.sql", "-- 只有注释\n");
        let reports = compile_dir(tmp.path(), &crate::rules::RuleRegistry::default()).unwrap();
        assert_eq!(reports.len(), 2);
        let ok = reports.iter().find(|r| r.file == "ok.sql").unwrap();
        assert!(ok.error.is_none(), "err: {:?}", ok.error);
        assert_eq!(ok.routines, 1);
        let empty = reports.iter().find(|r| r.file == "empty.sql").unwrap();
        assert!(empty.error.is_none());
        assert_eq!(empty.routines, 0);
    }

    #[test]
    fn should_capture_compile_error_with_code_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "bad.sql",
            "CREATE OR REPLACE PROCEDURE p_bad IS BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;",
        );
        let reports = compile_dir(tmp.path(), &crate::rules::RuleRegistry::default()).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        let e = r.error.as_ref().unwrap();
        assert_eq!(
            e.code.to_string(),
            "ENG-0022",
            "未纳入语句必须编译期显式拒绝"
        );
        assert_eq!(r.routines, 0);
    }

    #[test]
    fn should_summarize_reports_by_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "ok.sql",
            "CREATE OR REPLACE PROCEDURE p_ok IS BEGIN NULL; END;",
        );
        write(
            tmp.path(),
            "bad.sql",
            "CREATE OR REPLACE PROCEDURE p_bad IS BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;",
        );
        let reports = compile_dir(tmp.path(), &crate::rules::RuleRegistry::default()).unwrap();
        let s = summarize(&reports);
        assert_eq!(s.files, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.ok, 1);
        assert_eq!(s.routines, 1);
    }
}
