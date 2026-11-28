//! PL 中缀 MOD 显式拒绝与 `%` 取余语义守护（GaussDB A 模式实测口径）。
//!
//! 参照实测（2026-11-28 openGauss 5.0.0 容器，A 模式）：
//! - 中缀 `MOD` 在 SQL 与 PL 里**都是语法错误**——含 `7 MOD (3)`、`7 MOD(3)`；
//!   尾随 `mod`（`SELECT 7 mod FROM t`）是别名不报错；`mod(a, b)` 函数形式可用。
//! - `%` 运算符可用：`7 % 3` = 1、`-7 % 3` = -1（符号随被除数）、
//!   `7.5 % 2` = 1.5、`-7.5 % 2` = -1.5；`x % 0` 报除零。
//!
//! SpinDuck 的解析层（ogsql-parser，公共 API，行为问题只记录）对中缀 MOD
//! 是**有损解析**：`x := (v MOD 3)` 与 `RETURN (v MOD 3)` 的语句被整体丢弃、
//! `SELECT 7 MOD 3 INTO v` 丢成 `SELECT 7`——AST 无痕迹，无法在 IR 层检出。
//! 因此拒绝点在词元层：值操作数后跟裸 `MOD`（且后随值类词元）即编译期拒绝
//! （BIZ-0126，参照 SQLSTATE 语法错误域）。四个历史形态
//! （X4.mod / X4.modsql / X4.retmod / X4.lit）全部覆盖。
#![cfg(feature = "native")]

use spinduck_core::{DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    )
}

/// 用 `v := <expr>; RETURN v` 求值（与 guard_runtime_division_gaussdb 同口径）。
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

fn assert_rejected(eng: &mut Engine, sql: &str) {
    let error = eng.register(sql).unwrap_err();
    assert_eq!(error.code.0, "BIZ-0126", "{sql}");
    assert!(
        error.message.contains("MOD") && error.message.contains("mod(a, b)"),
        "{sql} => {}",
        error.message
    );
}

#[test]
fn should_reject_infix_mod_in_pl_assignment() {
    let mut eng = engine();
    assert_rejected(
        &mut eng,
        "CREATE OR REPLACE FUNCTION f1 RETURN NUMBER IS \
         v NUMBER := 7; x NUMBER; BEGIN x := (v MOD 3); RETURN x; END;",
    );
}

#[test]
fn should_reject_infix_mod_in_pl_return() {
    let mut eng = engine();
    assert_rejected(
        &mut eng,
        "CREATE OR REPLACE FUNCTION f2 RETURN NUMBER IS \
         v NUMBER := 7; BEGIN RETURN (v MOD 3); END;",
    );
}

#[test]
fn should_reject_infix_mod_in_static_sql() {
    let mut eng = engine();
    assert_rejected(
        &mut eng,
        "CREATE OR REPLACE FUNCTION f3 RETURN NUMBER IS \
         v NUMBER; BEGIN SELECT 7 MOD 3 INTO v FROM sys_dummy; RETURN v; END;",
    );
}

#[test]
fn should_reject_infix_mod_in_return_literal() {
    // 历史上这条拆成 `RETURN 7` + SQL 语句 "MOD 3" 报 ENG-0022；词元扫描
    // 统一为 BIZ-0126（编译失败的行为不变，错误码与参照语义对齐）。
    let mut eng = engine();
    assert_rejected(
        &mut eng,
        "CREATE OR REPLACE FUNCTION f4 RETURN NUMBER IS BEGIN RETURN 7 MOD 3; END;",
    );
}

#[test]
fn should_reject_infix_mod_before_parenthesized_operand() {
    // 参照实测：`7 MOD (3)` 与 `7 MOD(3)` 都是语法错误，函数形式 mod (7,3) 可用。
    let mut eng = engine();
    assert_rejected(
        &mut eng,
        "CREATE OR REPLACE FUNCTION f5 RETURN NUMBER IS \
         x NUMBER; BEGIN x := 7 MOD (3); RETURN x; END;",
    );
}

#[test]
fn should_ignore_mod_in_comments_and_strings() {
    // 注释与字符串字面量里的 MOD 不误伤；语句本身合法（mod 函数形式 + 字符串）
    let mut eng = engine();
    let sql = "CREATE OR REPLACE FUNCTION f6 RETURN NUMBER IS \
         -- 注释里的 v MOD 3 不触发\n \
         x NUMBER; BEGIN /* block MOD 3 */ x := 'MOD'; RETURN x; END;";
    eng.register(sql).unwrap();
    let r = eng.call("f6", &[]).unwrap();
    assert_eq!(r.return_value, Some(SpinValue::Str("MOD".into())));
}

#[test]
fn should_keep_mod_function_form_working() {
    let mut eng = engine();
    assert_eq!(eval(&mut eng, "mod(7, 3)"), SpinValue::Int(1));
    assert_eq!(eval(&mut eng, "mod (7, 3)"), SpinValue::Int(1));
}

#[test]
fn should_modulo_integers_like_gaussdb_percent() {
    let mut eng = engine();
    // 7 % 3 = 1；-7 % 3 = -1（符号随被除数）
    assert_eq!(eval(&mut eng, "7 % 3"), SpinValue::Int(1));
    assert_eq!(eval(&mut eng, "-7 % 3"), SpinValue::Int(-1));
    assert_eq!(eval(&mut eng, "0 % 3"), SpinValue::Int(0));
}

#[test]
fn should_modulo_decimals_like_gaussdb_percent() {
    let mut eng = engine();
    // 7.5 % 2 = 1.5；-7.5 % 2 = -1.5（实测定标：小数位取操作数较大者）
    assert_eq!(eval(&mut eng, "7.5 % 2"), SpinValue::Decimal(15, 38, 1));
    assert_eq!(eval(&mut eng, "-7.5 % 2"), SpinValue::Decimal(-15, 38, 1));
    assert_eq!(eval(&mut eng, "7 % 2.0"), SpinValue::Decimal(10, 38, 1));
}

#[test]
fn should_raise_biz_0124_on_percent_zero_divisor() {
    let mut eng = engine();
    let sql = "CREATE OR REPLACE FUNCTION f_z RETURN NUMBER IS \
         BEGIN RETURN 7 % 0; END;";
    eng.register(sql).unwrap();
    let error = eng.call("f_z", &[]).unwrap_err();
    assert_eq!(error.code.0, "BIZ-0124");
    assert!(
        error.message.contains("division by zero"),
        "{}",
        error.message
    );
}
