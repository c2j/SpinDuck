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
            "SET t.fund_code = (SELECT sd_v FROM sd_fn0 WHERE sd_a0 IS NOT DISTINCT FROM (t.gddm)"
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
    assert!(out.contains("(SELECT sd_v FROM sd_fn0"));
}

#[test]
fn should_dedupe_identical_calls_and_inline_each_occurrence() {
    let sql = "SELECT fnc_x(a), fnc_x(a) FROM t GROUP BY fnc_x(a)";
    let (out, sites) =
        plan_sites(sql, &VarScope::new(), &BTreeMap::new(), &names(&["fnc_x"])).unwrap();
    assert_eq!(sites.len(), 1, "同 name+实参只物化一次");
    assert_eq!(out.matches("(SELECT sd_v FROM sd_fn0").count(), 3);
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
    assert!(out.contains("(SELECT sd_v FROM sd_fn0"));
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
    assert_eq!(out.matches("(SELECT sd_v FROM sd_fn0").count(), 2);
}

#[test]
fn should_take_insert_select_context_per_branch_for_parameter_tuples() {
    // 语料 prc_pre_deal_bjgsyh 形态：INSERT INTO … SELECT … UNION ALL SELECT …，
    // 调用在 SELECT 列表且实参是**源表列**——单行元组查询无法解析列名，
    // 参数元组必须取各自分支的 FROM/WHERE。
    let sql = "INSERT INTO out_trd_bjgsyh (trade_no, stock_kind) SELECT a.trade_no, fnc_get_stock_kind(a.stock_code, p_i_date, v_scdm) FROM bjgsyh_temp a, dic_stock_kind_sub t2 WHERE a.stock_code between t2.begin_code and t2.end_code AND a.proc IN ('01', '02') UNION ALL SELECT b.trade_no, fnc_get_stock_kind(b.stock_code, p_i_date, v_scdm) FROM tsbjgsyh_temp b WHERE b.status = '1'";
    let vars: VarScope = ["p_i_date".to_string(), "v_scdm".to_string()]
        .into_iter()
        .collect();
    let (out, sites) = plan_sites(
        sql,
        &vars,
        &BTreeMap::new(),
        &names(&["fnc_get_stock_kind"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 2, "实参文本不同 → 两个调用点：{sites:?}");
    for s in &sites {
        assert!(
            s.distinct_sql.contains("FROM"),
            "{} 的元组查询应带分支上下文：{}",
            s.name,
            s.distinct_sql
        );
    }
    let a_site = sites
        .iter()
        .find(|s| s.args[0].contains('a'))
        .expect("a 分支调用点");
    assert!(
        a_site.distinct_sql.contains("bjgsyh_temp"),
        "a 分支取自身源表：{}",
        a_site.distinct_sql
    );
    let b_site = sites
        .iter()
        .find(|s| s.args[0].contains('b'))
        .expect("b 分支调用点");
    assert!(
        b_site.distinct_sql.contains("tsbjgsyh_temp"),
        "b 分支取自身源表：{}",
        b_site.distinct_sql
    );
    assert_eq!(out.matches("(SELECT sd_v FROM sd_fn").count(), 2, "{out}");
    assert!(!out.contains("fnc_get_stock_kind("), "{out}");
}

#[test]
fn should_lower_routine_call_nested_in_another_call_arguments() {
    // 语料费用链形态：fnc_get_bond_yjlx(substr(fnc_get_stock_kind(...),1,2) || '001', …)
    // 内层先物化，外层元组查询引用内层缓存表；整条语句不得残留例程原名。
    let sql = "INSERT INTO out_trd_bjgsyh (trade_no, cjje) SELECT a.trade_no, a.vol * 10 * fnc_get_bond_yjlx(substr(fnc_get_stock_kind(a.stock_code, p_i_date, v_scdm), 1, 2) || '001', p_i_date, 1, '-1') FROM bjgsyh_temp a WHERE a.proc = '01'";
    let vars: VarScope = ["p_i_date".to_string(), "v_scdm".to_string()]
        .into_iter()
        .collect();
    let (out, sites) = plan_sites(
        sql,
        &vars,
        &BTreeMap::new(),
        &names(&["fnc_get_bond_yjlx", "fnc_get_stock_kind"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 2, "内外各一个调用点：{sites:?}");
    let inner = sites
        .iter()
        .find(|s| s.name == "fnc_get_stock_kind")
        .expect("内层调用点");
    let outer = sites
        .iter()
        .find(|s| s.name == "fnc_get_bond_yjlx")
        .expect("外层调用点");
    assert!(
        outer
            .distinct_sql
            .contains(&format!("FROM {}", inner.table)),
        "外层元组查询须引用内层缓存表：{}",
        outer.distinct_sql
    );
    let order: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        order.first().copied(),
        Some("fnc_get_stock_kind"),
        "内层先物化（拓扑序）：{sites:?}"
    );
    assert!(
        !out.contains("fnc_get_bond_yjlx(") && !out.contains("fnc_get_stock_kind("),
        "{out}"
    );
}

#[test]
fn should_lower_routine_call_in_another_call_arguments() {
    // 行为变更（原测试 `should_reject_nested_routine_call_in_arguments` 断言 BIZ-0158 拒绝）：
    // 语料存在 `fnc_get_bond_yjlx(substr(fnc_get_stock_kind(…), …))` 形态，实参内嵌套调用
    // 改为逐层下沉（内层先物化，外层元组查询引用内层缓存表）；BIZ-0158 随之下线。
    let (out, sites) = plan_sites(
        "SELECT fnc_a(fnc_b(x)) FROM t",
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["fnc_a", "fnc_b"]),
    )
    .unwrap();
    assert_eq!(
        out,
        "SELECT (SELECT sd_v FROM sd_fn0 WHERE sd_a0 IS NOT DISTINCT FROM ((SELECT sd_v FROM sd_fn1 WHERE sd_a0 IS NOT DISTINCT FROM (x)))) FROM t"
    );
    let order: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(order, vec!["fnc_b", "fnc_a"], "内层先物化");
    assert_eq!(sites[0].table, "sd_fn1");
    assert_eq!(sites[0].distinct_sql, "SELECT DISTINCT x FROM t");
    assert_eq!(sites[1].table, "sd_fn0");
    assert_eq!(
        sites[1].distinct_sql,
        "SELECT DISTINCT (SELECT sd_v FROM sd_fn1 WHERE sd_a0 IS NOT DISTINCT FROM (x)) FROM t"
    );
    assert_eq!(
        sites[1].args,
        vec!["fnc_b(x)".to_string()],
        "诊断口径仍给原文实参"
    );
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

#[test]
fn should_lower_package_qualified_routine_call_in_sql_context() {
    // 语料以 package.procedure 全名引用例程；目录键是点号全名，扫描必须
    // 按点号标识符序列匹配，否则原始调用文本直达内核（Catalog Error）。
    let (out, sites) = plan_sites(
        "SELECT pkg_demo.f_tax(a) FROM t",
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["pkg_demo.f_tax"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].name, "pkg_demo.f_tax");
    assert_eq!(sites[0].distinct_sql, "SELECT DISTINCT a FROM t");
    assert!(out.contains("(SELECT sd_v FROM sd_fn0"), "{out}");
    assert!(!out.contains("pkg_demo.f_tax("), "{out}");
}

#[test]
fn should_match_bare_name_when_only_qualified_key_is_registered() {
    // 注册表同时给裸名与全名时（见 catalog 的裸名别名），裸名调用照旧命中
    let (_, sites) = plan_sites(
        "SELECT f_tax(a) FROM t",
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["pkg_demo.f_tax", "f_tax"]),
    )
    .unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].name, "f_tax");
}

#[test]
fn should_not_lower_qualified_call_absent_from_catalog() {
    // 只有目录里有点号全名的调用才下沉；未注册的 pkg.f 保持原样
    let (out, sites) = plan_sites(
        "SELECT other_pkg.f_tax(a) FROM t",
        &VarScope::new(),
        &BTreeMap::new(),
        &names(&["pkg_demo.f_tax"]),
    )
    .unwrap();
    assert!(sites.is_empty());
    assert_eq!(out, "SELECT other_pkg.f_tax(a) FROM t");
}

#[test]
fn should_not_bind_cache_columns_when_pl_variable_shares_their_names() {
    // 缓存列名与 PL 变量同名曾击穿下沉：`DECLARE v …` 的过程里，管线在调用点
    // 替换之后还要跑变量绑定，缓存子查询的裸值列 `v`（及参数列 `a0`）会被替换成
    // 绑定占位 `$v_v` → 内核找不到列，SELECT INTO 取到 NULL。
    // 列名与表名同用 sd_ 保留前缀，与变量命名空间隔离。
    let rules = crate::rules::RuleRegistry::default();
    let vars: VarScope = ["v".to_string(), "a0".to_string()].into_iter().collect();
    let unit = crate::ir::rewrite_sql(
        "SELECT pkg.f(amt) FROM t WHERE id = 3",
        &vars,
        &rules,
        &names(&["pkg.f"]),
    )
    .unwrap();
    assert_eq!(
        unit.text,
        "SELECT (SELECT sd_v FROM sd_fn0 WHERE sd_a0 IS NOT DISTINCT FROM (amt)) FROM t WHERE id = 3"
    );
    assert!(unit.binds.is_empty(), "不应产生变量绑定：{:?}", unit.binds);
}
