//! 游标查询形态升级守护（V2 设计 §5-4：别名投影、WHERE 全定义证明、
//! 查询绑定注入批写；字符串字面量大小写不得被 token 归一篡改）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        "CREATE TABLE t_copy(val BIGINT)",
        "INSERT INTO source_rows VALUES (10, 1), (20, 2), (30, NULL)",
        "CREATE TABLE t_codes(code VARCHAR)",
        "INSERT INTO t_codes VALUES ('ES'), ('es'), ('77')",
        "CREATE TABLE t_codes_copy(code VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query("SELECT val FROM t_copy ORDER BY val", &BindMap::new())
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_convert_with_unused_quoted_catalog_columns() {
    let sql = "CREATE PROCEDURE copy_quoted IS BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        INSERT INTO t_copy(val) VALUES (r.a); END LOOP; END;";
    let setup = || {
        let mut eng = engine();
        for ddl in [
            "ALTER TABLE source_rows ADD COLUMN \"operator\" VARCHAR",
            "ALTER TABLE t_copy ADD COLUMN \"$v_x\" VARCHAR",
            "ALTER TABLE t_copy ADD COLUMN \"Mixed\"\"Name\" VARCHAR",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let output = |eng: &Engine| {
        eng.ex
            .query("SELECT * FROM t_copy ORDER BY val", &BindMap::new())
            .unwrap()
            .fetch_many(20)
    };
    let mut baseline = setup();
    baseline.call("copy_quoted", &[]).unwrap();
    let expected = [10, 20, 30]
        .into_iter()
        .map(|n| vec![SpinValue::Int(n), SpinValue::Null, SpinValue::Null])
        .collect::<Vec<_>>();
    assert_eq!(output(&baseline), expected);
    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_quoted", &[]).unwrap();
    assert_eq!(output(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()
    );
}

#[test]
fn should_convert_with_unused_temporal_columns() {
    let sql = "CREATE PROCEDURE copy_temporal IS BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        INSERT INTO t_copy(val) VALUES (r.a); END LOOP; END;";
    let setup = || {
        let mut eng = engine();
        for ddl in [
            "ALTER TABLE source_rows ADD COLUMN checked_at TIMESTAMP",
            "ALTER TABLE source_rows ADD COLUMN checked_time TIME",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_temporal", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Int(10)],
        vec![SpinValue::Int(20)],
        vec![SpinValue::Int(30)],
    ];
    assert_eq!(rows(&baseline), expected);
    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_temporal", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()
    );
}

#[test]
fn should_convert_cursor_with_aliases_and_where_filter() {
    // 别名投影（AS x / 裸 y）+ WHERE 过滤：记录字段按别名映射源列，
    // NULL b 行被 WHERE 排除；BatchAtomic Converted 且与 Off 逐行一致。
    let sql = "CREATE PROCEDURE copy_alias IS BEGIN \
        FOR r IN SELECT s.a AS x, s.b y FROM source_rows s WHERE s.b > 0 LOOP \
        INSERT INTO t_copy(val) VALUES (r.x); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_alias", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(
        expected,
        vec![vec![SpinValue::Int(10)], vec![SpinValue::Int(20)]]
    );

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_alias", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0].status
    );
}

#[test]
fn should_carry_cursor_query_binds_into_batch_sql() {
    // 游标 WHERE 引用 IN 形参：绑定必须随批写 SqlExec 注入，
    // 否则运行期占位符未绑定报错。
    let sql = "CREATE PROCEDURE copy_bind(p_i IN NUMBER) IS BEGIN \
        FOR r IN SELECT s.a FROM source_rows s WHERE s.a > p_i LOOP \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine();
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_bind", &[SpinValue::Int(15)]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(
        expected,
        vec![vec![SpinValue::Int(20)], vec![SpinValue::Int(30)]]
    );

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_bind", &[SpinValue::Int(15)]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0].status
    );
}

#[test]
fn should_reject_cursor_filter_with_unproven_expression() {
    // WHERE 含除法：非全定义白名单形态，保守拒绝（UnprovenQueryTotalness），
    // 原区域解释执行与 Off 逐行一致。
    let sql = "CREATE PROCEDURE copy_divfilter IS BEGIN \
        FOR r IN SELECT s.a FROM source_rows s WHERE s.a / s.b > 0 LOOP \
        INSERT INTO t_copy(val) VALUES (r.a); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_divfilter", &[]).unwrap();
    let expected = rows(&baseline);

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_divfilter", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenQueryTotalness
        }
    );
}

#[test]
fn should_preserve_string_literal_case_in_cursor_filter() {
    // 字符串字面量 'ES' 不得被 token 小写归一篡改为 'es'：
    // 大小写敏感比较下 Converted 与 Off 行集必须一致。
    let sql = "CREATE PROCEDURE copy_codes IS BEGIN \
        FOR r IN SELECT s.code FROM t_codes s WHERE s.code IN ('ES', '77') LOOP \
        INSERT INTO t_codes_copy(code) VALUES (r.code); \
        END LOOP; END;";
    let codes = |eng: &Engine| {
        eng.ex
            .query(
                "SELECT code FROM t_codes_copy ORDER BY code",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20)
    };
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_codes", &[]).unwrap();
    let expected = codes(&baseline);
    assert_eq!(
        expected,
        vec![
            vec![SpinValue::Str("77".into())],
            vec![SpinValue::Str("ES".into())]
        ]
    );

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_codes", &[]).unwrap();
    assert_eq!(codes(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0].status
    );
}
