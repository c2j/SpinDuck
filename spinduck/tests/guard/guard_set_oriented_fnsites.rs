//! 引号美元列名误分类与残留 fn_sites 守护（V2 设计 §6.2 / §4.2）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT)",
        "INSERT INTO source_rows VALUES (10), (20)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

#[test]
fn should_reject_quoted_dollar_column_misclassification() {
    // 情形 1：目标列名恰为 "$v_x"——不得被误当绑定占位符；
    // 形态白名单无法证明时保守跳过，Off 基线逐行一致。
    let proc = "CREATE PROCEDURE copy_dollar IS BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        INSERT INTO dollar_cols(\"$v_x\") VALUES (r.a); \
        END LOOP; END;";
    let setup_dollar = || {
        let mut eng = engine();
        eng.ex
            .execute("CREATE TABLE dollar_cols(\"$v_x\" BIGINT)", &BindMap::new())
            .unwrap();
        eng.register(proc).unwrap();
        eng
    };
    let dollar_rows = |eng: &Engine| {
        eng.ex
            .query(
                "SELECT \"$v_x\" FROM dollar_cols ORDER BY 1",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20)
    };
    let mut baseline = setup_dollar();
    baseline.call("copy_dollar", &[]).unwrap();
    let expected = dollar_rows(&baseline);
    assert_eq!(expected.len(), 2);

    let mut eng = setup_dollar();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_dollar", &[]).unwrap();
    assert_eq!(dollar_rows(&eng), expected);
    assert!(
        matches!(eng.set_oriented_reports()[0].status, Status::Skipped { .. }),
        "quoted dollar column must not be misclassified: {:?}",
        eng.set_oriented_reports()[0].status
    );
}

#[test]
fn should_reject_unresolved_fn_sites() {
    let setup_fnsite = || {
        let mut eng = engine();
        eng.ex
            .execute("CREATE TABLE target_rows(val BIGINT)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE FUNCTION f_id(p_a NUMBER) RETURN NUMBER IS v NUMBER; \
            BEGIN v := p_a; RETURN v; END;",
        )
        .unwrap();
        eng.register(
            "CREATE PROCEDURE copy_fnsite IS BEGIN \
            FOR r IN SELECT f_id(a) AS x FROM source_rows LOOP \
            INSERT INTO target_rows(val) VALUES (r.x); \
            END LOOP; END;",
        )
        .unwrap();
        eng
    };
    let fnsite_rows = |eng: &Engine| {
        eng.ex
            .query(
                "SELECT val FROM target_rows ORDER BY rowid",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20)
    };
    let mut baseline = setup_fnsite();
    baseline.call("copy_fnsite", &[]).unwrap();
    let expected = fnsite_rows(&baseline);
    assert_eq!(
        expected,
        vec![vec![SpinValue::Int(10)], vec![SpinValue::Int(20)]]
    );

    let mut eng = setup_fnsite();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_fnsite", &[]).unwrap();
    assert_eq!(fnsite_rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnresolvedFnSite
        }
    );
}
