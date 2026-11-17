//! V2 集合化性能对比：大表逐行 vs 集合化，验证数据等价 + 打印加速比。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};
use std::time::Instant;

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
    END LOOP; END;";

fn setup(n: usize) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, payload VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    let batch = 1000;
    for start in (0..n).step_by(batch) {
        let end = (start + batch).min(n);
        let values: Vec<String> = (start..end).map(|i| format!("({i}, 'row_{i}')")).collect();
        eng.ex
            .execute(
                &format!("INSERT INTO source_rows VALUES {}", values.join(",")),
                &BindMap::new(),
            )
            .unwrap();
    }
    eng
}

fn row_count(eng: &Engine) -> i64 {
    let mut s = eng
        .ex
        .query("SELECT COUNT(*) FROM target_rows", &BindMap::new())
        .unwrap();
    let rows = s.fetch_many(1);
    match &rows[0][0] {
        SpinValue::Int(n) => *n,
        _ => panic!("unexpected count type"),
    }
}

#[test]
fn should_match_row_by_row_output_at_scale() {
    let n = 10_000;
    let mut baseline = setup(n);
    baseline.register(COPY).unwrap();
    let t0 = Instant::now();
    baseline.call("copy_rows", &[]).unwrap();
    let baseline_ms = t0.elapsed().as_millis();

    let mut optimized = setup(n);
    optimized.register(COPY).unwrap();
    optimized.set_oriented = SetOrientedMode::Strict;
    let t1 = Instant::now();
    optimized.call("copy_rows", &[]).unwrap();
    let optimized_ms = t1.elapsed().as_millis();

    assert_eq!(row_count(&baseline), n as i64);
    assert_eq!(row_count(&optimized), n as i64);
    assert!(
        matches!(
            optimized.set_oriented_reports()[0].status,
            spinduck_core::ir::set_oriented::Status::Converted { .. }
        ),
        "性能对比必须走集合化路径：{:?}",
        optimized.set_oriented_reports()[0]
    );

    let mut s1 = baseline
        .ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id",
            &BindMap::new(),
        )
        .unwrap();
    let mut s2 = optimized
        .ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id",
            &BindMap::new(),
        )
        .unwrap();
    assert_eq!(s1.fetch_many(n), s2.fetch_many(n));

    eprintln!(
        "[perf] {n} rows: baseline={baseline_ms}ms optimized={optimized_ms}ms speedup={:.1}x",
        baseline_ms as f64 / optimized_ms.max(1) as f64
    );
}
