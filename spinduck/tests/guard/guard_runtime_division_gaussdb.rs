//! GaussDB A 模式 PL 数值除法守护（2026-11-28 openGauss 5.0.0 实测）。
//!
//! 两条口径必须同时钉死：
//! - 整数 `/` 走 float8，赋给 NUMBER 时按 `DBL_DIG=15` 位有效数字固化；
//! - DECIMAL/NUMBER `/` 走 PostgreSQL `select_div_scale`：至少 16 位有效
//!   数字，结果小数位随商的首位权按 4 位一组变化，并按 half-away-from-zero
//!   舍入。
//!
//! 期望值全部来自参照库实测（`.5` 与 `0.5` 只差显示前导零，`norm` 同口径），
//! 不得按实现现状反推。集合化改写或定标优化不得改变这些值。
#![cfg(feature = "native")]

use spinduck_core::{DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    )
}

/// 用 `v := <expr>; RETURN v` 求值，避开 RETURN 直接返回表达式的编译差异。
fn eval(eng: &mut Engine, expr: &str) -> SpinValue {
    let sql = format!(
        "CREATE OR REPLACE FUNCTION f_probe RETURN NUMBER IS \
         v NUMBER; BEGIN v := {expr}; RETURN v; END;"
    );
    eng.register(&sql).unwrap();
    eng.call("f_probe", &[])
        .unwrap()
        .return_value
        .unwrap_or_else(|| panic!("f_probe({expr}) returned no value"))
}

#[test]
fn should_divide_pl_integers_like_gaussdb_float8() {
    let mut eng = engine();
    // openGauss 5.0.0：1/2=.5、2/3=.666666666666667、6/3=2、
    // -1/3=-.333333333333333、9007199254740993/1=9007199254740990
    for (expr, want) in [
        ("1 / 2", SpinValue::Decimal(5, 38, 1)),
        ("2 / 3", SpinValue::Decimal(666666666666667, 38, 15)),
        ("6 / 3", SpinValue::Decimal(2, 38, 0)),
        ("-1 / 3", SpinValue::Decimal(-333333333333333, 38, 15)),
        (
            "9007199254740993 / 1",
            SpinValue::Decimal(9007199254740990, 38, 0),
        ),
    ] {
        assert_eq!(eval(&mut eng, expr), want, "{expr}");
    }
}

#[test]
fn should_apply_select_div_scale_like_gaussdb_numeric() {
    let mut eng = engine();
    // 实测：1.0/3.0 二十位小数；100.0/3.0 十六位；123456.0/3.0 十二位；
    // 1234567890123.0/3.0 八位；0.00001/3.0 二十四位；2.0/3.0 末位进位。
    for (expr, want) in [
        (
            "1.0 / 3.0",
            SpinValue::Decimal(33333333333333333333, 38, 20),
        ),
        (
            "100.0 / 3.0",
            SpinValue::Decimal(333333333333333333, 38, 16),
        ),
        (
            "123456.0 / 3.0",
            SpinValue::Decimal(41152000000000000, 38, 12),
        ),
        (
            "99999.0 / 3.0",
            SpinValue::Decimal(33333000000000000, 38, 12),
        ),
        (
            "1234567890123.0 / 3.0",
            SpinValue::Decimal(41152263004100000000, 38, 8),
        ),
        (
            "0.00001 / 3.0",
            SpinValue::Decimal(3333333333333333333, 38, 24),
        ),
        (
            "1.0 / 100.0",
            SpinValue::Decimal(1000000000000000000, 38, 20),
        ),
        (
            "1.0 / 8.0",
            SpinValue::Decimal(12500000000000000000, 38, 20),
        ),
        (
            "2.0 / 3.0",
            SpinValue::Decimal(66666666666666666667, 38, 20),
        ),
        (
            "1.0 / 2.0",
            SpinValue::Decimal(50000000000000000000, 38, 20),
        ),
        ("1 / 2.0", SpinValue::Decimal(50000000000000000000, 38, 20)),
    ] {
        assert_eq!(eval(&mut eng, expr), want, "{expr}");
    }
}

/// 已知差距①（复审 2026-11-28 登记，勿「顺手修复」）：**内联链式**除法里，
/// 本仓把每个除法结果先固化为 15 位有效数字的 DECIMAL 再相加（…334），
/// 参照全程 float8、输出时才 %.15g（`2/3 + 2/3` = 1.33333333333333）。
/// 根因是无 float8 值域；经 NUMBER 变量中转的链式（`a := 2/3; b := 2/3;
/// a + b`）参照同样得 …334，与本仓一致——只有内联形态分歧。
#[test]
fn should_pin_inline_chained_division_sum_as_known_gap() {
    let mut eng = engine();
    assert_eq!(
        eval(&mut eng, "2 / 3 + 2 / 3"),
        SpinValue::Decimal(1333333333333334, 38, 15)
    );
}

/// 已知差距②（复审 2026-11-28 登记，勿「顺手修复」）：整数槽直赋除法，
/// 参照 `v INTEGER := 1/2` 得 0（float8 → int 截断固化），本仓保留 0.5
/// （INTEGER 声明槽不固化类型，var_types 仅 VARCHAR 强转）——同根因
/// （无 float8 值域 + 变量槽无数值类型收敛）。
#[test]
fn should_pin_integer_slot_direct_assignment_division_as_known_gap() {
    let mut eng = engine();
    let sql = "CREATE OR REPLACE FUNCTION f_int_slot RETURN INTEGER IS \
         v INTEGER; BEGIN v := 1 / 2; RETURN v; END;";
    eng.register(sql).unwrap();
    let r = eng.call("f_int_slot", &[]).unwrap();
    assert_eq!(r.return_value, Some(SpinValue::Decimal(5, 38, 1)));
}
