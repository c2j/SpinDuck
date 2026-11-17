//! 静态游标集合化的逐行基线与等价守护。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
    END LOOP; END;";

fn engine() -> Engine {
    engine_with_target("CREATE TABLE target_rows(id BIGINT, payload VARCHAR)")
}

fn engine_with_target(target: &str) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
        target,
        "INSERT INTO source_rows VALUES (7, 'a'), (7, 'a'), (NULL, NULL)",
        // 引擎自备表（与 verify/deploy.rs 部署口径一致）：入口守卫要求恰 1 行
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id NULLS LAST, payload",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

fn expected() -> Vec<Vec<SpinValue>> {
    vec![
        vec![SpinValue::Int(7), SpinValue::Str("a".into())],
        vec![SpinValue::Int(7), SpinValue::Str("a".into())],
        vec![SpinValue::Null, SpinValue::Null],
    ]
}

#[derive(Default)]
struct Trace(Vec<spinduck_core::exec::TraceEvent>);

impl spinduck_core::exec::TraceSink for Trace {
    fn on_event(&mut self, event: spinduck_core::exec::TraceEvent) {
        self.0.push(event);
    }
}

#[path = "set_oriented_schema.rs"]
mod schema;

#[path = "set_oriented_modes.rs"]
mod modes;

#[path = "set_oriented_inline.rs"]
mod inline;

#[path = "set_oriented_transactions.rs"]
mod transactions;
