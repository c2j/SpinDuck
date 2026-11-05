//! 黄金对比（语料级，M10 补强）：**逐字取自考题基线**的真实函数
//! `FNC_GET_STOCK_KIND`（5_函数/1_FNC_GET_STOCK_KIND.sql），
//! 在 openGauss 5.0.0 Oracle A 模式容器（dota-gaussdb）原生执行 vs SpinDuck 执行。
//!
//! 被测点：`%TYPE` 声明、`rownum = 1` 首行谓词（[patterns] 规则改写）、
//! 内层 BEGIN..EXCEPTION OTHERS、BETWEEN 区间查找、SELECT INTO 行数语义。
//! 容器部署方法见 lib/fastaas/prototype-1.7/dota-scheduler/dota-adapter/docker。

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
        .join("golden/case_corpus_stock_kind")
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
    Some(GaussConn::connect_parts(&host, port, &user, &password, &dbname).expect("connect gaussdb"))
}

/// 数值文本归一：去尾零、去尾点、补整数部分前导零（两侧 scale 格式化差异
/// 不应造成假失败；GaussDB NUMBER 按声明 scale 渲染 `0.0000`，SpinDuck `0`）。
fn norm(v: Option<String>) -> String {
    let Some(s) = v else {
        return "NULL".to_string();
    };
    let t = s.trim().trim_start_matches('+');
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

fn read(case: &Path, name: &str) -> String {
    std::fs::read_to_string(case.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
}

/// 语料探针组：(stock_code, market, 期望语义说明)
const CASES: [(&str, &str, &str); 4] = [
    ("600000", "83", "沪A股票区间命中"),
    ("000001", "83", "沪A基金区间命中"),
    ("510050", "83", "未配置区间"),
    ("600000", "99", "市场无配置"),
];

#[test]
fn golden_corpus_stock_kind_matches_a_mode_baseline() {
    let case = case_dir();
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
    };
    let routine_text = read(&case, "routine.sql");

    // 1. 参照侧：A 模式 GaussDB（openGauss 内核）原生部署并执行
    og.exec_script(&read(&case, "schema.sql")).unwrap();
    og.exec_script(&read(&case, "seed.sql")).unwrap();
    og.exec_script(&routine_text).unwrap();

    // 2. 被测侧：DuckDB 建表 → 物化种子 → 注册**同一段语料源码** → 调用
    let rules = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )
    .expect("加载种子规则");
    assert!(
        rules.patterns_rownum_first_row,
        "种子规则应开启 rownum 改写"
    );
    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(&read(&case, "schema.sql"), &BindMap::new())
        .unwrap();
    let n = og
        .materialize(&ex, "dic_stock_kind_sub", None)
        .expect("物化 dic_stock_kind_sub");
    assert_eq!(n, 3, "种子 3 行");

    let mut eng = Engine::new(Box::new(ex), rules);
    eng.register(&routine_text).unwrap();

    for (code, market, why) in CASES {
        // 参照侧（A 模式原生）
        let (_, rows) = og
            .fetch_rows(&format!(
                "SELECT FNC_GET_STOCK_KIND('{code}', '20240105', '{market}')"
            ))
            .unwrap();
        assert_eq!(rows.len(), 1, "{why}: 参照侧应返回 1 行");
        let base = rows[0][0].clone().unwrap_or_default();

        // 被测侧（SpinDuck）
        let r = eng
            .call(
                "fnc_get_stock_kind",
                &[
                    SpinValue::Str(code.to_string()),
                    SpinValue::Str("20240105".to_string()),
                    SpinValue::Str(market.to_string()),
                ],
            )
            .unwrap_or_else(|e| panic!("{why}: SpinDuck 调用失败: {e:?}"));
        let got = r.return_value.as_ref().map(|v| v.as_str_loose());

        assert_eq!(
            got.as_deref(),
            Some(base.as_str()),
            "{why}:({code},{market}) SpinDuck={got:?} vs GaussDB={base:?}"
        );
    }
}

/// 语料级黄金第二例：`FNC_COM_GET_MIN_TAX`（费率表最低收费查询，真实表查找）。
/// 被测点：PL 内建 `substr` 下沉、`nvl`→`coalesce` 规则改名、子查询
/// `ORDER BY … DESC` + `rownum = 1`（内层排序语义）、日期 BETWEEN、
/// `DEFAULT NULL` 参数（两侧均按全参数调用，语义等价）、`%TYPE` 带初值声明。
#[test]
fn golden_corpus_min_tax_matches_a_mode_baseline() {
    let case = case_dir().parent().unwrap().join("case_corpus_min_tax");
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
    };
    let routine_text = read(&case, "routine.sql");

    og.exec_script(&read(&case, "schema.sql")).unwrap();
    og.exec_script(&read(&case, "seed.sql")).unwrap();
    og.exec_script(&routine_text).unwrap();

    let rules = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )
    .expect("加载种子规则");
    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(&read(&case, "schema.sql"), &BindMap::new())
        .unwrap();
    let n = og
        .materialize(&ex, "par_sys_marketrate", None)
        .expect("物化 par_sys_marketrate");
    assert_eq!(n, 7, "种子 7 行");

    let mut eng = Engine::new(Box::new(ex), rules);
    eng.register(&routine_text).unwrap();

    let scenarios: [(&str, &str, &str, &str); 4] = [
        (
            "83",
            "P1",
            "0102",
            "精确 stock_kind 命中（2023 过期行必须排除）",
        ),
        ("83", "P2", "0105", "组内兜底 0100"),
        ("83", "P3", "0200", "全局兜底 0000"),
        ("83", "P4", "0900", "无配置 → EXCEPTION OTHERS + nvl 归 0"),
    ];
    for (market, par, kind, why) in scenarios {
        let (_, rows) = og
            .fetch_rows(&format!(
                "SELECT FNC_COM_GET_MIN_TAX('{market}','{par}','{kind}','20240630',NULL,NULL)"
            ))
            .unwrap();
        assert_eq!(rows.len(), 1, "{why}");
        let base = norm(rows[0][0].clone());

        let r = eng
            .call(
                "fnc_com_get_min_tax",
                &[
                    SpinValue::Str(market.to_string()),
                    SpinValue::Str(par.to_string()),
                    SpinValue::Str(kind.to_string()),
                    SpinValue::Str("20240630".to_string()),
                    SpinValue::Null,
                    SpinValue::Null,
                ],
            )
            .unwrap_or_else(|e| panic!("{why}: SpinDuck 调用失败: {e:?}"));
        let got = r.return_value.map(|v| v.as_str_loose());

        let got_norm = got.as_ref().map(|g| norm(Some(g.clone())));
        assert_eq!(
            got_norm,
            Some(base.clone()),
            "{why}:({market},{par},{kind}) SpinDuck={got:?} vs GaussDB={base:?}"
        );
    }
}
