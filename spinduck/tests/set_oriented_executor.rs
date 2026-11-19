//! 集合化执行接缝的 SQL、绑定、事务与无回放守护。

use spinduck_core::exec::{ExecOutcome, FakeExecutor, RowStream, SqlExecutor};
use spinduck_core::{BindMap, Engine, EngineError, RuleRegistry, SpinValue};
use std::rc::Rc;

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id FROM source_rows LOOP \
    INSERT INTO target_rows(id) VALUES (r.id); END LOOP; END;";

#[cfg(feature = "native")]
struct RecordedNative {
    inner: spinduck_core::DuckExecutor,
    calls: Rc<std::cell::RefCell<Vec<(String, BindMap)>>>,
}

#[cfg(feature = "native")]
impl SqlExecutor for RecordedNative {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.inner.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.calls
            .borrow_mut()
            .push((sql.to_string(), binds.clone()));
        self.inner.execute(sql, binds)
    }
}

#[cfg(feature = "native")]
#[test]
fn should_not_commit_the_batch_when_every_source_row_is_filtered() {
    use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
    let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut eng = Engine::new(
        Box::new(RecordedNative {
            inner: spinduck_core::DuckExecutor::open_in_memory().unwrap(),
            calls: calls.clone(),
        }),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        "CREATE TABLE target_rows(val DECIMAL(15,3))",
        "INSERT INTO source_rows VALUES (1, 0), (0, 0)",
        "CREATE TABLE sys_dummy AS SELECT 1",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(
        "CREATE PROCEDURE copy_filtered IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP BEGIN \
        INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP; END;",
    )
    .unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    calls.borrow_mut().clear();
    eng.call("copy_filtered", &[]).unwrap();
    let Status::Converted { sql } = &eng.set_oriented_reports()[0].status else {
        panic!("{:?}", eng.set_oriented_reports());
    };
    assert_eq!(
        *calls.borrow(),
        vec![
            ("BEGIN TRANSACTION".into(), BindMap::new()),
            (sql.clone(), BindMap::new()),
            ("COMMIT".into(), BindMap::new()),
        ]
    );
    assert_eq!(
        eng.ex
            .query("SELECT val FROM target_rows", &BindMap::new())
            .unwrap()
            .fetch_many(10),
        Vec::<Vec<SpinValue>>::new()
    );
}

struct FailingBulk(Rc<FakeExecutor>);

impl SqlExecutor for FailingBulk {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        let result = self.0.execute(sql, binds)?;
        if sql.starts_with("WITH sd_cur") {
            Err(EngineError::kernel(10, "injected bulk failure"))
        } else {
            Ok(result)
        }
    }
}

#[test]
fn should_rollback_bulk_failure_without_replaying_original_instructions() {
    let recorder = Rc::new(FakeExecutor::default());
    // 证明期与入口守卫各一轮：sys_dummy 存在性 + COUNT(*)=1 + 双表结构
    for _ in 0..2 {
        recorder.push_script(RowStream::new(Vec::new(), vec![vec![SpinValue::Int(1)]]));
        recorder.push_script(RowStream::new(Vec::new(), vec![vec![SpinValue::Int(1)]]));
        for name in ["source_rows", "target_rows"] {
            recorder.push_script(RowStream::new(
                Vec::new(),
                vec![vec![
                    SpinValue::Str("memory".into()),
                    SpinValue::Str("main".into()),
                    SpinValue::Str(name.into()),
                    SpinValue::Str(format!("CREATE TABLE {name}(id BIGINT);")),
                    SpinValue::Bool(true),
                ]],
            ));
        }
    }
    let mut eng = Engine::new(
        Box::new(FailingBulk(recorder.clone())),
        RuleRegistry::default(),
    );
    eng.register(COPY).unwrap();
    eng.set_oriented = spinduck_core::ir::set_oriented::SetOrientedMode::Strict;
    let error = eng.call("copy_rows", &[]).unwrap_err();
    assert_eq!(error.code.0, "KERNEL-0010");
    assert_eq!(error.message, "injected bulk failure");
    let calls = recorder.calls();
    assert_eq!(calls.len(), 11);
    assert_eq!(calls[0], ("BEGIN TRANSACTION".into(), BindMap::new()));
    for base in [1, 5] {
        assert!(calls[base].0.contains("FROM duckdb_tables()"));
        assert!(calls[base].0.contains("FROM duckdb_views()"));
        assert_eq!(calls[base + 1].0, "SELECT COUNT(*) FROM sys_dummy");
        for (call, table) in calls[base + 2..base + 4]
            .iter()
            .zip(["source_rows", "target_rows"])
        {
            assert!(call.0.contains("FROM duckdb_tables()"));
            assert!(call.0.contains("FROM duckdb_indexes()"));
            assert_eq!(
                call.1,
                BindMap::from([("$table".into(), SpinValue::Str(table.into()))])
            );
        }
    }
    assert_eq!(calls[9], (
        "WITH sd_cur AS (SELECT \"id\" FROM \"memory\".\"main\".\"source_rows\") INSERT INTO \"memory\".\"main\".\"target_rows\" (\"id\") SELECT sd_cur.\"id\" FROM sd_cur".into(),
        BindMap::new(),
    ));
    assert_eq!(calls[10], ("ROLLBACK".into(), BindMap::new()));
}

/// M2 §2.1/§5.1-1：原 `routine.prog` 上的多段 `RunSpan` 驱动——区域 0 集合化
/// 后后缀（绝对 `GOTO` + `PushTrap` 异常处理）仍按原 PC 解释执行。
const M2_SPAN_PROC: &str =
    "CREATE OR REPLACE PROCEDURE m2_span(p_mode IN NUMBER, p_mark OUT VARCHAR2) IS \
    v_ct VARCHAR2(10); v_step VARCHAR2(20); \
    BEGIN \
    p_mark := 'start'; \
    FOR r IN SELECT a, fund FROM source_rows LOOP \
    BEGIN \
    SELECT commision_type INTO v_ct FROM par_fund_seat_para \
    WHERE fund_code = r.fund AND ROWNUM = 1 ORDER BY seat_no; \
    EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
    BEGIN \
    INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
    COMMIT; \
    EXCEPTION WHEN OTHERS THEN NULL; END; \
    END LOOP; \
    v_step := 'pre'; \
    IF p_mode = 1 THEN GOTO err_path; END IF; \
    GOTO trap_path; \
    v_step := 'dead'; \
    <<trap_path>> \
    BEGIN \
    SELECT 'x' INTO v_step FROM source_rows WHERE a = -999; \
    EXCEPTION WHEN OTHERS THEN v_step := 'caught'; END; \
    v_step := v_step || '+post'; \
    IF p_mode = 2 THEN \
    SELECT 'x' INTO v_step FROM source_rows WHERE a = -999; \
    END IF; \
    p_mark := v_step; \
    RETURN; \
    <<err_path>> \
    p_mark := 'errpath'; \
    RETURN; END;";

fn setup_m2_span(seat_seeds: &[&str]) -> Engine {
    let mut eng = Engine::new(
        Box::new(spinduck_core::DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_seat_para(fund_code VARCHAR, seat_no BIGINT, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ]
    .iter()
    .copied()
    .chain(seat_seeds.iter().copied())
    {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(M2_SPAN_PROC).unwrap();
    eng
}

fn m2_target_rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

/// 入口守卫查询失败注入：`sd_ties` 只出现在 entry_ok 的 Top-1 并列一致性
/// 守卫 SQL 中（计划期只生成文本不执行），命中即返回内核错误。
struct FailGuardQuery {
    inner: spinduck_core::DuckExecutor,
    seen: Rc<std::cell::RefCell<Vec<String>>>,
}

impl SqlExecutor for FailGuardQuery {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.seen.borrow_mut().push(sql.to_string());
        if sql.contains("sd_ties") {
            return Err(EngineError::kernel(10, "injected entry guard failure"));
        }
        self.inner.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.inner.execute(sql, binds)
    }
}

#[cfg(feature = "native")]
#[test]
fn should_drive_multiple_regions_with_run_span_and_preserve_suffix_absolute_gotos() {
    use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};

    // 边界 ①：batch_commit = false。区域 0 集合化后当场 flow::tx(true)
    // 提交；后缀 p_mode=2 的 SELECT INTO 无行 → 未捕获 NO_DATA_FOUND →
    // 外层 call_with_sink ROLLBACK；区域 0 已提交行必须保留。
    let mut eng = setup_m2_span(&[
        "INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A')",
        "INSERT INTO par_fund_seat_para VALUES ('F2', 1, 'B')",
    ]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let err = eng
        .call("m2_span", &[SpinValue::Int(2), SpinValue::Null])
        .unwrap_err();
    assert_eq!(err.code.0, "BIZ-0100");
    assert_eq!(
        m2_target_rows(&eng),
        vec![
            vec![SpinValue::Int(10), SpinValue::Str("A".into())],
            vec![SpinValue::Int(20), SpinValue::Str("B".into())],
        ],
        "区域 0 当场提交的行必须在后缀回滚后保留"
    );
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()
    );

    // 边界 ②：batch_commit = true。区域 0 只发 TxCommit trace、不当场
    // flow::tx；后缀同一未捕获错误回滚时区域 0 一并回滚（对齐原循环
    // --batch-commit 语义）。
    let mut eng = setup_m2_span(&[
        "INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A')",
        "INSERT INTO par_fund_seat_para VALUES ('F2', 1, 'B')",
    ]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.batch_commit = true;
    let err = eng
        .call("m2_span", &[SpinValue::Int(2), SpinValue::Null])
        .unwrap_err();
    assert_eq!(err.code.0, "BIZ-0100");
    assert!(
        m2_target_rows(&eng).is_empty(),
        "batch_commit 下后缀回滚必须连带撤掉区域 0 的写入"
    );
    assert!(matches!(
        eng.set_oriented_reports()[0].status,
        Status::Converted { .. }
    ));

    // 边界 ③a：entry_ok 外层 Err(EngineError) 直接上抛，绝不落入
    // SkipReason 退回分支（退回会把原区域解释完并让 p_mode=0 成功返回）。
    let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut eng = Engine::new(
        Box::new(FailGuardQuery {
            inner: spinduck_core::DuckExecutor::open_in_memory().unwrap(),
            seen: seen.clone(),
        }),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_seat_para(fund_code VARCHAR, seat_no BIGINT, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
        "INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A')",
        "INSERT INTO par_fund_seat_para VALUES ('F2', 1, 'B')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(M2_SPAN_PROC).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let err = eng
        .call("m2_span", &[SpinValue::Int(0), SpinValue::Null])
        .unwrap_err();
    assert!(
        err.message.contains("injected entry guard failure"),
        "entry_ok 内核错误必须原样上抛: {err:?}"
    );
    assert!(m2_target_rows(&eng).is_empty());

    // 边界 ③b：仅内层 Ok(Err(SkipReason))（Top-1 异值并列）才退回原
    // 区域 [open_pc .. close_pc + 1)；退回后后缀绝对 GOTO 与异常处理
    // 仍按原 PC 生效（p_mark = 'caught+post' 证明 PushTrap 处理器与
    // GOTO trap_path 都走到了）。
    let seeds = [
        "INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A')",
        "INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'B')",
    ];
    let mut baseline = setup_m2_span(&seeds);
    let expected_mark = baseline
        .call("m2_span", &[SpinValue::Int(0), SpinValue::Null])
        .unwrap()
        .outs
        .get("p_mark")
        .cloned();
    let expected_rows = m2_target_rows(&baseline);

    let mut eng = setup_m2_span(&seeds);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let result = eng
        .call("m2_span", &[SpinValue::Int(0), SpinValue::Null])
        .unwrap();
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
    assert_eq!(
        result.outs.get("p_mark"),
        expected_mark.as_ref(),
        "退回原区域后后缀的 GOTO 与 WHEN OTHERS 处理器必须仍按原 PC 生效"
    );
    assert_eq!(
        m2_target_rows(&eng),
        expected_rows,
        "退回路径按原循环逐行解释，结果须与 Off 基线逐行一致"
    );
}

/// M2 §2.2-2/§5.1-2：嵌套静态游标（内层 `OpenCursor` 落在外层
/// `[open_pc .. close_pc]` 区间内、外层 `CLOSE` 之前）——外层区域不
/// `Converted`，内层也不单独转换；`BatchAtomic` 与 `Off` 的结果行
/// 与 `OUT` 逐值一致。
const NESTED_CURSOR_PROC: &str =
    "CREATE OR REPLACE PROCEDURE nested_cursors(p_mark OUT VARCHAR2) IS BEGIN \
    FOR o IN SELECT a FROM source_rows LOOP \
    FOR i IN SELECT b FROM inner_rows LOOP \
    INSERT INTO target_rows(val, tag) VALUES (o.a, i.b); \
    END LOOP; END LOOP; \
    p_mark := 'done'; END;";

fn setup_nested_cursor() -> Engine {
    let mut eng = Engine::new(
        Box::new(spinduck_core::DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT)",
        "INSERT INTO source_rows VALUES (1), (2)",
        "CREATE TABLE inner_rows(b VARCHAR)",
        "INSERT INTO inner_rows VALUES ('x'), ('y')",
        "CREATE TABLE target_rows(val BIGINT, tag VARCHAR)",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(NESTED_CURSOR_PROC).unwrap();
    eng
}

fn nested_target_rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, tag FROM target_rows ORDER BY val, tag",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[cfg(feature = "native")]
#[test]
fn should_reject_outer_and_inner_when_static_cursors_are_nested() {
    use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};

    // Off 基线：两层静态游标按原解释器逐行执行（笛卡尔积 2×2=4 行）。
    let mut baseline = setup_nested_cursor();
    let expected_mark = baseline
        .call("nested_cursors", &[SpinValue::Null])
        .unwrap()
        .outs
        .get("p_mark")
        .cloned();
    let expected_rows = nested_target_rows(&baseline);
    assert_eq!(expected_rows.len(), 4, "Off 基线应产出 4 行");

    // BatchAtomic：嵌套静态游标不转换——报告中不得出现任何
    // Converted（外层区域与落在其 PC 区间内的内层 OpenCursor 均不
    // 单独转换），且结果与 Off 逐值一致。
    let mut eng = setup_nested_cursor();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let result = eng.call("nested_cursors", &[SpinValue::Null]).unwrap();
    assert_eq!(
        eng.set_oriented_reports().len(),
        1,
        "嵌套静态游标应整体拒绝: {:?}",
        eng.set_oriented_reports()
    );
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        },
        "extract 见到第二个静态 OpenCursor 即拒绝整个例程"
    );
    assert_eq!(result.outs.get("p_mark"), expected_mark.as_ref());
    assert_eq!(
        nested_target_rows(&eng),
        expected_rows,
        "BatchAtomic 与 Off 的结果行必须逐值一致"
    );
}
