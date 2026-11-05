//! 黄金对比共享 harness（§12.1 项目模式的首个消费者）。
//!
//! 全部路径与用例定义来自项目资产：`<case>/spinduck.toml` + `<case>/cases/*.case.toml`；
//! 本模块只负责"部署 → 执行 → 逐值/逐行比对 → 基线漂移检查"的通用流程。

#![cfg(feature = "gauss")]
#![allow(dead_code)] // 各测试 crate 只用到其中一部分

use spinduck_core::engine::Engine;
use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::io::GaussConn;
use spinduck_core::project::{CaseConfig, CaseKind, ProjectConfig};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use std::path::{Path, PathBuf};

const MAX_STACK: usize = 256 * 1024 * 1024;

/// 同一测试二进制内的用例串行化：容器侧用例都会重建应用 schema，
/// 并行执行会互相踩（duplicate schema）。跨二进制由 cargo 顺序执行保证。
static GOLDEN_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

pub fn golden_root() -> PathBuf {
    if let Ok(p) = std::env::var("SPINDUCK_GOLDEN_DIR") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .join("golden")
}

pub fn og() -> Option<GaussConn> {
    let host = std::env::var("SPINDUCK_OG_HOST").ok()?;
    let port: u16 = std::env::var("SPINDUCK_OG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let user = std::env::var("SPINDUCK_OG_USER").unwrap_or_else(|_| "gaussdb".into());
    let password = std::env::var("SPINDUCK_OG_PASSWORD").expect("SPINDUCK_OG_PASSWORD");
    let dbname = std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".into());
    Some(GaussConn::connect_parts(&host, port, &user, &password, &dbname).expect("connect gaussdb"))
}

/// 数值/文本归一（与既有 golden 用例同口径：按值比较，忽略 scale 与尾零）。
pub fn norm(v: Option<String>) -> String {
    let Some(s) = v else {
        return "NULL".to_string();
    };
    let t = s.trim().trim_start_matches('+');
    // A 模式（Oracle 语义）：空字符串即 NULL，两侧归一同口径
    if t.is_empty() {
        return "NULL".to_string();
    }
    let t = if let Some(rest) = t.strip_prefix('.') {
        format!("0.{rest}")
    } else if let Some(rest) = t.strip_prefix("-.") {
        format!("-0.{rest}")
    } else {
        t.to_string()
    };
    if t.contains('.') {
        let t2 = t.trim_end_matches('0').trim_end_matches('.');
        if t2.is_empty() || t2 == "-" {
            "0".to_string()
        } else {
            t2.to_string()
        }
    } else {
        t
    }
}

/// Oracle 风格 `/` 分隔符拆块 + 序列 MAXVALUE 越界截断（V6 已知项）。
pub fn split_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.trim() == "/" {
            let c = cur.trim();
            if !c.is_empty() {
                chunks.push(c.to_string());
            }
            cur.clear();
        } else {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    let c = cur.trim();
    if !c.is_empty() {
        chunks.push(c.to_string());
    }
    chunks
        .into_iter()
        .map(|c| c.replace("999999999999999999999999999", "9223372036854775807"))
        .collect()
}

/// 场景参数：SQL 字面量形态（`'x'` / `null` / 数字）→ `SpinValue`。
pub fn parse_arg(lit: &str) -> SpinValue {
    let t = lit.trim();
    if t.eq_ignore_ascii_case("null") {
        return SpinValue::Null;
    }
    if let Some(inner) = t.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return SpinValue::Str(inner.replace("''", "'"));
    }
    SpinValue::Str(t.to_string())
}

fn err(ctx: &str, e: impl std::fmt::Display) -> ! {
    panic!("{ctx}: {e}")
}

pub fn run_case(name: &str) {
    let owned = name.to_string();
    let h = std::thread::Builder::new()
        .stack_size(MAX_STACK)
        .spawn(move || {
            // 锁在子线程内获取：MutexGuard 非 Send，不能跨线程移动
            let lock = GOLDEN_LOCK.get_or_init(|| std::sync::Mutex::new(()));
            let _serialized = lock.lock().unwrap_or_else(|p| p.into_inner());
            run_case_inner(&owned)
        })
        .expect("spawn big-stack thread");
    h.join().expect("golden case panicked");
}

fn run_case_inner(name: &str) {
    let dir = golden_root().join(name);
    let project = ProjectConfig::load(&dir).unwrap_or_else(|e| err("加载 spinduck.toml", e));
    let case_files = project.case_files().unwrap_or_else(|e| err("枚举 case", e));
    assert_eq!(case_files.len(), 1, "{name}: 期望恰好一个 *.case.toml");
    let case = CaseConfig::load(&case_files[0]).unwrap_or_else(|e| err("加载 case.toml", e));
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
    };

    // ── 参照侧：A 模式容器整体重建 → 部署 → 种子 → 执行 ──
    og.exec_script("DROP SCHEMA IF EXISTS gaussdb CASCADE;")
        .unwrap();
    og.exec_script("CREATE SCHEMA gaussdb;").unwrap();
    deploy_native(&mut og, &project, &case).unwrap_or_else(|e| err("参照侧部署", e.message));
    let dataset = project
        .datasets_dir
        .read_dir()
        .map(|rd| {
            let mut v: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
                .collect();
            v.sort();
            v
        })
        .unwrap_or_default();
    for f in dataset {
        og.exec_script(&std::fs::read_to_string(&f).unwrap())
            .unwrap_or_else(|e| err(&format!("种子 {}", f.display()), e));
    }
    let native = run_native(&mut og, &case);

    // ── 被测侧：DuckDB 表（不含视图）+ 占位视图 → 例程/桩注册 → 物化 → 执行 ──
    let mut eng = build_engine(&project).unwrap_or_else(|e| err("被测侧构建", e));
    materialize_inputs(&mut og, &mut eng, &case).unwrap_or_else(|e| err("物化输入表", e));
    let spin = run_spin(&mut eng, &case);

    // ── 输出表行数一致性（防"输出表未物化"导致的漏比）──
    for t in &case.outputs {
        let base_n = og
            .fetch_rows(&format!("SELECT count(*) FROM {t}"))
            .map(|(_, r)| r[0][0].clone().unwrap_or_default())
            .unwrap_or_else(|e| err(&format!("参照侧 {t} 计数"), e.message));
        let mut rs = eng
            .ex
            .query(&format!("SELECT count(*) FROM {t}"), &BindMap::new())
            .unwrap_or_else(|e| err(&format!("被测侧 {t} 计数"), e.message));
        let got = rs
            .fetch_many(1)
            .first()
            .and_then(|r| r.first().map(|v| v.as_str_loose()))
            .unwrap_or_default();
        assert_eq!(
            norm(Some(got)),
            norm(Some(base_n)),
            "[{name}] 输出表 {t} 行数不一致"
        );
    }

    // ── 三方比对：参照 = 被测 = 基线 ──
    let baseline = load_baseline(&project, &case);
    for (i, sc) in case.scenarios.iter().enumerate() {
        let why = if sc.why.is_empty() {
            format!("场景 {}", i + 1)
        } else {
            sc.why.clone()
        };
        assert_eq!(
            spin[i],
            native[i],
            "[{name}] {why}: SpinDuck 与 A 模式基线不一致\n  SpinDuck: {}\n  参照侧  : {}",
            spin[i].describe(),
            native[i].describe()
        );
        assert_eq!(
            spin[i], baseline[i],
            "[{name}] {why}: 与提交基线 expected.json 不一致（行为变化需单独评审）"
        );
        if let Some(extra) = &case.extra_sql {
            let (_, b_rows) = og
                .fetch_rows(extra)
                .unwrap_or_else(|e| err("参照侧附加输出读取", e.message));
            let base_extra: Vec<Vec<String>> = b_rows
                .iter()
                .map(|r| r.iter().map(|c| norm(c.clone())).collect())
                .collect();
            let mut rs = eng
                .ex
                .query(extra, &BindMap::new())
                .unwrap_or_else(|e| err("被测侧附加输出读取", e.message));
            let got_extra: Vec<Vec<String>> = rs
                .fetch_many(1000)
                .iter()
                .map(|r| r.iter().map(|v| norm(Some(v.as_str_loose()))).collect())
                .collect();
            assert_eq!(
                got_extra, base_extra,
                "[{name}] {why}: 附加输出（`{extra}`）不一致"
            );
            println!("OK [{name}] {why} [extra]: rows={}", got_extra.len());
        }
        println!("OK [{name}] {why}: {}", spin[i].describe());
    }
}

/// 容器侧部署：表 → 视图 → 桩 → 例程 → 参照侧专用例程。
fn deploy_native(
    og: &mut GaussConn,
    project: &ProjectConfig,
    case: &CaseConfig,
) -> Result<(), spinduck_core::error::EngineError> {
    let mut files: Vec<PathBuf> = project.ddl_files()?;
    files.extend(project.view_files()?);
    files.extend(project.stub_files()?);
    files.extend(project.routine_files()?);
    if let Some(r) = &case.reference_routine {
        files.push(project.baselines_dir.join(r));
    }
    for f in files {
        for chunk in split_chunks(&std::fs::read_to_string(&f).unwrap_or_default()) {
            og.exec_script(&chunk).map_err(|e| {
                spinduck_core::error::EngineError::env(
                    30,
                    format!("部署 {}: {}", f.display(), e.message),
                )
            })?;
        }
    }
    Ok(())
}

/// 参照侧执行：function → `SELECT`；procedure → `[reference_call_sql]` 或 `CALL`，
/// 输出取 `[case.output].sql`。
fn run_native(og: &mut GaussConn, case: &CaseConfig) -> Vec<Outcome> {
    let mut out = Vec::new();
    for sc in &case.scenarios {
        let args = sc.args.join(", ");
        match case.kind {
            CaseKind::Function => {
                let (_, rows) = og
                    .fetch_rows(&format!("SELECT {}({args})", case.routine))
                    .unwrap_or_else(|e| err("参照侧函数调用", e));
                out.push(Outcome::Scalar(norm(rows[0][0].clone())));
            }
            CaseKind::Procedure => {
                let call = match case_reference_call(case, &args) {
                    Some(sql) => sql,
                    None => format!("CALL {}({args})", case.routine),
                };
                og.exec_script(&call)
                    .unwrap_or_else(|e| err(&format!("参照侧过程调用 {call}"), e));
                out.push(Outcome::Rows(fetch_rows_norm(og, case)));
            }
        }
    }
    out
}

fn case_reference_call(case: &CaseConfig, args: &str) -> Option<String> {
    case.reference_call_sql
        .as_ref()
        .map(|t| t.replace("{args}", args))
}

fn fetch_rows_norm(og: &mut GaussConn, case: &CaseConfig) -> Vec<Vec<String>> {
    let sql = case
        .output_sql
        .clone()
        .expect("procedure case 必有 output.sql");
    let (_, rows) = og
        .fetch_rows(&sql)
        .unwrap_or_else(|e| err("参照侧输出读取", e));
    rows.iter()
        .map(|r| r.iter().map(|c| norm(c.clone())).collect())
        .collect()
}

fn build_engine(project: &ProjectConfig) -> Result<Engine, spinduck_core::error::EngineError> {
    let rules = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )?;
    let rules = rules.with_project_rules(&project.rules_files)?;
    let dex = DuckExecutor::open_in_memory()?;
    // 表 DDL（不含视图：视图含内核专属函数，且仅 %TYPE 锚定用途）
    dex.execute("CREATE SCHEMA IF NOT EXISTS gaussdb", &BindMap::new())?;
    let ddl_funcs = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )?
    .functions;
    for f in project.ddl_files()? {
        let ddl = std::fs::read_to_string(&f).unwrap_or_default();
        execute_table_ddl(&dex, &ddl, &ddl_funcs).map_err(|e| {
            spinduck_core::error::EngineError::kernel(
                70,
                format!("建表 {}: {}", f.display(), e.message),
            )
        })?;
    }
    // A 模式系统单行表在 DuckDB 的等价物
    dex.execute(
        "CREATE TABLE IF NOT EXISTS sys_dummy AS SELECT 1",
        &BindMap::new(),
    )?;
    // 视图：仅占位（%TYPE 声明不参与求值）
    for f in project.view_files()? {
        let name = view_name(&f);
        dex.execute(&format!("CREATE VIEW {name} AS SELECT 1"), &BindMap::new())
            .map_err(|e| {
                spinduck_core::error::EngineError::kernel(
                    71,
                    format!("占位视图 {name}: {}", e.message),
                )
            })?;
    }
    let mut eng = Engine::new(Box::new(dex), rules);
    let mut sources: Vec<PathBuf> = project.stub_files()?;
    sources.extend(project.routine_files()?);
    for f in sources {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        eng.register(&text).map_err(|e| {
            spinduck_core::error::EngineError::env(
                30,
                format!("注册 {}: {}", f.display(), e.message),
            )
        })?;
    }
    Ok(eng)
}

/// 表 DDL 落 DuckDB：先剔除"非字面量 DEFAULT"（如 A 模式 `to_char(sysdate(), …)`，
/// DuckDB 无对应函数；本侧数据由物化提供，缺省值仅影响被测侧自有 INSERT），
/// 再走改写管线（nvl/to_char 等在其它位置仍可翻译）。
fn execute_table_ddl(
    dex: &DuckExecutor,
    ddl: &str,
    funcs: &std::collections::BTreeMap<String, String>,
) -> Result<(), spinduck_core::error::EngineError> {
    let mut pre = String::with_capacity(ddl.len());
    for line in ddl.lines() {
        let up = line.to_ascii_uppercase();
        if up.contains("DEFAULT") && line.contains('(') {
            if let Some(pos) = up.find(" DEFAULT") {
                pre.push_str(line[..pos].trim_end());
                pre.push_str(",\n");
                continue;
            }
        }
        pre.push_str(line);
        pre.push('\n');
    }
    let unit = spinduck_core::sql::rewrite_tokens_with_rules(&pre, &Default::default(), funcs)?;
    let transformed = spinduck_core::sql::apply_ast_transforms(&unit.text)?;
    dex.execute(&transformed, &BindMap::new()).map(|_| ())
}

/// 用表 DDL 同一改写管线建立真实视图（跳过前置 `drop view`）。
fn create_view(dex: &DuckExecutor, f: &Path) -> Result<(), spinduck_core::error::EngineError> {
    let raw = std::fs::read_to_string(f).unwrap_or_default();
    let up = raw.to_ascii_uppercase();
    let Some(idx) = up.find("CREATE OR REPLACE") else {
        return Err(spinduck_core::error::EngineError::kernel(
            71,
            "缺少 CREATE VIEW",
        ));
    };
    let stmt = raw[idx..].trim().trim_end_matches(';');
    let funcs = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )?
    .functions;
    let unit = spinduck_core::sql::rewrite_tokens_with_rules(stmt, &Default::default(), &funcs)?;
    let sql = spinduck_core::sql::apply_ast_transforms(&unit.text)?;
    dex.execute(&sql, &BindMap::new())?;
    Ok(())
}

/// 视图名（`... VIEW <name> ...` 首个标识符）。
fn view_name(f: &Path) -> String {
    let text = std::fs::read_to_string(f).unwrap_or_default();
    let up = text.to_ascii_uppercase();
    let mut from = 0usize;
    while let Some(i) = up[from..].find("VIEW ") {
        let at = from + i + 5;
        let word = text[at..]
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(word.as_str(), "if" | "exists" | "as" | "or" | "replace") {
            return word;
        }
        from = at;
    }
    "v_placeholder".to_string()
}

/// 物化输入表：清单来自 `case.toml [case.data].materialize`（输出表不物化）。
pub fn materialize_inputs(
    og: &mut GaussConn,
    eng: &mut Engine,
    case: &CaseConfig,
) -> Result<(), spinduck_core::error::EngineError> {
    for table in &case.materialize {
        let n = og.materialize(&*eng.ex, table, None)?;
        println!("MAT {table} rows={n}");
    }
    Ok(())
}

fn run_spin(eng: &mut Engine, case: &CaseConfig) -> Vec<Outcome> {
    let mut out = Vec::new();
    for sc in &case.scenarios {
        let args: Vec<SpinValue> = sc.args.iter().map(|a| parse_arg(a)).collect();
        let r = eng
            .call(&case.routine, &args)
            .unwrap_or_else(|e| err(&format!("SpinDuck 调用 {}", case.routine), e.message));
        if let Some(want) = &case.expect_succeed {
            let got = r
                .outs
                .get("p_o_succeed")
                .map(|v| v.as_str_loose())
                .unwrap_or_default();
            assert!(
                want.contains(got.as_str()) || got == *want,
                "p_o_succeed: 期望 {want}, 实际 {got}"
            );
        }
        match case.kind {
            CaseKind::Function => {
                out.push(Outcome::Scalar(norm(
                    r.return_value.map(|v| v.as_str_loose()),
                )));
            }
            CaseKind::Procedure => {
                let sql = case
                    .output_sql
                    .clone()
                    .expect("procedure case 必有 output.sql");
                let mut rs = eng
                    .ex
                    .query(&sql, &BindMap::new())
                    .unwrap_or_else(|e| err("被测侧输出读取", e.message));
                let rows: Vec<Vec<String>> = rs
                    .fetch_many(1000)
                    .iter()
                    .map(|r| r.iter().map(|v| norm(Some(v.as_str_loose()))).collect())
                    .collect();
                out.push(Outcome::Rows(rows));
            }
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Scalar(String),
    Rows(Vec<Vec<String>>),
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::Scalar(s) => format!("scalar={s}"),
            Outcome::Rows(r) => format!("rows={r:?}"),
        }
    }
}

fn load_baseline(project: &ProjectConfig, case: &CaseConfig) -> Vec<Outcome> {
    let p = project.baselines_dir.join(&case.baseline);
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| err(&format!("读取基线 {}", p.display()), e));
    let v: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| err(&format!("解析基线 {}", p.display()), e));
    let arr = v["scenarios"]
        .as_array()
        .unwrap_or_else(|| err("基线缺 scenarios", ""));
    arr.iter()
        .map(|s| match case.kind {
            CaseKind::Function => {
                let val = match s.get("value") {
                    Some(serde_json::Value::String(x)) => x.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    Some(serde_json::Value::Null) | None => "NULL".to_string(),
                    Some(other) => other.to_string(),
                };
                Outcome::Scalar(norm(Some(val)))
            }
            CaseKind::Procedure => {
                let rows = s["rows"]
                    .as_array()
                    .unwrap_or_else(|| err("procedure 基线缺 rows", ""))
                    .iter()
                    .map(|r| {
                        r.as_array()
                            .unwrap()
                            .iter()
                            .map(|c| match c {
                                serde_json::Value::String(x) => norm(Some(x.clone())),
                                serde_json::Value::Null => "NULL".to_string(),
                                other => norm(Some(other.to_string())),
                            })
                            .collect()
                    })
                    .collect();
                Outcome::Rows(rows)
            }
        })
        .collect()
}
