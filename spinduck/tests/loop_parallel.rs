//! loop-parallel 端到端守护（设计 §7.2 矩阵的首批切片）：
//! Safe 模式接管游标循环、threads 1/4 结果等价、min_rows 回退串行。
#![cfg(feature = "native")]

use spinduck_core::ir::loop_parallel::{LoopParallelConfig, LoopParallelMode, ParallelStatus};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

/// 纯前缀（5 条赋值）+ 单行写后缀的最小可并行游标循环。
const PROC: &str = "CREATE PROCEDURE p_copy IS \
    v1 NUMBER; v2 NUMBER; v3 NUMBER; v4 NUMBER; v5 NUMBER; \
    BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
        v1 := r.id + 1; \
        v2 := v1 * 2; \
        v3 := v2 - 1; \
        v4 := v3 + r.id; \
        v5 := v4 + 0; \
        INSERT INTO target_rows(id, payload) VALUES (v5, r.payload); \
    END LOOP; \
    END;";

fn engine(mode: LoopParallelMode, threads: usize, chunk: usize, min_rows: usize) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, payload VARCHAR)",
        "INSERT INTO source_rows VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(PROC).unwrap();
    eng.loop_parallel = LoopParallelConfig {
        mode,
        threads,
        chunk_size: chunk,
        min_rows,
    };
    eng
}

fn target_rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

/// v5 = ((id+1)*2 - 1) + id = 3*id + 1。
fn expected() -> Vec<Vec<SpinValue>> {
    ["a", "b", "c", "d", "e"]
        .iter()
        .enumerate()
        .map(|(i, p)| {
            vec![
                SpinValue::Int(3 * (i as i64 + 1) + 1),
                SpinValue::Str((*p).to_string()),
            ]
        })
        .collect()
}

#[test]
fn should_parallelize_cursor_loop_with_identical_results() {
    let mut eng = engine(LoopParallelMode::Safe, 2, 2, 2);
    eng.call("p_copy", &[]).unwrap();
    assert_eq!(target_rows(&eng), expected());
    let reports = eng.loop_parallel_reports();
    assert_eq!(reports.len(), 1, "reports: {reports:?}");
    match &reports[0].status {
        ParallelStatus::Parallelized {
            rows_dispatched,
            rows_fallback_serial,
            ..
        } => {
            assert_eq!(*rows_dispatched, 5);
            assert_eq!(*rows_fallback_serial, 0);
        }
        other => panic!("应为 Parallelized，实际: {other:?}"),
    }
}

#[test]
fn should_match_serial_output_for_threads_1_and_4() {
    let mut outs = Vec::new();
    for threads in [1usize, 4] {
        let mut eng = engine(LoopParallelMode::Safe, threads, 2, 2);
        eng.call("p_copy", &[]).unwrap();
        outs.push(target_rows(&eng));
        assert!(matches!(
            eng.loop_parallel_reports()[0].status,
            ParallelStatus::Parallelized { .. }
        ));
    }
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0], expected());
}

#[test]
fn should_fall_back_serial_below_min_rows() {
    let mut eng = engine(LoopParallelMode::Safe, 4, 2, 100);
    eng.call("p_copy", &[]).unwrap();
    assert_eq!(target_rows(&eng), expected());
    assert!(matches!(
        eng.loop_parallel_reports()[0].status,
        ParallelStatus::Skipped {
            reason: spinduck_core::ir::loop_parallel::ParallelSkipReason::InsufficientParallelWork
        }
    ));
}

#[test]
fn should_stay_serial_when_mode_off() {
    let mut eng = engine(LoopParallelMode::Off, 4, 2, 2);
    eng.call("p_copy", &[]).unwrap();
    assert_eq!(target_rows(&eng), expected());
    assert!(eng.loop_parallel_reports().is_empty());
}

/// SO+LP 派发缺口守护：purchase kind 开关同款形态——循环内不变量
/// 查找变量驱动 IF 路由（THEN 臂含不可集合化语句），SO 计划期接受、
/// 入口路由守卫运行态拒绝时，loop-parallel 必须接管同一区域
/// （前缀已执行绝不重放），而非整区域退回串行。
const ROUTE_PROC: &str = "CREATE PROCEDURE p_route IS \
    v_kind VARCHAR; v_fee VARCHAR; \
    BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN \
            SELECT kind_id INTO v_kind FROM switches WHERE operation_kind = 'SW'; \
        EXCEPTION WHEN OTHERS THEN v_kind := '0'; END; \
        BEGIN \
            IF v_kind = '1' THEN \
                v_fee := 'T'; \
            ELSE \
                SELECT fee INTO v_fee FROM fee_tbl WHERE fee_tbl.id = r.id; \
            END IF; \
        EXCEPTION WHEN OTHERS THEN v_fee := 'D'; END; \
        INSERT INTO target_rows(id, fee) VALUES (r.id, v_fee); \
    END LOOP; END;";

fn route_engine(kind: &str, lp: LoopParallelMode) -> Engine {
    use spinduck_core::ir::set_oriented::SetOrientedMode;
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, fee VARCHAR)",
        "CREATE TABLE switches(operation_kind VARCHAR, kind_id VARCHAR)",
        "CREATE TABLE fee_tbl(id BIGINT, fee VARCHAR)",
        "CREATE TABLE sys_dummy AS SELECT 1",
        "INSERT INTO source_rows VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')",
        "INSERT INTO fee_tbl VALUES (1,'f1'),(2,'f2'),(3,'f3'),(4,'f4'),(5,'f5')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.ex
        .execute(
            &format!("INSERT INTO switches VALUES ('SW', '{kind}')"),
            &BindMap::new(),
        )
        .unwrap();
    eng.register(ROUTE_PROC).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.loop_parallel = LoopParallelConfig {
        mode: lp,
        threads: 2,
        chunk_size: 2,
        min_rows: 2,
    };
    eng
}

fn route_fees(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, fee FROM target_rows ORDER BY id",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

fn fee_rows(fees: &[&str]) -> Vec<Vec<SpinValue>> {
    fees.iter()
        .enumerate()
        .map(|(i, f)| vec![SpinValue::Int(i as i64 + 1), SpinValue::Str((*f).into())])
        .collect()
}

#[test]
fn should_parallelize_region_when_route_guard_rejects_at_runtime() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    let mut eng = route_engine("1", LoopParallelMode::Safe);
    eng.call("p_route", &[]).unwrap();
    // THEN 臂生效：v_fee 恒为 'T'
    assert_eq!(route_fees(&eng), fee_rows(&["T", "T", "T", "T", "T"]));
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        }
    ));
    assert!(
        matches!(
            eng.loop_parallel_reports()[0].status,
            ParallelStatus::Parallelized {
                rows_dispatched: 5,
                rows_fallback_serial: 0,
                ..
            }
        ),
        "lp reports: {:?}",
        eng.loop_parallel_reports()
    );
}

#[test]
fn should_convert_when_route_guard_passes() {
    use spinduck_core::ir::set_oriented::Status;
    let mut eng = route_engine("0", LoopParallelMode::Safe);
    eng.call("p_route", &[]).unwrap();
    // ELSE 臂生效：v_fee 逐行取自 fee_tbl
    assert_eq!(route_fees(&eng), fee_rows(&["f1", "f2", "f3", "f4", "f5"]));
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));
    assert!(eng.loop_parallel_reports().is_empty());
}

#[test]
fn should_stay_serial_when_both_optimizations_off() {
    let mut eng = route_engine("1", LoopParallelMode::Off);
    eng.set_oriented = spinduck_core::ir::set_oriented::SetOrientedMode::Off;
    eng.call("p_route", &[]).unwrap();
    assert_eq!(route_fees(&eng), fee_rows(&["T", "T", "T", "T", "T"]));
    assert!(eng.loop_parallel_reports().is_empty());
    assert!(eng.set_oriented_reports().is_empty());
}
