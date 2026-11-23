//! 维表 SqlQuery 关系化守护（V2 设计 §4.4 B1）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{Arm, SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT)",
        "CREATE TABLE target_rows(val BIGINT)",
        "INSERT INTO source_rows VALUES (10), (20), (30)",
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
            "SELECT val FROM target_rows ORDER BY rowid",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[path = "set_oriented_lookup_cardinality.rs"]
mod cardinality;

#[path = "set_oriented_lookup_top1.rs"]
mod top1;

#[path = "set_oriented_lookup_fallback.rs"]
mod fallback;

#[path = "set_oriented_lookup_lateral.rs"]
mod lateral;

#[path = "set_oriented_lookup_dependency.rs"]
mod dependency;

#[path = "set_oriented_lookup_inline.rs"]
mod inline;
