//! §5.1-9 V3-5 第 4 点：INSERT 处理器调非 NoOp `inst_log` 时，
//! 坏行数必须为 0 才允许集合化（bad_rows==0 入口守卫，处理器不可达）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::value::SpinValue;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry};
use std::collections::BTreeMap;

const DDL: &[&str] = &[
    "CREATE TABLE cur_rows(a DECIMAL(20,6), note VARCHAR, tag VARCHAR)",
    "CREATE TABLE dst_ovf(val DECIMAL(15,3), tag VARCHAR, extra VARCHAR DEFAULT 'd')",
    "CREATE TABLE dst_nn(note VARCHAR NOT NULL, tag VARCHAR, extra INT DEFAULT 7)",
    "CREATE TABLE inst_log_log(msg VARCHAR)",
    "CREATE TABLE sys_dummy AS SELECT 1 AS x",
];

/// inst_log 是真实写日志表的例程（目录摘要 Impure，非 NoOp）。
const INST_LOG: &str = "CREATE OR REPLACE PROCEDURE inst_log(p_msg VARCHAR2) IS
  BEGIN
    INSERT INTO inst_log_log(msg) VALUES (p_msg);
  END;";

/// numeric 溢出夹具：a DECIMAL(20,6) 写入 DECIMAL(15,3)，超限行触发异常。
const CALLER_OVF: &str = "CREATE OR REPLACE PROCEDURE caller_ovf(p_o_succeed OUT VARCHAR2) IS
  BEGIN
    p_o_succeed := '0';
    FOR r IN SELECT a, note, tag FROM cur_rows LOOP
      BEGIN
        INSERT INTO dst_ovf(val, tag) VALUES (r.a, r.tag);
      EXCEPTION WHEN OTHERS THEN
        inst_log(r.tag);
        p_o_succeed := 'LOGGED';
      END;
    END LOOP;
  END;";

/// NOT NULL 违例夹具：note 为 NULL 的行写入 NOT NULL 列（违例列即
/// 本条 INSERT 写入列）。
const CALLER_NN: &str = "CREATE OR REPLACE PROCEDURE caller_nn(p_o_succeed OUT VARCHAR2) IS
  BEGIN
    p_o_succeed := '0';
    FOR r IN SELECT a, note, tag FROM cur_rows LOOP
      BEGIN
        INSERT INTO dst_nn(note, tag) VALUES (r.note, r.tag);
      EXCEPTION WHEN OTHERS THEN
        inst_log(r.tag);
        p_o_succeed := 'LOGGED';
      END;
    END LOOP;
  END;";

const ROUTINES: &[&str] = &[INST_LOG, CALLER_OVF, CALLER_NN];

fn engine_with(ddl_rows: &[&str], oriented: SetOrientedMode) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in DDL {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    for sql in ddl_rows {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    for sql in ROUTINES {
        eng.register(sql).unwrap();
    }
    eng.set_oriented = oriented;
    eng
}

fn rows(eng: &Engine, sql: &str) -> Vec<Vec<SpinValue>> {
    let mut stream = eng.ex.query(sql, &BindMap::new()).unwrap();
    let mut out = Vec::new();
    loop {
        let chunk = stream.fetch_many(1024);
        if chunk.is_empty() {
            break;
        }
        out.extend(chunk);
    }
    out
}

fn state(eng: &Engine, dst: &str) -> (Vec<Vec<SpinValue>>, Vec<Vec<SpinValue>>) {
    (
        rows(eng, &format!("SELECT * FROM {dst} ORDER BY tag")),
        rows(eng, "SELECT msg FROM inst_log_log ORDER BY msg"),
    )
}

fn run_pair(
    routine: &str,
    seed: &str,
) -> (
    Engine,
    Engine,
    BTreeMap<String, SpinValue>,
    BTreeMap<String, SpinValue>,
) {
    let rows_ddl = [seed];
    let mut off = engine_with(&rows_ddl, SetOrientedMode::Off);
    let expected = off.call(routine, &[SpinValue::Null]).unwrap();
    let mut eng = engine_with(&rows_ddl, SetOrientedMode::BatchAtomic);
    let actual = eng.call(routine, &[SpinValue::Null]).unwrap();
    (eng, off, expected.outs, actual.outs)
}

fn assert_same_state(eng: &Engine, off: &Engine, dst: &str) {
    let (d1, l1) = state(eng, dst);
    let (d2, l2) = state(off, dst);
    assert_eq!(d1, d2, "目标表须与 Off 一致");
    assert_eq!(l1, l2, "日志表须与 Off 一致（坏行不得被滤掉）");
}

/// 坏行存在时必须经 bad_rows 入口守卫退回（UQTN = 守卫计数非零），
/// 而非计划期拒绝——后者无法证明守卫 SQL 覆盖了写入列。
fn not_converted(eng: &Engine) {
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnprovenQueryTotalness
            }
        ),
        "坏行须经 bad_rows 守卫退回: {:?}",
        eng.set_oriented_reports()
    );
}

#[test]
fn should_require_zero_bad_rows_when_inst_log_is_non_noop() {
    scenario_not_null_violation();
    scenario_numeric_overflow();
    scenario_zero_bad_rows();
}

/// ① 目标列 NOT NULL 违例（违例列 note 是本条 INSERT 写入列）：
/// 不得 Converted，日志表/目标表/p_o_succeed 与 Off 一致。
fn scenario_not_null_violation() {
    let seed = "INSERT INTO cur_rows VALUES \
        (1.5, NULL, 'a'), (3.0, 'n2', 'b'), (5.0, NULL, 'c')";
    let (eng, off, expected_outs, actual_outs) = run_pair("caller_nn", seed);
    not_converted(&eng);
    assert_same_state(&eng, &off, "dst_nn");
    assert_eq!(actual_outs, expected_outs, "p_o_succeed 须与 Off 一致");
    let (_, log) = state(&off, "dst_nn");
    assert!(!log.is_empty(), "Off 侧坏行必须真实触发过日志处理器");
}

/// ② 写入值超出目标 numeric(p,s)（a 超出 DECIMAL(15,3) 值域）：
/// 同样不得 Converted，日志、目标表、p_o_succeed 与 Off 一致。
fn scenario_numeric_overflow() {
    let seed = "INSERT INTO cur_rows VALUES \
        (1.5, 'n1', 'a'), (99999999999999.999999, 'n2', 'b'), (2.5, 'n3', 'c')";
    let (eng, off, expected_outs, actual_outs) = run_pair("caller_ovf", seed);
    not_converted(&eng);
    assert_same_state(&eng, &off, "dst_ovf");
    assert_eq!(actual_outs, expected_outs);
    let (_, log) = state(&off, "dst_ovf");
    assert!(!log.is_empty(), "Off 侧溢出行必须真实触发过日志处理器");
}

/// ③ 零坏行：Converted，日志表 0 行（处理器未跑），目标表与
/// p_o_succeed 和 Off 一致。
fn scenario_zero_bad_rows() {
    let seed = "INSERT INTO cur_rows VALUES \
        (1.5, 'n1', 'a'), (3.0, 'n2', 'b'), (2.5, 'n3', 'c')";
    let (eng, off, expected_outs, actual_outs) = run_pair("caller_ovf", seed);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "零坏行必须 Converted: {:?}",
        eng.set_oriented_reports()
    );
    assert_same_state(&eng, &off, "dst_ovf");
    let (_, log) = state(&eng, "dst_ovf");
    assert!(log.is_empty(), "集合化下处理器不可达，日志必须为 0 行");
    assert_eq!(actual_outs, expected_outs, "p_o_succeed 不得因集合化改变");
}
