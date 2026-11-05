//! 黄金对比（M10）：同一夹具分别由 openGauss 5.0.0 与 SpinDuck 执行，逐行比对。
//!
//! 基线来源：GaussDB 实例不可达（连接被拒绝，见 validation.md V10），改用同内核
//! 血统的 openGauss 5.0.0 作为参照实现；用例数据与例程均为本仓库夹具，不依赖
//! 外部库。未设置 `SPINDUCK_OG_HOST` 时跳过（不静默通过：打印 SKIP 原因）。

#![cfg(feature = "gauss")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::io::GaussConn;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use std::path::{Path, PathBuf};

fn case_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SPINDUCK_GOLDEN_DIR") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .join("golden/case_split_agg")
}

fn og() -> Option<GaussConn> {
    let host = std::env::var("SPINDUCK_OG_HOST").ok()?;
    let port: u16 = std::env::var("SPINDUCK_OG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let user = std::env::var("SPINDUCK_OG_USER").unwrap_or_else(|_| "gaussdb".into());
    let password = std::env::var("SPINDUCK_OG_PASSWORD").expect("SPINDUCK_OG_PASSWORD");
    let dbname = std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".into());
    Some(
        GaussConn::connect_parts(&host, port, &user, &password, &dbname)
            .expect("connect openGauss"),
    )
}

fn read(case: &Path, name: &str) -> String {
    std::fs::read_to_string(case.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
}

/// 数值文本归一：去尾零、去尾点、去正号（两侧格式化差异不应造成假失败）。
fn norm(v: Option<String>) -> String {
    let Some(s) = v else {
        return "NULL".to_string();
    };
    let t = s.trim().trim_start_matches('+');
    // openGauss 侧 NUMERIC 可能渲染为 ".81"（无整数部分），统一补 0
    let t = if let Some(rest) = t.strip_prefix('.') {
        format!("0.{rest}")
    } else if let Some(rest) = t.strip_prefix("-.") {
        format!("-0.{rest}")
    } else {
        t.to_string()
    };
    let t = t.as_str();
    if t.contains('.') {
        let t2 = t.trim_end_matches('0').trim_end_matches('.');
        if t2.is_empty() || t2 == "-" {
            "0".to_string()
        } else {
            t2.to_string()
        }
    } else {
        t.to_string()
    }
}

#[test]
fn golden_case_split_agg_matches_opengauss_baseline() {
    let case = case_dir();
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
    };

    // 1. 参照侧：openGauss 建表/种子/例程 → 调用
    og.exec_script(&read(&case, "schema.sql")).unwrap();
    og.exec_script(&read(&case, "seed.sql")).unwrap();
    og.exec_script(&read(&case, "routine_gauss.sql")).unwrap();
    og.exec_script("SELECT fn_agg_fund('20240105')").unwrap();
    let (_, base_rows) = og
        .fetch_rows(
            "SELECT bcrq, fund_code, scdm, cjsl, cjje, fee \
             FROM dat_inst_fund_cjqs ORDER BY fund_code, scdm",
        )
        .unwrap();
    let base: Vec<Vec<String>> = base_rows
        .iter()
        .map(|r| r.iter().map(|c| norm(c.clone())).collect())
        .collect();
    assert!(!base.is_empty(), "参照侧应有产出");

    // 2. 被测侧：DuckDB 建表 → 物化种子 → 注册 Oracle 形态例程 → CALL
    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(&read(&case, "schema.sql"), &BindMap::new())
        .unwrap();
    let n = og.materialize(&ex, "mid_yjqs_detail", None).unwrap();
    assert_eq!(n, 4, "种子应物化 4 行");
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(&read(&case, "routine.sql")).unwrap();
    eng.call("prc_agg_fund", &[SpinValue::Str("20240105".to_string())])
        .unwrap();

    let mut s = eng
        .ex
        .query(
            "SELECT bcrq, fund_code, scdm, cjsl, cjje, fee \
             FROM dat_inst_fund_cjqs ORDER BY fund_code, scdm",
            &BindMap::new(),
        )
        .unwrap();
    let rows = s.fetch_many(100);
    let got: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| norm(Some(v.as_str_loose().to_string())))
                .collect()
        })
        .collect();

    assert_eq!(got, base, "SpinDuck 产出应与 openGauss 基线逐行一致");

    // 3. 提交的期望基线（expected.json）也必须一致（防基线漂移）
    let expected = read(&case, "expected.json");
    let v: serde_json::Value = serde_json::from_str(&expected).unwrap();
    let exp_rows = v["result"].as_array().unwrap();
    let want: Vec<Vec<String>> = exp_rows
        .iter()
        .map(|r| {
            ["bcrq", "fund_code", "scdm", "cjsl", "cjje", "fee"]
                .iter()
                .map(|k| norm(r[*k].as_str().map(|s| s.to_string())))
                .collect()
        })
        .collect();
    assert_eq!(
        want, base,
        "expected.json 与 openGauss 实时基线不一致，需重新生成"
    );
}
