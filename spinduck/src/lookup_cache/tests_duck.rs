//! 用真实 DuckDB 核对日期区间、coalesce 和同连接写入。假执行器看不到这三件事。

use super::tests::{cache, purchase_binds, strs, PURCHASE};
use super::*;
use crate::sql::DuckExecutor;

fn as_set(rows: &[Row]) -> Vec<String> {
    let mut out = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn exec(duck: &DuckExecutor, sql: &str) {
    duck.execute(sql, &BindMap::new()).unwrap();
}

#[test]
fn should_let_duckdb_apply_purchase_date_range_and_coalesce() {
    let duck = DuckExecutor::open_in_memory().unwrap();
    exec(
        &duck,
        "CREATE TABLE par_sys_purchase(security_id VARCHAR, purchase_days INTEGER, \
         inure_begin_date VARCHAR, inure_end_date VARCHAR)",
    );
    exec(
        &duck,
        "INSERT INTO par_sys_purchase VALUES \
         ('S1', 7, '20260101', '20260131'), \
         ('S1', NULL, '20260101', '20260131'), \
         ('OUT', 9, '20250101', '20250102')",
    );
    let truth = duck
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(4);
    let ex = cache(Box::new(duck));
    let got = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(4);
    assert_eq!(as_set(&got), as_set(&truth));
    let outside = ex
        .query(PURCHASE, &purchase_binds("20260105", "OUT"))
        .unwrap()
        .fetch_many(2);
    assert!(outside.is_empty(), "区间外的证券不得命中");
    ex.execute(
        "INSERT INTO par_sys_purchase VALUES ('S3', 4, '20260101', '20260131')",
        &BindMap::new(),
    )
    .unwrap();
    let added = ex
        .query(PURCHASE, &purchase_binds("20260105", "S3"))
        .unwrap()
        .fetch_many(2);
    assert_eq!(added.len(), 1);
    let still = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(4);
    assert_eq!(as_set(&still), as_set(&truth));
}

#[test]
fn should_let_duckdb_apply_partner_seat_date_ranges() {
    let duck = DuckExecutor::open_in_memory().unwrap();
    exec(
        &duck,
        "CREATE TABLE par_fund_partner(partner_code VARCHAR, sub_partner_code VARCHAR, \
         market_code VARCHAR, fund_code VARCHAR, inure_begin_date VARCHAR, inure_end_date VARCHAR)",
    );
    exec(
        &duck,
        "CREATE TABLE par_fund_seat(seat_code VARCHAR, fund_code VARCHAR, \
         inure_begin_date VARCHAR, inure_end_date VARCHAR)",
    );
    exec(
        &duck,
        "INSERT INTO par_fund_partner VALUES \
         ('A0001', 'S1', '001', 'F001', '20260101', '20260131'), \
         ('A0001', 'S1', '001', 'F009', '20200101', '20200102')",
    );
    exec(
        &duck,
        "INSERT INTO par_fund_seat VALUES \
         ('X001', 'F001', '20200101', '20200102'), \
         ('X001', 'F009', '20260101', '20260131')",
    );
    let sql = "SELECT t.fund_code FROM par_fund_partner AS t WHERE $v_p_i_date BETWEEN \
        t.inure_begin_date AND t.inure_end_date AND t.partner_code = $v_gddm AND \
        t.sub_partner_code = $v_sub AND t.market_code = $v_mkt AND t.fund_code IN \
        (SELECT p.fund_code FROM par_fund_seat AS p WHERE p.seat_code = $v_seat AND \
        $v_p_i_date BETWEEN p.inure_begin_date AND p.inure_end_date)";
    let binds = strs(&[
        ("$v_p_i_date", "20260105"),
        ("$v_gddm", "A0001"),
        ("$v_sub", "S1"),
        ("$v_mkt", "001"),
        ("$v_seat", "X001"),
    ]);
    let truth = duck.query(sql, &binds).unwrap().fetch_many(4);
    let got = cache(Box::new(duck))
        .query(sql, &binds)
        .unwrap()
        .fetch_many(4);
    assert!(truth.is_empty(), "两侧日期都对不上时半连接应为空");
    assert_eq!(as_set(&got), as_set(&truth));
}
