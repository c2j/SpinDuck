//! PL 表达式/控制流的编译覆盖（语料体检发现的行为缺口，AGENTS §4.3 回归守护）。
//!
//! 全部走公共 API（Engine::register + Engine::call），不触碰 DuckDB：
//! 编译与 PL 上下文求值只依赖 NoopExecutor 接缝。

use spinduck_core::cli::NoopExecutor;
use spinduck_core::engine::Engine;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::value::SpinValue;

fn engine() -> Engine {
    Engine::new(Box::new(NoopExecutor), RuleRegistry::default())
}

#[test]
fn should_compile_if_without_else_branch() {
    // 回归：空 ELSE 时 `if_next` 标签未 mark，fixup 解析失败（ENG-0020）
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_if IS \
         BEGIN IF 1 > 0 THEN NULL; END IF; END;",
    )
    .unwrap();
    // ELSIF 链同样不得留下未解析标签
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_elsif IS \
         BEGIN IF 1 > 0 THEN NULL; ELSIF 1 < 0 THEN NULL; END IF; END;",
    )
    .unwrap();
}

#[test]
fn should_evaluate_unary_minus_and_parentheses() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_neg(p IN NUMBER) RETURN NUMBER IS \
         BEGIN RETURN -(p); END;",
    )
    .unwrap();
    let r = eng.call("f_neg", &[SpinValue::Int(7)]).unwrap();
    assert_eq!(r.return_value, Some(SpinValue::Int(-7)));

    eng.register(
        "CREATE OR REPLACE FUNCTION f_paren(p IN NUMBER) RETURN NUMBER IS \
         BEGIN RETURN (p + 1) * 3; END;",
    )
    .unwrap();
    let r = eng.call("f_paren", &[SpinValue::Int(2)]).unwrap();
    assert_eq!(r.return_value, Some(SpinValue::Int(9)));
}

#[test]
fn should_evaluate_between_inclusive_and_negated() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_btw(p IN NUMBER) RETURN BOOLEAN IS \
         BEGIN RETURN p BETWEEN 1 AND 3; END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_btw", &[SpinValue::Int(2)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(true))
    );
    assert_eq!(
        eng.call("f_btw", &[SpinValue::Int(4)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(false))
    );
    assert_eq!(
        eng.call("f_btw", &[SpinValue::Int(1)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(true)),
        "BETWEEN 闭区间"
    );

    eng.register(
        "CREATE OR REPLACE FUNCTION f_not_btw(p IN NUMBER) RETURN BOOLEAN IS \
         BEGIN RETURN p NOT BETWEEN 1 AND 3; END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_not_btw", &[SpinValue::Int(2)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(false))
    );
    assert_eq!(
        eng.call("f_not_btw", &[SpinValue::Int(9)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(true))
    );
}

#[test]
fn should_evaluate_in_list_and_negated_form() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_in(p IN VARCHAR2) RETURN BOOLEAN IS \
         BEGIN RETURN p IN ('02', '06'); END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_in", &[SpinValue::Str("06".into())])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(true))
    );
    assert_eq!(
        eng.call("f_in", &[SpinValue::Str("07".into())])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(false))
    );

    eng.register(
        "CREATE OR REPLACE FUNCTION f_not_in(p IN VARCHAR2) RETURN BOOLEAN IS \
         BEGIN RETURN p NOT IN ('02', '06'); END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_not_in", &[SpinValue::Str("07".into())])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(true))
    );
}

#[test]
fn should_resolve_goto_to_labeled_statement() {
    // 回归：`<<lbl>>` 被解析为带 label 的 Block，编译期未 mark 导致 ENG-0020
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_goto IS \
         DECLARE v_a NUMBER := 0; \
         BEGIN \
           LOOP \
             <<error_row>> \
             v_a := v_a + 1; \
             EXIT WHEN v_a >= 3; \
             IF v_a = 1 THEN GOTO error_row; END IF; \
           END LOOP; \
         END;",
    )
    .unwrap();
}

#[test]
fn should_compile_static_execute_immediate() {
    // 语料 PKG_SPLIT_TRADE_STEP2：EXECUTE IMMEDIATE 'truncate table ...'
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_dyn IS \
         BEGIN EXECUTE IMMEDIATE 'truncate table out_trd_gh_zyk'; END;",
    )
    .unwrap();
}

#[test]
fn should_compile_runtime_built_execute_immediate() {
    // 决策 2025-11-05（统一收口）：运行期拼接文本经同一条解析管线执行；
    // 解析失败在运行期显式报 BIZ-061，不再编译期整体拒绝
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_dyn2 IS \
         DECLARE v_sql VARCHAR2(100); \
         BEGIN v_sql := 'truncate table t'; EXECUTE IMMEDIATE v_sql; END;",
    )
    .unwrap();
}

#[test]
fn should_desugar_pl_case_statement() {
    // 简单 CASE（选择子）与搜索 CASE 都要落到首命中分支
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_case(p IN NUMBER) RETURN NUMBER IS \
         DECLARE v NUMBER := 0; \
         BEGIN \
           CASE p \
             WHEN 1 THEN v := 10; \
             WHEN 2 THEN v := 20; \
             ELSE v := 99; \
           END CASE; \
           RETURN v; \
         END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_case", &[SpinValue::Int(1)])
            .unwrap()
            .return_value,
        Some(SpinValue::Int(10))
    );
    assert_eq!(
        eng.call("f_case", &[SpinValue::Int(2)])
            .unwrap()
            .return_value,
        Some(SpinValue::Int(20))
    );
    assert_eq!(
        eng.call("f_case", &[SpinValue::Int(7)])
            .unwrap()
            .return_value,
        Some(SpinValue::Int(99))
    );

    eng.register(
        "CREATE OR REPLACE FUNCTION f_case2(p IN NUMBER) RETURN VARCHAR2 IS \
         DECLARE v VARCHAR2(10) := 'x'; \
         BEGIN \
           CASE \
             WHEN p < 0 THEN v := 'neg'; \
             WHEN p = 0 THEN v := 'zero'; \
             ELSE v := 'pos'; \
           END CASE; \
           RETURN v; \
         END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_case2", &[SpinValue::Int(-5)])
            .unwrap()
            .return_value,
        Some(SpinValue::Str("neg".into()))
    );
    assert_eq!(
        eng.call("f_case2", &[SpinValue::Int(0)])
            .unwrap()
            .return_value,
        Some(SpinValue::Str("zero".into()))
    );
}

#[test]
fn should_treat_decimal_literals_as_numbers_not_strings() {
    // 回归：小数曾被降级为字符串，导致 `x * 0.0008` 在 DuckDB 侧类型推断失败
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_rate(p IN NUMBER) RETURN NUMBER IS \
         BEGIN RETURN p * 0.0008; END;",
    )
    .unwrap();
    let r = eng.call("f_rate", &[SpinValue::Int(1000000)]).unwrap();
    // 标度取操作数最大标度（Int 0，字面量 4）→ 800.0000
    assert_eq!(
        r.return_value,
        Some(SpinValue::Decimal(8_000_000, 38, 4)),
        "1000000 * 0.0008 = 800.0000"
    );
}
