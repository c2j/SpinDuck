//! SQL 与 PL 字符串拼接的类型和 NULL 守护。

use super::*;

#[test]
fn should_convert_concat_preserving_both_null_semantics() {
    // 原生 SQL 面的 `||` 经 concat_null_as_empty 改写为 concat(...)：
    // 单侧 NULL 视作空串、全 NULL 得 ''（Off 实测）。转换必须逐字复现，
    // 不得套用 PL 解释器层 Concat 的双 NULL→NULL CASE（那是下沉路径）。
    let sql = "CREATE PROCEDURE copy_cat IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.a || r.b); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a VARCHAR, b VARCHAR)",
            "CREATE TABLE target_rows(val VARCHAR)",
            "INSERT INTO source_rows VALUES (NULL, NULL), (NULL, 'x'), ('a', NULL), ('a', 'b')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_cat", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Str("".into())],
        vec![SpinValue::Str("x".into())],
        vec![SpinValue::Str("a".into())],
        vec![SpinValue::Str("ab".into())],
    ];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_cat", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_sink_pl_concat_with_both_null_case() {
    // 设计 §4.6：PL 解释器 Concat 双侧 NULL→NULL、单侧 NULL 视作空串
    // （runtime/eval.rs:215-216），与原生 SQL 面 concat_null_as_empty 的
    // 全 NULL→'' 不同；下沉必须生成双 NULL CASE，不得裸用 concat。
    let sql = "CREATE PROCEDURE copy_pl IS v VARCHAR2(40); BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a || r.b; INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a VARCHAR, b VARCHAR)",
            "CREATE TABLE target_rows(val VARCHAR)",
            "INSERT INTO source_rows VALUES (NULL, NULL), (NULL, 'x'), ('a', NULL), ('a', 'b')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Null],
        vec![SpinValue::Str("x".into())],
        vec![SpinValue::Str("a".into())],
        vec![SpinValue::Str("ab".into())],
    ];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_sink_pl_concat_decimal_without_float_suffix() {
    // DECIMAL(p,0) 拼接按整数串（无 `.0`）；DECIMAL(p,s>0) 按定宽小数
    // （as_str_loose 与 DuckDB decimal→VARCHAR 同口径），不得浮点化。
    let sql = "CREATE PROCEDURE copy_pl IS v VARCHAR2(40); BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a || r.b; INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a DECIMAL(16,0), b DECIMAL(15,3))",
            "CREATE TABLE target_rows(val VARCHAR)",
            "INSERT INTO source_rows VALUES (5, 1.5), (NULL, NULL)",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[]).unwrap();
    let expected = vec![vec![SpinValue::Str("51.500".into())], vec![SpinValue::Null]];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}
