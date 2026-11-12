//! `compile` 语料体检与 `explain` IR 打印：两者的规则口径必须同源（见 `rules_for_paths`）。

use super::{io_err, EXIT_FAIL, EXIT_OK};
use crate::cli::{compile_dir, summarize, FileReport};
use crate::error::EngineError;
use crate::rules::RuleRegistry;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 对一组路径做项目规则发现：向上查找 `spinduck.toml` `[paths].rules` 并集（§6 只增不删）。
pub(super) fn discover_rules_files(paths: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    let mut files = std::collections::BTreeSet::new();
    for p in paths {
        if let Some(proj) = crate::project::ProjectConfig::discover(p) {
            files.extend(proj.rules_files);
        }
    }
    files
}

/// 语料目录的规则口径：内置种子为基座 + 各路径向上发现的 `spinduck.toml`
/// `[paths].rules` 叠加（§6 只增不删）。compile/explain 与 golden 用例同一基座，
/// 避免"体检绿、运行红"的口径分裂。
pub(super) fn rules_for_paths(paths: &[PathBuf]) -> Result<RuleRegistry, EngineError> {
    let files = discover_rules_files(paths);
    RuleRegistry::default().with_project_rules(&files.iter().cloned().collect::<Vec<_>>())
}

pub(super) fn compile_cmd(paths: &[PathBuf], out: &mut dyn Write) -> Result<i32, EngineError> {
    let rules = rules_for_paths(paths)?;
    let mut all: Vec<FileReport> = Vec::new();
    for p in paths {
        if p.is_dir() {
            for r in compile_dir(p, &rules)? {
                all.push(FileReport {
                    file: format!("{}/{}", p.display(), r.file),
                    ..r
                });
            }
        } else {
            let one = compile_single(p, &rules)?;
            all.push(one);
        }
    }
    let s = summarize(&all);
    for r in &all {
        let mark = if r.non_utf8 { "  (non-utf8)" } else { "" };
        match &r.error {
            None => writeln!(out, "OK   {}  routines={}{}", r.file, r.routines, mark),
            Some(e) => writeln!(out, "FAIL {}  [{}] {}{}", r.file, e.code, e.message, mark),
        }
        .map_err(io_err)?;
    }
    writeln!(
        out,
        "summary: files={} ok={} failed={} routines={}",
        s.files, s.ok, s.failed, s.routines
    )
    .map_err(io_err)?;
    for (code, n) in failure_histogram(&all) {
        writeln!(out, "reason {} x{}", code, n).map_err(io_err)?;
    }
    Ok(if s.failed == 0 { EXIT_OK } else { EXIT_FAIL })
}

pub(super) fn explain_cmd(
    file: &Path,
    routine: &str,
    trace: Option<&Path>,
    out: &mut dyn Write,
) -> Result<i32, EngineError> {
    let text = crate::cli::read_source_strict(file)?;
    let lines = match trace {
        None => crate::cli::explain::explain_routine(&text, routine)?,
        Some(log) => {
            let log_text = std::fs::read_to_string(log).map_err(|e| {
                EngineError::env(12, format!("read trace log {}: {e}", log.display()))
            })?;
            crate::cli::explain::explain_traced(&text, routine, &log_text)?
        }
    };
    for line in lines {
        writeln!(out, "{line}").map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

fn compile_single(path: &Path, rules: &RuleRegistry) -> Result<FileReport, EngineError> {
    let (text, non_utf8) = crate::cli::read_source(path)?;
    let mut eng = crate::engine::Engine::new(Box::new(crate::cli::NoopExecutor), rules.clone());
    Ok(match eng.register(&text) {
        Ok(n) => FileReport {
            file: path.display().to_string(),
            routines: n,
            error: None,
            non_utf8,
        },
        Err(e) => FileReport {
            file: path.display().to_string(),
            routines: 0,
            error: Some(e),
            non_utf8,
        },
    })
}

/// 失败原因直方图（按错误码降序计数），用于优先级排序。
fn failure_histogram(reports: &[FileReport]) -> Vec<(String, usize)> {
    let mut m: BTreeMap<String, usize> = BTreeMap::new();
    for r in reports {
        if let Some(e) = &r.error {
            *m.entry(e.code.to_string()).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run::{run_str, EXIT_FAIL, EXIT_OK};

    #[test]
    fn should_report_compile_summary_and_reason_histogram() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("ok.sql"),
            "CREATE OR REPLACE PROCEDURE p_ok IS BEGIN NULL; END;",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("bad.sql"),
            "CREATE OR REPLACE PROCEDURE p_bad IS BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;",
        )
        .unwrap();
        let (code, out) = run_str(&["compile", &tmp.path().to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(
            out.contains("summary: files=2 ok=1 failed=1 routines=1"),
            "{out}"
        );
        assert!(out.contains("reason ENG-0022 x1"), "{out}");
    }

    #[test]
    fn should_overlay_project_rules_when_compiling() {
        // 语料目录所在项目（spinduck.toml `[paths].rules`）的规则必须参与编译，
        // 否则 compile 体检口径与 call/golden 不一致（评审第三轮 F-编译规则）
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
        let corpus = root.join("sql/routines");
        std::fs::create_dir_all(&corpus).unwrap();

        let r = rules_for_paths(std::slice::from_ref(&corpus)).expect("项目规则叠加");
        assert_eq!(r.functions["MYNVL"], "coalesce");
        assert_eq!(r.functions["NVL"], "coalesce", "内置种子仍为基座");
        // 项目声明对其下所有语料目录生效（子目录继承，无需重复声明）
        let nested = root.join("sql/routines/pkg");
        std::fs::create_dir_all(&nested).unwrap();
        let r2 = rules_for_paths(&[nested]).expect("子目录继承项目规则");
        assert_eq!(r2.functions["MYNVL"], "coalesce");
        // 无项目声明的目录回落内置种子（compile 与 call 同口径的基座）
        let outside = tempfile::tempdir().unwrap();
        let plain = rules_for_paths(&[outside.path().to_path_buf()]).expect("无项目声明也可编译");
        assert_eq!(plain.functions.get("MYNVL"), None);
    }

    #[test]
    fn should_explain_routine_as_stable_ir_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("p.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE PROCEDURE prc_x(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 2; END;",
        )
        .unwrap();
        let (code, out) = run_str(&["explain", "--file", &f.to_string_lossy(), "prc_x"]);
        assert_eq!(code, EXIT_OK, "{out}");
        // 参数由调用方预绑定，IR 只含函数体：唯一一条赋值
        assert_eq!(out, "0\tassign\tp_o := (p_i * 2)\n", "{out}");
    }
}
