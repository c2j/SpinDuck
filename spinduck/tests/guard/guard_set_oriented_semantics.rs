//! §4.6 四层语义保全守护：SqlFn/日期算术与异构 CASE 不得被集合化篡改。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine(ddls: &[&str]) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in ddls {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val FROM target_rows ORDER BY rowid",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

#[test]
fn should_preserve_sqlfn_date_arithmetic_and_concat_semantics() {
    // 设计 §6.2：to_date(a) - to_date(b) 的日期差（INTERVALTONUM/SqlFn
    // 改写面）不在表达式白名单内，必须保守跳过且 Off 语义逐字不变。
    // Concat(NULL,NULL)→NULL 与 numeric(16,0) 无 `.0` 拼接由
    // guard_set_oriented_expr 的同名簇锁定。
    let sql = "CREATE PROCEDURE copy_date IS BEGIN \
        FOR r IN SELECT d, e FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) \
        VALUES (to_date(r.d, 'YYYYMMDD') - to_date(r.e, 'YYYYMMDD')); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine(&[
            "CREATE TABLE source_rows(d VARCHAR, e VARCHAR)",
            "CREATE TABLE target_rows(val VARCHAR)",
            "INSERT INTO source_rows VALUES ('20250715', '20250701'), ('20250301', '20250228')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ]);
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_date", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Str("14 days".into())],
        vec![SpinValue::Str("1 day".into())],
    ];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_date", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenWriteCompleteness
        }
    );
}

#[test]
fn should_defer_heterogeneous_case_alignment_to_write_sink() {
    // 设计 §4.6/§6.2：v_yj 型异构分支（DECIMAL 列值 vs 字符串 '0'）严禁
    // 在中间赋值位置提前数值强转——'0' || 'x' 必须得 '0x' 而非 '0.000x'。
    // 含 IF 的区域当前保守跳过；守护锁定 Off 的分支类型语义不被篡改。
    let sql = "CREATE PROCEDURE copy_het IS v NUMBER; BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        BEGIN IF r.a > 0 THEN v := r.a; ELSE v := '0'; END IF; \
        INSERT INTO target_rows(val) VALUES (v || 'x'); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine(&[
            "CREATE TABLE source_rows(a DECIMAL(15,3))",
            "CREATE TABLE target_rows(val VARCHAR)",
            "INSERT INTO source_rows VALUES (5), (-1)",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ]);
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_het", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Str("5.000x".into())],
        vec![SpinValue::Str("0x".into())],
    ];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_het", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        }
    );
}
