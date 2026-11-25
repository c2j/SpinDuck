//! Issue #65 审核修复回归（第二轮）：四个已复现语义问题 + 别名站点。
//!
//! - B1：残余谓词含算术 → 预计算溢出污染事务，结果必须与 Off 一致；
//! - B2：内外层例程同 SQL 文本不同不变量绑定 → 全绑定探测键隔离；
//! - B3：DDL 重建键列类型 → SiteSpec 必须重证，不得复用旧 proof；
//! - B4：DOUBLE 键有损舍入 → 键列含浮点类型整站禁用；
//! - 别名：`FROM t AS a` + 限定列的 B 面站点应同样命中缓存。

#![cfg(feature = "native")]

#[path = "common/issue65.rs"]
mod issue65;

use issue65::*;
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::value::SpinValue;

fn batch_rows(setup: fn() -> Case1Fixture, routine: &str, table_sql: &str) -> Vec<Vec<SpinValue>> {
    let mut fx = setup();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call(routine, &[]).expect("batch call");
    table_rows(&mut fx.eng, table_sql)
}

fn off_rows(setup: fn() -> Case1Fixture, routine: &str, table_sql: &str) -> Vec<Vec<SpinValue>> {
    let mut off = setup();
    off.eng.call(routine, &[]).expect("off call");
    table_rows(&mut off.eng, table_sql)
}

#[test]
fn should_issue65_b1_arith_residual_never_reaches_build() {
    // 审核复现：游标仅查 A，维度表 Z 行 pri=BIGINT_MAX，残余 `pri*pri>0`。
    // Off 与 BatchAtomic 都必须写 'KA'——算术残余必须在计划期禁用缓存，
    // 不得让预计算溢出污染事务。
    let off = off_rows(
        setup_arith,
        "issue65_arith",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(
        off,
        vec![vec![
            SpinValue::Int(1),
            SpinValue::Str("A".into()),
            SpinValue::Str("KA".into())
        ]],
        "Off 参考自检"
    );
    let batch = batch_rows(
        setup_arith,
        "issue65_arith",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "B1：算术残余必须禁用缓存，输出与 Off 一致");
}

#[test]
fn should_issue65_b2_nested_routine_binds_isolated() {
    // 审核复现：内层 p_biz_date=20200101 先触发预计算，外层
    // p_biz_date=20220601 不得命中内层缓存（probe key 必须含全部绑定）。
    let mut off = setup_nested();
    off.eng.call("issue65_outer", &[]).expect("off call");
    let off_inner = table_rows(
        &mut off.eng,
        "SELECT seq, kind FROM inner_result ORDER BY seq",
    );
    let off_outer = table_rows(
        &mut off.eng,
        "SELECT seq, kind FROM outer_result ORDER BY seq",
    );
    assert_eq!(
        off_inner,
        vec![vec![SpinValue::Int(1), SpinValue::Str("K2020".into())]],
        "Off 内层参考"
    );
    assert_eq!(
        off_outer,
        vec![vec![SpinValue::Int(1), SpinValue::Str("K2024".into())]],
        "Off 外层参考"
    );

    let batch = batch_rows(
        setup_nested,
        "issue65_outer",
        "SELECT seq, kind FROM outer_result ORDER BY seq",
    );
    assert_eq!(batch, off_outer, "B2：外层不得误用内层缓存");
    let mut fx = setup_nested();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_outer", &[]).expect("batch call");
    let batch_inner = table_rows(
        &mut fx.eng,
        "SELECT seq, kind FROM inner_result ORDER BY seq",
    );
    assert_eq!(batch_inner, off_inner, "B2：内层输出与 Off 一致");
}

#[test]
fn should_issue65_b3_ddl_rebuilds_site_proof() {
    // 审核复现：首次 lookup 前把 security_id 从 VARCHAR 重建为 BIGINT。
    // 站点必须丢弃旧 SiteSpec 并按新 schema 重证（类型不等 → 禁用缓存），
    // 原 SQL 隐式转换命中 'KA'，输出与 Off 一致。
    let off = off_rows(
        setup_ddl,
        "issue65_ddl",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(
        off,
        vec![vec![
            SpinValue::Int(1),
            SpinValue::Str("1".into()),
            SpinValue::Str("KA".into())
        ]],
        "Off 参考自检"
    );
    let batch = batch_rows(
        setup_ddl,
        "issue65_ddl",
        "SELECT seq, security_id, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "B3：DDL 后必须重证站点，输出与 Off 一致");
}

#[test]
fn should_issue65_b4_double_key_column_disables_cache() {
    // 审核复现：DOUBLE 键 1.0/1.0000004 经 6 位小数有损舍入折叠。
    // 键列含浮点类型必须整站禁用——两行输出与 Off 一致（均 K0）。
    let off = off_rows(
        setup_double,
        "issue65_double",
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    let mut fx = setup_double();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_double", &[]).expect("batch call");
    let batch = table_rows(
        &mut fx.eng,
        "SELECT seq, kind FROM result_rows ORDER BY seq",
    );
    assert_eq!(batch, off, "B4：DOUBLE 键必须禁用缓存");
    assert_eq!(
        lookup_calls(&fx.queries),
        2,
        "B4：DOUBLE 键列必须整站禁用——原形态 lookup 保持逐行"
    );
    assert_eq!(
        batch.iter().map(|r| r[1].clone()).collect::<Vec<_>>(),
        vec![SpinValue::Str("K0".into()), SpinValue::Str("K0".into())],
        "B4：两行均应命中 K0（Off 口径）"
    );
}

#[test]
fn should_issue65_alias_site_uses_cache() {
    // P1：`FROM asset_kind AS t` + `t.` 限定列的 B 面站点必须同样
    // 命中缓存（预计算恰 1 次、原形态 lookup 0 次），语义与 Off 一致。
    let mut off = setup_alias();
    off.eng.call("issue65_alias", &[]).expect("off call");
    let expected = result_rows(&mut off.eng);
    assert_eq!(expected, expected_case1_rows(), "Off 参考自检");

    let mut fx = setup_alias();
    fx.eng.set_oriented = SetOrientedMode::BatchAtomic;
    fx.eng.call("issue65_alias", &[]).expect("batch call");
    assert_eq!(result_rows(&mut fx.eng), expected, "别名站点语义一致");
    assert_eq!(
        lookup_calls(&fx.queries),
        0,
        "别名站点：原形态内核 lookup×0（缓存必须生效）"
    );
    let agg = fx
        .queries
        .borrow()
        .iter()
        .filter(|s| {
            let u = s.to_ascii_uppercase();
            u.contains("ASSET_KIND") && (u.contains("GROUP BY") || u.contains("COUNT("))
        })
        .count();
    assert_eq!(agg, 1, "别名站点：聚合预计算恰 1 次");
}
