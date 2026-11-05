//! SQL 上下文下沉单测（自 `sqlfn.rs` 拆分，AGENTS §3；断言一字不改）。

use crate::sql::sqlfn::plan_sites;
use crate::sql::VarScope;
use std::collections::{BTreeMap, BTreeSet};

fn names(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn should_inline_update_call_as_parameter_cache_subquery() {
    let sql = "UPDATE mid_yjqs_detail t SET t.fund_code = fnc_com_get_fund_code(t.gddm, t.sub_gddm, p_i_date, t.xwdm) WHERE t.bcrq = p_i_date AND scdm = '001'";
    let vars: VarScope = ["p_i_date".to_string()].into_iter().collect();
    let (out, sites) = plan_sites(
        sql,
        &vars,
        &BTreeMap::new(),
        &names(&["fnc_com_get_fund_code"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].name, "fnc_com_get_fund_code");
    assert_eq!(sites[0].table, "sd_fn0");
    assert_eq!(
        sites[0].distinct_sql,
        "SELECT DISTINCT t.gddm, t.sub_gddm, $v_p_i_date, t.xwdm FROM mid_yjqs_detail t WHERE t.bcrq = $v_p_i_date AND scdm = '001'"
    );
    assert!(
        out.contains(
            "SET t.fund_code = (SELECT v FROM sd_fn0 WHERE a0 IS NOT DISTINCT FROM (t.gddm)"
        ),
        "{out}"
    );
    assert!(!out.contains("fnc_com_get_fund_code("));
}

#[test]
fn should_strip_own_predicate_from_distinct_context() {
    // WHERE 中调用的例程：参数元组查询不得包含该谓词（否则自引用）
    let sql = "SELECT a.gddm FROM mid_yjqs_detail a WHERE a.bcrq = '20240105' AND (SELECT fnc_get_fund_data_mode(a.bcrq, a.gddm, a.sub_gddm, a.xwdm)) = '1' AND a.cjsl > 0";
    let (out, sites) = plan_sites(
        sql,
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["fnc_get_fund_data_mode"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(
        sites[0].distinct_sql,
        "SELECT DISTINCT a.bcrq, a.gddm, a.sub_gddm, a.xwdm FROM mid_yjqs_detail a WHERE a.bcrq = '20240105' AND a.cjsl > 0"
    );
    assert!(out.contains("(SELECT v FROM sd_fn0"));
}

#[test]
fn should_dedupe_identical_calls_and_inline_each_occurrence() {
    let sql = "SELECT fnc_x(a), fnc_x(a) FROM t GROUP BY fnc_x(a)";
    let (out, sites) =
        plan_sites(sql, &VarScope::new(), &BTreeMap::new(), &names(&["fnc_x"])).unwrap();
    assert_eq!(sites.len(), 1, "同 name+实参只物化一次");
    assert_eq!(out.matches("(SELECT v FROM sd_fn0").count(), 3);
}

#[test]
fn should_plan_insert_values_as_single_row_context() {
    let sql = "INSERT INTO dat_fund_cjqs (a, b) VALUES (1, fnc_get_rate_info2(f, d, '3', k))";
    let (out, sites) = plan_sites(
        sql,
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["fnc_get_rate_info2"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].distinct_sql, "SELECT f, d, '3', k");
    assert!(out.contains("(SELECT v FROM sd_fn0"));
}

#[test]
fn should_plan_call_nested_in_derived_table_with_inner_context() {
    // 语料 prc_trd_hz_byfund 动态 SQL 形态：调用在两层派生表的 SELECT 列表，
    // 且同名调用亦出现在 GROUP BY（去重）；参数元组必须取自**内层** FROM/WHERE，
    // 不能把调用本身带进元组查询（自引用）。
    let sql = "SELECT SUM(FXJ), ROUND(NVL(SUM(fxj2), 0), 2), FUND_CODE, XWDM, TRADE_TYPE, VENTURE_TYPE FROM (SELECT SUM(NVL(ROUND(fxj, 2), 0)) FXJ, NVL(SUM(fxj), 0) FXJ2, fund_code, bs, xwdm, trade_type, venture_type FROM (SELECT zqdm, fund_code, bs, xwdm, trade_type, SUM(NVL(fxj, 0)) FXJ, fnc_com_get_seat_fee_type(BCRQ, SCDM, FUND_CODE, XWDM, STOCK_KIND) VENTURE_TYPE FROM mid_yjqs_detail WHERE SCDM = '001' AND BCRQ = '20250715' AND (substr(stock_kind, 1, 2) = '01') AND NOT (is_yysg = '1' AND ywlb IN ('ES', '77')) GROUP BY zqdm, bs, fund_code, xwdm, trade_type, fnc_com_get_seat_fee_type(BCRQ, SCDM, FUND_CODE, XWDM, STOCK_KIND)) GROUP BY fund_code, bs, XWDM, TRADE_TYPE, venture_type) GROUP BY fund_code, XWDM, TRADE_TYPE, venture_type";
    let (out, sites) = plan_sites(
        sql,
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["fnc_com_get_seat_fee_type"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 1, "同名调用去重");
    assert!(
        !sites[0]
            .distinct_sql
            .to_ascii_lowercase()
            .contains("fnc_com_get_seat_fee_type("),
        "元组查询不得自引用：{}",
        sites[0].distinct_sql
    );
    assert!(
        sites[0].distinct_sql.contains("FROM mid_yjqs_detail"),
        "上下文应取内层 FROM：{}",
        sites[0].distinct_sql
    );
    assert_eq!(out.matches("(SELECT v FROM sd_fn0").count(), 2);
}

#[test]
fn should_reject_nested_routine_call_in_arguments() {
    let e = plan_sites(
        "SELECT fnc_a(fnc_b(x)) FROM t",
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["fnc_a", "fnc_b"]),
    )
    .unwrap_err();
    assert_eq!(e.code.to_string(), "BIZ-0158");
}

#[test]
fn should_reject_unsupported_statement_shape() {
    let e = plan_sites(
        "MERGE INTO t USING s ON (1=1) WHEN MATCHED THEN UPDATE SET a = fnc_x(b)",
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["fnc_x"]),
    )
    .unwrap_err();
    assert_eq!(e.code.to_string(), "BIZ-0159");
}
