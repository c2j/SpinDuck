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
use spinduck_core::verify::{
    clamp_sequence_bounds, deploy_routines, deploy_schema, drop_cache_option,
};
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

/// Oracle 风格 `/` 分隔符拆块：参照侧逐字下发语料文本。
///
/// 语料一个字不改；只有序列 DDL 的两处形态在**下发时**裁剪（见 `deploy_native`），
/// 因为超 BIGINT 的边界与 `CACHE` 选项连 A 模式容器也拒绝——被测侧走
/// `spinduck_core::verify::deploy_schema` 的同名函数，两侧口径一致。
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

/// 项目内全部用例逐一比对（`*.case.toml` 按 `[cases].include` 顺序）。
///
/// 每个用例独立成轮：参照侧 schema 整体重建、被测侧引擎全新构建——一个项目声明
/// 多条 `[[entry]]`（考题式接入）时，用例之间的写副作用互不影响。
/// 无用例定义视为失败：黄金层不得静默返回。
fn run_case_inner(name: &str) {
    let dir = golden_root().join(name);
    let project = ProjectConfig::load(&dir).unwrap_or_else(|e| err("加载 spinduck.toml", e));
    let case_files = project.case_files().unwrap_or_else(|e| err("枚举 case", e));
    assert!(
        !case_files.is_empty(),
        "[{name}] 无 *.case.toml：黄金层没有可执行用例（跳过不等于通过）"
    );
    let Some(mut og) = og() else {
        // §4.1：黄金层禁止"跳过即绿"——缺容器必须让测试显式变红，
        // 否则 V13–V15 结论可被无容器环境静默"复验"
        panic!(
            "黄金用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };
    for f in &case_files {
        let case = CaseConfig::load(f).unwrap_or_else(|e| err("加载 case.toml", e));
        run_one(&project, &case, name, &mut og);
    }
}

/// 一个用例的一轮：参照侧（重建→部署→种子→执行）= 被测侧（构建→物化→执行）= 基线。
fn run_one(project: &ProjectConfig, case: &CaseConfig, name: &str, og: &mut GaussConn) {
    reset_reference(project, case, og);
    let native = run_native(og, case);
    let mut eng = build_engine(project).unwrap_or_else(|e| err("被测侧构建", e));
    materialize_inputs(og, &mut eng, case).unwrap_or_else(|e| err("物化输入表", e));
    let spin = run_spin(&mut eng, case);
    // 输出表行数一致性（防"输出表未物化"导致的漏比）
    check_row_counts(name, case, og, &mut eng);
    let baseline = load_baseline(project, case);
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
            compare_extra(name, &why, extra, og, &mut eng);
            println!("OK [{name}] {why} [extra]");
        }
        println!("OK [{name}] {why}: {}", spin[i].describe());
    }
}

/// 参照侧一轮的起点：A 模式容器整体重建 → 语料部署 → 数据集种子。
fn reset_reference(project: &ProjectConfig, case: &CaseConfig, og: &mut GaussConn) {
    og.exec_script("DROP SCHEMA IF EXISTS gaussdb CASCADE;")
        .unwrap();
    og.exec_script("CREATE SCHEMA gaussdb;").unwrap();
    deploy_native(og, project, case).unwrap_or_else(|e| err("参照侧部署", e.message));
    for f in project
        .dataset_files()
        .unwrap_or_else(|e| err("枚举数据集", e))
    {
        og.exec_script(&std::fs::read_to_string(&f).unwrap())
            .unwrap_or_else(|e| err(&format!("种子 {}", f.display()), e));
    }
}

/// 输出表行数：被测侧必须与参照侧同数量（两侧各自写入，不经物化）。
fn check_row_counts(name: &str, case: &CaseConfig, og: &mut GaussConn, eng: &mut Engine) {
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
}

/// 附加输出（`[case] extra_sql`）：同一 SELECT 在两侧逐行比对。
fn compare_extra(name: &str, why: &str, extra: &str, og: &mut GaussConn, eng: &mut Engine) {
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
            let chunk = drop_cache_option(&clamp_sequence_bounds(&chunk));
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

/// 被测侧构建：与 `spinduck verify` 走同一部署实现（引擎侧唯一口径）。
fn build_engine(project: &ProjectConfig) -> Result<Engine, spinduck_core::error::EngineError> {
    let rules = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )?;
    let rules = rules.with_project_rules(&project.rules_files)?;
    let dex = DuckExecutor::open_in_memory()?;
    // 参照侧对象在 gaussdb schema 下；被测侧的语料名不限定，保留 schema 供限定引用
    dex.execute("CREATE SCHEMA IF NOT EXISTS gaussdb", &BindMap::new())?;
    let mut eng = Engine::new(Box::new(dex), rules);
    deploy_schema(&mut eng, project)?;
    deploy_routines(&mut eng, project)?;
    Ok(eng)
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
