//! 子命令执行与渲染（纯函数式：写 `out` 并返回退出码，便于契约测试）。

use crate::cli::args::{parse, usage, Command};
use crate::cli::{compile_dir, summarize, FileReport};
use crate::error::EngineError;
use crate::rules::RuleRegistry;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 退出码：0 成功 / 1 业务或编译失败 / 2 用法错误。
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

/// 执行一次 CLI 调用。
pub fn run(argv: &[String], out: &mut dyn Write) -> i32 {
    let cmd = match parse(argv) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            let _ = write!(out, "{}", usage());
            return EXIT_USAGE;
        }
    };
    match exec(cmd, out) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(out, "[{}] {}", e.code, e.message);
            EXIT_FAIL
        }
    }
}

fn exec(cmd: Command, out: &mut dyn Write) -> Result<i32, EngineError> {
    match cmd {
        Command::Help => {
            write!(out, "{}", usage()).map_err(io_err)?;
            Ok(EXIT_OK)
        }
        Command::Version => {
            writeln!(out, "spinduck {}", crate::version()).map_err(io_err)?;
            Ok(EXIT_OK)
        }
        Command::RulesProbe { path } => {
            let mut rules = RuleRegistry::default();
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| EngineError::env(12, format!("read rules: {e}")))?;
            rules.overlay(&raw)?;
            let rules = rules;
            writeln!(
                out,
                "rules: {} target_duckdb={} probe_duckdb={} match={}",
                path.display(),
                rules.target_duckdb,
                rules.probe_duckdb,
                rules.probe_versions_match()
            )
            .map_err(io_err)?;
            Ok(if rules.probe_versions_match() {
                EXIT_OK
            } else {
                EXIT_FAIL
            })
        }
        Command::Compile { paths } => compile_cmd(&paths, out),
        Command::Explain { file, routine } => explain_cmd(&file, &routine, out),
        Command::Materialize { .. } => materialize_cmd(cmd_materialize(cmd), out),
        Command::Sync { .. } => sync_cmd(cmd_sync(cmd), out),
        Command::Call { .. } => call_cmd(cmd_call(cmd), out),
    }
}

/// 兼容取包（避免在 match 中重复绑定）。
fn cmd_call(cmd: Command) -> CallParts {
    match cmd {
        Command::Call {
            db,
            rules,
            files,
            routine,
            args,
        } => CallParts {
            db,
            rules,
            files,
            routine,
            args,
        },
        _ => unreachable!("cmd_call only for Call"),
    }
}

struct MaterializeParts {
    db: PathBuf,
    table: String,
    where_clause: Option<String>,
}

fn cmd_materialize(cmd: Command) -> MaterializeParts {
    match cmd {
        Command::Materialize {
            db,
            table,
            where_clause,
        } => MaterializeParts {
            db,
            table,
            where_clause,
        },
        _ => unreachable!("cmd_materialize only for Materialize"),
    }
}

struct SyncParts {
    db: PathBuf,
    table: String,
    cols: Vec<String>,
}

fn cmd_sync(cmd: Command) -> SyncParts {
    match cmd {
        Command::Sync { db, table, cols } => SyncParts { db, table, cols },
        _ => unreachable!("cmd_sync only for Sync"),
    }
}

struct CallParts {
    db: Option<PathBuf>,
    rules: Option<PathBuf>,
    files: Vec<PathBuf>,
    routine: String,
    args: Vec<String>,
}

/// 语料目录的规则口径：内置种子为基座 + 各路径向上发现的 `spinduck.toml`
/// `[paths].rules` 叠加（§6 只增不删）。compile/explain 与 golden 用例同一基座，
/// 避免"体检绿、运行红"的口径分裂。
fn rules_for_corpus(paths: &[PathBuf]) -> Result<RuleRegistry, EngineError> {
    let mut files: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for p in paths {
        if let Some(proj) = crate::project::ProjectConfig::discover(p) {
            files.extend(proj.rules_files);
        }
    }
    RuleRegistry::default().with_project_rules(&files.iter().cloned().collect::<Vec<_>>())
}

fn compile_cmd(paths: &[PathBuf], out: &mut dyn Write) -> Result<i32, EngineError> {
    let rules = rules_for_corpus(paths)?;
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

fn explain_cmd(file: &Path, routine: &str, out: &mut dyn Write) -> Result<i32, EngineError> {
    let (text, _) = crate::cli::read_source(file)?;
    for line in crate::cli::explain::explain_routine(&text, routine)? {
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

#[cfg(feature = "native")]
fn call_cmd(parts: CallParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    use crate::cli::args::parse_literal;
    use crate::engine::Engine;
    // 内置种子规则恒为基座；--rules 为项目覆盖（叠加语义，§6 只增不删）
    let mut rules = RuleRegistry::default();
    if let Some(p) = &parts.rules {
        let raw = std::fs::read_to_string(p)
            .map_err(|e| EngineError::env(12, format!("read rules: {e}")))?;
        rules.overlay(&raw)?;
    }
    let ex = match &parts.db {
        Some(p) => crate::sql::DuckExecutor::open(&p.to_string_lossy())?,
        None => crate::sql::DuckExecutor::open_in_memory()?,
    };
    let mut eng = Engine::new(Box::new(ex), rules);
    for f in &parts.files {
        let (text, _) = crate::cli::read_source(f)?;
        eng.register(&text)?;
    }
    let args: Vec<crate::value::SpinValue> = parts.args.iter().map(|a| parse_literal(a)).collect();
    let res = eng.call(&parts.routine, &args)?;
    if res.outs.is_empty() && res.return_value.is_none() {
        writeln!(out, "CALL {} OK", parts.routine).map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    for (k, v) in &res.outs {
        writeln!(out, "OUT {} = {}", k, v.display()).map_err(io_err)?;
    }
    if let Some(v) = &res.return_value {
        writeln!(out, "RETURN {}", v.display()).map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

/// GaussDB 连接参数来自环境变量（避免命令行泄露口令，§10.4）。
#[cfg(feature = "gauss")]
fn open_gauss() -> Result<crate::io::GaussConn, EngineError> {
    let host = std::env::var("SPINDUCK_OG_HOST")
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_HOST is required".to_string()))?;
    let port: u16 = std::env::var("SPINDUCK_OG_PORT")
        .unwrap_or_else(|_| "5432".to_string())
        .parse()
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_PORT must be a port number".to_string()))?;
    let user = std::env::var("SPINDUCK_OG_USER").unwrap_or_else(|_| "gaussdb".to_string());
    let password = std::env::var("SPINDUCK_OG_PASSWORD")
        .map_err(|_| EngineError::env(14, "SPINDUCK_OG_PASSWORD is required".to_string()))?;
    let dbname = std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".to_string());
    crate::io::GaussConn::connect_parts(&host, port, &user, &password, &dbname)
}

#[cfg(feature = "gauss")]
fn materialize_cmd(parts: MaterializeParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    let mut og = open_gauss()?;
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    let n = og.materialize(&duck, &parts.table, parts.where_clause.as_deref())?;
    writeln!(out, "MATERIALIZE {} rows={}", parts.table, n).map_err(io_err)?;
    Ok(EXIT_OK)
}

#[cfg(not(feature = "gauss"))]
fn materialize_cmd(parts: MaterializeParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "materialize {} -> {} (where: {:?}) requires feature `gauss`",
            parts.table,
            parts.db.display(),
            parts.where_clause
        ),
    ))
}

#[cfg(feature = "gauss")]
fn sync_cmd(parts: SyncParts, out: &mut dyn Write) -> Result<i32, EngineError> {
    let mut og = open_gauss()?;
    let duck = crate::sql::DuckExecutor::open(&parts.db.to_string_lossy())?;
    let n = og.sync_append(&duck, &parts.table, &parts.cols)?;
    writeln!(out, "SYNC {} rows={}", parts.table, n).map_err(io_err)?;
    Ok(EXIT_OK)
}

#[cfg(not(feature = "gauss"))]
fn sync_cmd(parts: SyncParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "sync {} ({}) from {} requires feature `gauss`",
            parts.table,
            parts.cols.join(","),
            parts.db.display()
        ),
    ))
}

#[cfg(not(feature = "native"))]
fn call_cmd(parts: CallParts, _out: &mut dyn Write) -> Result<i32, EngineError> {
    Err(EngineError::env(
        12,
        format!(
            "call {} (args: {}, files: {:?}, db: {:?}, rules: {:?}) requires feature `native`",
            parts.routine,
            parts.args.join(","),
            parts.files,
            parts.db,
            parts.rules
        ),
    ))
}

fn io_err(e: std::io::Error) -> EngineError {
    EngineError::env(13, format!("stdout: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn run_str(items: &[&str]) -> (i32, String) {
        let mut buf: Vec<u8> = Vec::new();
        let code = run(&argv(items), &mut buf);
        (code, String::from_utf8(buf).unwrap())
    }

    #[test]
    fn should_print_usage_and_exit_2_on_usage_error() {
        let (code, out) = run_str(&["compile"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(out.contains("ENG-0010"));
        assert!(out.contains("compile <path>"));
    }

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

        let r = rules_for_corpus(std::slice::from_ref(&corpus)).expect("项目规则叠加");
        assert_eq!(r.functions["MYNVL"], "coalesce");
        assert_eq!(r.functions["NVL"], "coalesce", "内置种子仍为基座");
        // 项目声明对其下所有语料目录生效（子目录继承，无需重复声明）
        let nested = root.join("sql/routines/pkg");
        std::fs::create_dir_all(&nested).unwrap();
        let r2 = rules_for_corpus(&[nested]).expect("子目录继承项目规则");
        assert_eq!(r2.functions["MYNVL"], "coalesce");
        // 无项目声明的目录回落内置种子（compile 与 call 同口径的基座）
        let outside = tempfile::tempdir().unwrap();
        let plain = rules_for_corpus(&[outside.path().to_path_buf()]).expect("无项目声明也可编译");
        assert_eq!(plain.functions.get("MYNVL"), None);
    }

    #[test]
    fn should_pass_rules_probe_when_versions_match() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rules.toml");
        std::fs::write(&p, "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.5.5\"\n").unwrap();
        let (code, out) = run_str(&["rules", "probe", &p.to_string_lossy()]);
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("match=true"), "{out}");
    }

    #[test]
    fn should_fail_rules_probe_on_version_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rules.toml");
        std::fs::write(&p, "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.4.4\"\n").unwrap();
        let (code, out) = run_str(&["rules", "probe", &p.to_string_lossy()]);
        assert_eq!(code, EXIT_FAIL);
        assert!(out.contains("match=false"), "{out}");
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

    #[cfg(feature = "native")]
    #[test]
    fn should_register_file_and_call_routine_with_out_param() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("pkg.sql");
        std::fs::write(
            &f,
            "CREATE OR REPLACE PROCEDURE prc_calc(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 3; END;",
        )
        .unwrap();
        let (code, out) = run_str(&[
            "call",
            "--file",
            &f.to_string_lossy(),
            "prc_calc",
            "14",
            "null",
        ]);
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("OUT p_o = 42"), "{out}");
    }
}
