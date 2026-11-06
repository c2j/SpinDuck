//! golden 用例的原生断言层（`[[case.assert]]`）：进程内内核，零外部环境（AGENTS §4.1）。
//!
//! 口径（2026-11-06 决策）：语料级用例不能只在 A 模式容器里比基线——每个
//! `golden/case_*` 项目在 `cases/*.case.toml` 里显式声明断言集，四个维度、逐值比对：
//! `call`（返回值）/ `out`（OUT 参）/ `rows`（表行集）/ `error`（错误契约）。
//! 部署口径与 `verify` 完全一致（`deploy_project`：表 DDL → 视图 → 桩 → 例程 → 种子），
//! 场景调用（可执行性）与断言（可观察结果）在同一份资产上跑。
//!
//! 项目里一条断言都没有 = 显式变红（"跳过即绿" 属 AGENTS §2 Never）。
//! 取实测值（新增断言用，不参与门禁语义）：
//! `CASES_PROBE=1 cargo test --features native --test cases_native -- --nocapture`

#![cfg(feature = "native")]

#[path = "corpus_forms/mod.rs"]
mod forms;

use forms::{fmt_rows, lit_arg, norm, norm_scalar};
use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::project::{AssertKind, CaseAssert, CaseConfig, ProjectConfig};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use spinduck_core::verify::deploy_project;
use std::path::{Path, PathBuf};

#[test]
fn native_asserts_split_agg() {
    run_project("case_split_agg");
}

#[test]
fn native_asserts_corpus_stock_kind() {
    run_project("case_corpus_stock_kind");
}

#[test]
fn native_asserts_corpus_min_tax() {
    run_project("case_corpus_min_tax");
}

#[test]
fn native_asserts_corpus_purchase() {
    run_project("case_corpus_purchase");
}

fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .join("golden")
}

/// 探针模式：打印实测文本而不判红，用于新增断言时取基线值。
fn probe_mode() -> bool {
    std::env::var("CASES_PROBE").is_ok()
}

/// 一个项目跑完：部署 → 每个用例的场景调用 + 断言集 → 一次性汇总失败。
fn run_project(name: &str) {
    let root = golden_root().join(name);
    let project = ProjectConfig::load(&root)
        .unwrap_or_else(|e| panic!("[{name}] 项目加载失败: {}", e.message));
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap_or_else(|e| panic!("[{name}] 规则加载失败: {}", e.message));
    let ex = DuckExecutor::open_in_memory().expect("open duckdb");
    let mut eng = Engine::new(Box::new(ex), rules);
    deploy_project(&mut eng, &project)
        .unwrap_or_else(|e| panic!("[{name}] 部署失败 [{}]: {}", e.code, e.message));

    let mut bad: Vec<String> = Vec::new();
    let mut n_asserts = 0usize;
    for f in project
        .case_files()
        .unwrap_or_else(|e| panic!("[{name}] 用例发现失败: {}", e.message))
    {
        let case = CaseConfig::load(&f).unwrap_or_else(|e| panic!("读 {}: {e}", f.display()));
        run_scenarios(&mut eng, name, &case, &mut bad);
        for a in &case.asserts {
            n_asserts += 1;
            check_assert(&mut eng, name, a, &mut bad);
        }
    }
    assert!(
        n_asserts > 0,
        "[{name}]: cases/*.case.toml 里没有任何 [[case.assert]]——原生层不接受空覆盖"
    );
    assert!(
        bad.is_empty(),
        "[{name}] 原生断言失败（{} 条）：\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// 场景调用维度：每个 `[[case.scenarios]]` 必须可调用（结果由断言集锁定）。
fn run_scenarios(eng: &mut Engine, project: &str, case: &CaseConfig, bad: &mut Vec<String>) {
    for (i, sc) in case.scenarios.iter().enumerate() {
        let args: Vec<SpinValue> = sc.args.iter().map(|a| lit_arg(a)).collect();
        let id = format!("{project}/{}/{}", case.name, i + 1);
        if probe_mode() {
            println!(
                "PROBE {id} 场景 = {:?}",
                eng.call(&case.routine, &args).is_ok()
            );
            continue;
        }
        if let Err(e) = eng.call(&case.routine, &args) {
            bad.push(format!(
                "[{id}] 场景调用失败 [{}]: {}（why: {}）",
                e.code, e.message, sc.why
            ));
        }
    }
}

/// 一条断言：与 `want` 逐字比对（`error` 形态按 码 + 消息片段 比对）。
fn check_assert(eng: &mut Engine, project: &str, a: &CaseAssert, bad: &mut Vec<String>) {
    let why = a.why.clone();
    let id = format!("{project}/[{}] {why}", kind_name(a.kind));
    let actual = measure(eng, a);
    if probe_mode() {
        println!(
            "PROBE {id} = {:?}",
            match &actual {
                Ok(t) => t.clone(),
                Err(m) => format!("!FAIL {m}"),
            }
        );
        return;
    }
    let actual = match actual {
        Ok(t) => t,
        Err(m) => {
            bad.push(format!("[{id}] {m}"));
            return;
        }
    };
    let held = match a.kind {
        AssertKind::Error => {
            let (code, frag) = split_want(&a.want);
            let (got_code, got_msg) = split_want(&actual);
            got_code == code && got_msg.contains(frag)
        }
        _ => actual == a.want,
    };
    if !held {
        bad.push(format!(
            "[{id}] 期望 {want}\n      实际 {actual}",
            want = a.want
        ));
    }
}

/// 断言实测文本；执行本身失败（内核报错/例程未登记）以 `Err` 返回原因。
fn measure(eng: &mut Engine, a: &CaseAssert) -> Result<String, String> {
    let args: Vec<SpinValue> = a.args.iter().map(|s| lit_arg(s)).collect();
    match a.kind {
        AssertKind::Rows => {
            let mut rs = eng
                .ex
                .query(&a.target, &BindMap::new())
                .map_err(|e| format!("断言 SQL 执行失败 [{}]: {}", e.code, e.message))?;
            let rows: Vec<Vec<String>> = rs
                .fetch_many(1000)
                .iter()
                .map(|r| r.iter().map(cell).collect())
                .collect();
            Ok(fmt_rows(&rows))
        }
        AssertKind::Call => eng
            .call(&a.target, &args)
            .map(|r| norm_scalar(r.return_value))
            .map_err(|e| format!("调用 {} 失败 [{}]: {}", a.target, e.code, e.message)),
        AssertKind::Out => {
            let r = eng
                .call(&a.target, &args)
                .map_err(|e| format!("调用 {} 失败 [{}]: {}", a.target, e.code, e.message))?;
            r.outs
                .get(&a.param)
                .map(|v| norm(Some(v.as_str_loose())))
                .ok_or_else(|| format!("{} 无 OUT 参 '{}'", a.target, a.param))
        }
        AssertKind::Error => match eng.call(&a.target, &args) {
            Err(e) => Ok(format!("{}|{}", e.code, e.message)),
            Ok(r) => Err(format!(
                "期望 {} 失败，实际成功（return={:?}）",
                a.target,
                r.return_value.map(|v| v.display().to_string())
            )),
        },
    }
}

/// 单元格归一：与 golden/corpus 形态层同口径（忽略 scale、尾零，NULL 显式化）。
fn cell(v: &SpinValue) -> String {
    match v {
        SpinValue::Null => "NULL".to_string(),
        other => norm(Some(other.as_str_loose())),
    }
}

/// `want` 与实测文本的 `CODE|消息` 拆分（`|` 之后省略 = 只比错误码）。
fn split_want(text: &str) -> (&str, &str) {
    text.split_once('|').unwrap_or((text, ""))
}

fn kind_name(k: AssertKind) -> &'static str {
    match k {
        AssertKind::Rows => "rows",
        AssertKind::Call => "call",
        AssertKind::Out => "out",
        AssertKind::Error => "error",
    }
}
