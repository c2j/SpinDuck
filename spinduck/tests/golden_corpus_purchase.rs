//! 黄金对比（语料级·重量级）：考题核心过程 **pkg_split_trade_step3_sh.
//! prc_trd_deal_purchase**（回购/申购业务拆分，生产调用链第 4 步）。
//!
//! 源码逐字取自考题基线（`golden/case_corpus_purchase/src/`），两侧在同一份
//! 种子数据上执行并逐行比对 `dat_trd_repurchase`：
//!
//! 被测点：%ROWTYPE 声明 + 显式 FETCH INTO 整行记录、记录字段直接下发 SQL
//! （BindSrc::Field）、游标循环 + `<<error_row>>`/GOTO、嵌套 EXCEPTION OTHERS、
//! 过程内 COMMIT（延迟事务）、`to_char(to_date(…)+n)` 日期算术（SqlFn 下沉）、
//! INTERVALTONUM/to_date 解析级变换、PL 内建 substr 下沉、依赖函数链
//! （FNC_GET_PURCHASE_JS_DAYS → fnc_com_getday / fnc_com_if_deal_date）、
//! 外部日志桩包（inst_log）、异常路径（股东未维护 → 跳过该行）。
//!
//! 容器：golden/gaussdb-docker（Oracle A 模式）；未设 SPINDUCK_OG_HOST 打印 SKIP。

#![cfg(feature = "gauss")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::io::GaussConn;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use std::path::{Path, PathBuf};

const CASE: &str = "case_corpus_purchase";
const OUT_COLS: &str = "fund_code, security_id, partner_code, seat_code, purchase_no, purchase_fee, purchase_yj, purchase_money, purchase_quantity, purchase_date, buy_sale, repurchase_date, repurchase_money, repurchase_money_js, accrual_daily, accrual_daily_js, purchase_days, purchase_days_js, purchase_kind, gddmzm";

fn case_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SPINDUCK_GOLDEN_DIR") {
        return PathBuf::from(p).join(CASE);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .join(format!("golden/{CASE}"))
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

fn read(case: &Path, rel: &str) -> String {
    std::fs::read_to_string(case.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// 数值文本归一（与其它 golden 用例同口径）。
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

/// Oracle 风格 `/` 分隔符拆块 + 序列 MAXVALUE 越界截断（V6 已知项）。
fn split_chunks(text: &str) -> Vec<String> {
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

const SRC_FILES: [&str; 5] = [
    "STUB_pkg_split_trade_log.sql",
    "0_SIMPLIFIED_fnc_com_if_deal_date.sql",
    "0_SIMPLIFIED_fnc_com_getday.sql",
    "2_FNC_GET_PURCHASE_JS_DAYS.sql",
    "PKG_SPLIT_TRADE_STEP3_SH.sql",
];

const INPUT_TABLES: [&str; 17] = [
    "mid_yjqs_detail",
    "par_fund_partner",
    "par_fund_seat",
    "par_sys_purchase",
    "par_fund_seat_para",
    "swh_all_kind",
    "db_log",
    "dat_fund_cjqs",
    "par_fund_info",
    "dat_xw_venture",
    "par_fund_etf",
    "par_sys_bond",
    "par_sys_securities",
    "qfqscjhbrbb_temp",
    "par_fund_check_para",
    "par_fund_autorun_param",
    "par_drv_config",
];

#[test]
fn golden_corpus_purchase_matches_a_mode_baseline() {
    // cargo test 线程 2MiB 栈不够重量级语料的解析/编译递归
    let h = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(golden_impl)
        .unwrap();
    h.join().unwrap();
}

fn golden_impl() {
    let case = case_dir();
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
    };

    // 1. 参照侧：A 模式容器全新部署（应用 schema 整体重建保证幂等）→ 种子 → CALL
    og.exec_script("DROP SCHEMA IF EXISTS gaussdb CASCADE;")
        .unwrap();
    og.exec_script("CREATE SCHEMA gaussdb;").unwrap();
    og.exec_script(&read(&case, "schema.sql")).unwrap();
    og.exec_script(&read(&case, "seed.sql")).unwrap();
    for f in SRC_FILES {
        for chunk in split_chunks(&read(&case, &format!("src/{f}"))) {
            og.exec_script(&chunk)
                .unwrap_or_else(|e| panic!("部署 {f}: {}", e.message));
        }
    }
    og.exec_script("CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase('20250715', NULL);")
        .expect("参照侧 CALL 失败");
    let (_, base_rows) = og
        .fetch_rows(&format!(
            "SELECT {OUT_COLS} FROM dat_trd_repurchase ORDER BY security_id"
        ))
        .unwrap();
    let base: Vec<Vec<String>> = base_rows
        .iter()
        .map(|r| r.iter().map(|c| norm(c.clone())).collect())
        .collect();
    assert!(!base.is_empty(), "参照侧应产出（种子含 2 笔有效申购）");

    // 2. 被测侧：DuckDB 建表（考题 DDL）+ sys_dummy 单行表 → 物化输入 →
    //    注册同一批源码 → CALL → 读回
    let rules = RuleRegistry::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )
    .expect("加载种子规则");
    let dex = DuckExecutor::open_in_memory().unwrap();
    // 被测侧只需表 DDL：视图块仅用于容器 %TYPE 锚定，且引用 nvl（DuckDB 无）
    let schema = read(&case, "schema.sql");
    let tables_only = match schema.find("3_视图/1_V_PAR_FUND_CHECK_PARA.sql") {
        Some(i) => schema[..i].to_string(),
        None => schema.clone(),
    };
    dex.execute(&tables_only, &BindMap::new())
        .expect("被测侧建表");
    dex.execute("CREATE TABLE sys_dummy AS SELECT 1", &BindMap::new())
        .unwrap();
    // %TYPE 锚定视图（A 模式系统形态在容器侧部署；DuckDB 侧只需存在即可，
    // 类型声明不参与求值）：DuckDB 视图列兼容性有限，用单列占位视图
    dex.execute(
        "CREATE VIEW v_par_fund_check_para AS SELECT 1 AS venture_type",
        &BindMap::new(),
    )
    .unwrap();

    let mut eng = Engine::new(Box::new(dex), rules);
    for f in SRC_FILES {
        eng.register(&read(&case, &format!("src/{f}")))
            .unwrap_or_else(|e| panic!("注册 {f}: {}", e.message));
    }
    for t in INPUT_TABLES {
        let n = og
            .materialize(&*eng.ex, t, None)
            .unwrap_or_else(|e| panic!("物化 {t}: {}", e.message));
        println!("MAT {t} rows={n}");
    }

    let r = eng
        .call(
            "pkg_split_trade_step3_sh.prc_trd_deal_purchase",
            &[SpinValue::Str("20250715".into()), SpinValue::Null],
        )
        .unwrap_or_else(|e| panic!("SpinDuck CALL 失败: {}", e.message));
    let succeed = r
        .outs
        .get("p_o_succeed")
        .map(|v| v.as_str_loose())
        .unwrap_or_default();
    assert_eq!(succeed, "0", "p_o_succeed 应为 '0'（无业务失败）");

    let mut rs = eng
        .ex
        .query(
            &format!("SELECT {OUT_COLS} FROM dat_trd_repurchase ORDER BY security_id"),
            &BindMap::new(),
        )
        .unwrap();
    let duck_rows = rs.fetch_many(100);
    let got: Vec<Vec<String>> = duck_rows
        .iter()
        .map(|r| r.iter().map(|v| norm(Some(v.as_str_loose()))).collect())
        .collect();

    assert_eq!(got, base, "SpinDuck 与 A 模式基线应逐行逐字段一致");

    // 3. expected.json 漂移检查
    let expected: serde_json::Value = serde_json::from_str(&read(&case, "expected.json")).unwrap();
    let cols = [
        "fund_code",
        "security_id",
        "partner_code",
        "seat_code",
        "purchase_no",
        "purchase_fee",
        "purchase_yj",
        "purchase_money",
        "purchase_quantity",
        "purchase_date",
        "buy_sale",
        "repurchase_date",
        "repurchase_money",
        "repurchase_money_js",
        "accrual_daily",
        "accrual_daily_js",
        "purchase_days",
        "purchase_days_js",
        "purchase_kind",
        "gddmzm",
    ];
    let want: Vec<Vec<String>> = expected["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            cols.iter()
                .map(|k| norm(row[*k].as_str().map(String::from)))
                .collect()
        })
        .collect();
    assert_eq!(
        want, base,
        "expected.json 与容器实时基线不一致，需查明后重新生成"
    );
}
