//! PR #48 论文视角能力边界验证探针（归档复现资产，见 golden/pr48_set_oriented_verify/README.md）。
//!
//! 本文件依赖 PR #48（head f62787d）引入的 `spinduck_core::ir::set_oriented` 模块，
//! 不能在主分支编译。复现时由 run_verify.sh 注入到 PR 检出的 spinduck/tests/ 下运行。
//!
//! 方法论对照 docs/performance-optimization/docs/set-orientation-papers：
//! - 路线一（Froid/QBS 模式改写）：表达式/分支/查询形态的准入边界；
//! - 路线二（Aggify 状态依赖）：跨轮累加、OUT 写回、显式 FETCH INTO 的拒绝；
//! - 路线三（CIDR2024 Batching/DBridge 副作用）：事务/调用/动态 SQL/异常/组合开关。
//! 每个变体在相同独立快照上分别跑 OFF/ON，逐值比对业务观察面（行 bag、OUT、错误码+消息），
//! 并断言 ON 的 Converted/Skipped 原因。性能探针单独度量 RBAR 消除方向。
#![cfg(feature = "native")]

use spinduck_core::exec::{TraceEvent, TraceSink};
use spinduck_core::ir::set_oriented::{SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};
use std::cell::RefCell;

const SRC_DDL: &str = "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)";
const SRC_DATA: &str = "INSERT INTO source_rows VALUES (7, 'a'), (7, 'a'), (NULL, NULL)";
const TGT_DDL: &str = "CREATE TABLE target_rows(id BIGINT, payload VARCHAR)";

const COPY: &str = "CREATE PROCEDURE copy_rows IS BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
    END LOOP; END;";

thread_local! {
    static FAILS: RefCell<Vec<String>> = RefCell::new(Vec::new());
}

fn record_fail(msg: String) {
    println!("FAIL {msg}");
    FAILS.with(|f| f.borrow_mut().push(msg));
}

fn assert_empty_fails(context: &str) {
    FAILS.with(|f| {
        let fails = f.borrow();
        assert!(fails.is_empty(), "{context} 共 {} 项不符：\n{}", fails.len(), fails.join("\n"));
    });
}

fn big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[derive(Debug, PartialEq)]
struct Obs {
    rows: Vec<String>,
    outs: Vec<String>,
    err: Option<(String, String)>,
}

fn observe(eng: &Engine, res: Result<spinduck_core::CallResult, spinduck_core::EngineError>) -> Obs {
    let (outs, err) = match res {
        Ok(r) => (
            r.outs.iter().map(|(k, v)| format!("{k}={v:?}")).collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some((e.code.0.clone(), e.message.clone()))),
    };
    let mut rows: Vec<String> = eng
        .ex
        .query("SELECT * FROM target_rows", &BindMap::new())
        .unwrap()
        .fetch_many(1_000_000)
        .iter()
        .map(|r| format!("{r:?}"))
        .collect();
    rows.sort();
    Obs { rows, outs, err }
}

#[derive(Debug)]
enum Expect {
    Converted,
    Skipped(SkipReason),
}

fn setup(target_ddl: &str, source: Option<(&str, &str)>, extra: &[&str]) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    if let Some((ddl, data)) = source {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
        eng.ex.execute(data, &BindMap::new()).unwrap();
    }
    eng.ex.execute(target_ddl, &BindMap::new()).unwrap();
    for sql in extra {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

/// 通用差分：相同快照 OFF/ON 各跑一次，比对观察面并断言 ON 报告。失败收集不中断。
fn diff_full(
    name: &str,
    proc_sql: &str,
    target_ddl: &str,
    source: Option<(&str, &str)>,
    extra: &[&str],
    args: &[SpinValue],
    expect: Expect,
) {
    let mut off = setup(target_ddl, source, extra);
    if let Err(e) = off.register(proc_sql) {
        record_fail(format!("[{name}] OFF register 失败: {e:?}"));
        return;
    }
    let off_res = off.call("copy_rows", args);
    let off_obs = observe(&off, off_res);
    if !off.set_oriented_reports().is_empty() {
        record_fail(format!("[{name}] OFF 不应有报告"));
    }

    let mut on = setup(target_ddl, source, extra);
    if let Err(e) = on.register(proc_sql) {
        record_fail(format!("[{name}] ON register 失败: {e:?}"));
        return;
    }
    on.set_oriented = true;
    let on_res = on.call("copy_rows", args);
    let on_obs = observe(&on, on_res);

    if off_obs != on_obs {
        record_fail(format!("[{name}] OFF/ON 观察面不一致\n  OFF={off_obs:?}\n  ON ={on_obs:?}"));
    }
    let Some(report) = on.set_oriented_reports().first() else {
        record_fail(format!("[{name}] ON 报告为空（调用在选择产物前失败）"));
        return;
    };
    let ok = match &expect {
        Expect::Converted => matches!(&report.status, Status::Converted { sql } if sql.starts_with("WITH sd_cur AS (")),
        Expect::Skipped(reason) => report.status == Status::Skipped { reason: reason.clone() },
    };
    if ok {
        println!("PASS [{name}] -> {:?}", expect);
    } else {
        record_fail(format!("[{name}] 期望 {expect:?}，实际 {:?}", report.status));
    }
}

fn diff(name: &str, proc_sql: &str, expect: Expect) {
    diff_full(name, proc_sql, TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[], &[], expect);
}

fn skipped(name: &str, proc_sql: &str, reason: SkipReason) {
    diff(name, proc_sql, Expect::Skipped(reason));
}

/// 特征化：只断言 OFF/ON 等价不变量，打印实际选择（用于准入边界未知的形态）。
fn characterize(
    name: &str,
    proc_sql: &str,
    target_ddl: &str,
    source: Option<(&str, &str)>,
    args: &[SpinValue],
) {
    let mut off = setup(target_ddl, source, &[]);
    if let Err(e) = off.register(proc_sql) {
        println!("CHAR [{name}] 语法未被接受（register 失败）: {e:?}");
        return;
    }
    let off_res = off.call("copy_rows", args);
    let off_obs = observe(&off, off_res);
    let mut on = setup(target_ddl, source, &[]);
    on.register(proc_sql).unwrap();
    on.set_oriented = true;
    let on_res = on.call("copy_rows", args);
    let on_obs = observe(&on, on_res);
    let status = &on.set_oriented_reports()[0].status;
    if off_obs == on_obs {
        println!("CHAR [{name}] 等价 OK，选择={status:?}");
    } else {
        record_fail(format!(
            "[{name}] 等价被破坏！选择={status:?}\n  OFF={off_obs:?}\n  ON ={on_obs:?}"
        ));
    }
}

/// 路线一 + 路线二 + 路线三 的准入边界矩阵。
#[test]
fn probe_boundary_matrix() {
    big_stack(|| {
        use SkipReason::*;

        // ---- 路线一：Froid/QBS 模式边界（表达式 / 查询形态 / 分支）----
        skipped("select_expression",
            &COPY.replace("SELECT id, payload", "SELECT id + 1, payload"), UnprovenExpression);
        skipped("select_where",
            &COPY.replace("FROM source_rows", "FROM source_rows WHERE id > 0"), UnprovenExpression);
        skipped("select_alias",
            &COPY.replace("SELECT id, payload", "SELECT id AS x, payload"), UnprovenExpression);
        skipped("select_qualified",
            &COPY.replace("SELECT id, payload FROM source_rows", "SELECT s.id, s.payload FROM source_rows s"), UnprovenExpression);
        skipped("select_star",
            &COPY.replace("SELECT id, payload", "SELECT *"), UnprovenExpression);
        skipped("select_order_by",
            &COPY.replace("FROM source_rows", "FROM source_rows ORDER BY id"), UnprovenExpression);
        skipped("select_limit",
            &COPY.replace("FROM source_rows", "FROM source_rows LIMIT 2"), UnprovenExpression);
        skipped("select_distinct",
            &COPY.replace("SELECT id, payload", "SELECT DISTINCT id, payload"), UnprovenExpression);
        skipped("select_join",
            &COPY.replace("SELECT id, payload FROM source_rows",
                "SELECT a.id, a.payload FROM source_rows a JOIN source_rows b ON a.id = b.id"), UnprovenExpression);
        skipped("insert_expression",
            &COPY.replace("VALUES (r.id, r.payload)", "VALUES (r.id + 1, r.payload)"), UnsupportedWrite);
        skipped("insert_literal",
            &COPY.replace("VALUES (r.id, r.payload)", "VALUES (1, 'x')"), UnsupportedWrite);
        skipped("if_branch_in_loop",
            &COPY.replace("INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload);",
                "IF r.id IS NOT NULL THEN INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END IF;"),
            UnsupportedControlFlow);
        skipped("select_into_in_loop",
            "CREATE PROCEDURE copy_rows IS v NUMBER := 0; BEGIN \
             FOR r IN SELECT id, payload FROM source_rows LOOP \
             SELECT COUNT(*) INTO v FROM source_rows; \
             INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;",
            UnsupportedControlFlow);

        // ---- 路线二：Aggify 状态依赖（跨轮累加 / OUT 写回 / 显式 FETCH）----
        skipped("loop_carried_accumulator",
            "CREATE PROCEDURE copy_rows IS v NUMBER := 0; BEGIN \
             FOR r IN SELECT id, payload FROM source_rows LOOP \
             v := v + r.id; \
             INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;",
            UnsupportedControlFlow);
        diff_full("out_param_assigned_in_loop",
            "CREATE PROCEDURE copy_rows(x OUT NUMBER) IS BEGIN \
             FOR r IN SELECT id, payload FROM source_rows LOOP \
             x := r.id; \
             INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;",
            TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[], &[SpinValue::Null],
            Expect::Skipped(UnsupportedControlFlow));
        skipped("explicit_fetch_into_scalars",
            "CREATE PROCEDURE copy_rows IS \
             CURSOR c IS SELECT id, payload FROM source_rows; \
             v_id BIGINT; v_payload VARCHAR; \
             BEGIN OPEN c; LOOP FETCH c INTO v_id, v_payload; EXIT WHEN c%NOTFOUND; \
             INSERT INTO target_rows(id, payload) VALUES (v_id, v_payload); END LOOP; CLOSE c; END;",
            UnsupportedControlFlow);

        // ---- 路线三：Batching/DBridge 副作用 / 事务 / 调用 / 动态 ----
        skipped("commit_inside_loop",
            &COPY.replace("END LOOP", "COMMIT; END LOOP"), TransactionBoundaryInsideLoop);
        skipped("call_empty_proc_in_loop",
            "CREATE PROCEDURE log_nop IS BEGIN NULL; END; \
             CREATE PROCEDURE copy_rows IS BEGIN \
             FOR r IN SELECT id, payload FROM source_rows LOOP \
             log_nop(); \
             INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;",
            UnsupportedCall);
        skipped("execute_immediate_in_loop",
            "CREATE PROCEDURE copy_rows IS BEGIN \
             FOR r IN SELECT id, payload FROM source_rows LOOP \
             EXECUTE IMMEDIATE 'INSERT INTO target_rows(id, payload) VALUES (1, ''x'')'; \
             END LOOP; END;",
            DynamicSql);
        skipped("exception_handler_outer",
            &COPY.replace("END LOOP; END;", "END LOOP; EXCEPTION WHEN OTHERS THEN NULL; END;"),
            ExceptionScopePresent);
        skipped("update_instead_of_insert",
            &COPY.replace("INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload);",
                "UPDATE target_rows SET payload = 'x' WHERE id = r.id;"), UnsupportedWrite);
        skipped("delete_instead_of_insert",
            &COPY.replace("INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload);",
                "DELETE FROM target_rows WHERE id = r.id;"), UnsupportedWrite);
        skipped("two_inserts_in_loop",
            &COPY.replace("INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload);",
                "INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); \
                 INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload);"), UnsupportedControlFlow);
        skipped("nested_loop",
            "CREATE PROCEDURE copy_rows IS BEGIN \
             FOR r IN SELECT id, payload FROM source_rows LOOP \
             FOR s IN SELECT id, payload FROM source_rows LOOP \
             INSERT INTO target_rows(id, payload) VALUES (s.id, s.payload); END LOOP; END LOOP; END;",
            UnsupportedControlFlow);
        skipped("source_equals_target",
            &COPY.replace("INSERT INTO target_rows", "INSERT INTO source_rows"), TargetReadDependency);

        // ---- 模式 / 类型 / 结构证明（设计 §3.3，论文的类型保留要求）----
        diff_full("type_mismatch_int", COPY,
            "CREATE TABLE target_rows(id INTEGER, payload VARCHAR)",
            Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("nullable_to_notnull", COPY,
            "CREATE TABLE target_rows(id BIGINT NOT NULL, payload VARCHAR)",
            Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("unique_index", COPY, TGT_DDL, Some((SRC_DDL, SRC_DATA)),
            &["CREATE UNIQUE INDEX target_key ON target_rows(id)"], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("primary_key", COPY,
            "CREATE TABLE target_rows(id BIGINT PRIMARY KEY, payload VARCHAR)",
            Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("check_constraint", COPY,
            "CREATE TABLE target_rows(id BIGINT CHECK(id > 0), payload VARCHAR)",
            Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("default_value", COPY,
            "CREATE TABLE target_rows(id BIGINT, payload VARCHAR DEFAULT 'x')",
            Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("extra_target_column", COPY,
            "CREATE TABLE target_rows(id BIGINT, payload VARCHAR, extra BIGINT)",
            Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Skipped(UnprovenTargetConstraints));
        diff_full("decimal_columns", COPY,
            "CREATE TABLE target_rows(id DECIMAL(18,2), payload VARCHAR)",
            Some(("CREATE TABLE source_rows(id DECIMAL(18,2), payload VARCHAR)",
                  "INSERT INTO source_rows VALUES (7.5, 'a'), (NULL, NULL)")), &[], &[],
            Expect::Skipped(UnknownObjectDependency));
        diff_full("date_columns", COPY,
            "CREATE TABLE target_rows(id DATE, payload VARCHAR)",
            Some(("CREATE TABLE source_rows(id DATE, payload VARCHAR)",
                  "INSERT INTO source_rows VALUES (DATE '2020-01-01', 'a'), (NULL, NULL)")), &[], &[],
            Expect::Skipped(UnknownObjectDependency));
        diff_full("double_columns", COPY,
            "CREATE TABLE target_rows(id DOUBLE, payload VARCHAR)",
            Some(("CREATE TABLE source_rows(id DOUBLE, payload VARCHAR)",
                  "INSERT INTO source_rows VALUES (7.5, 'a'), (NULL, NULL)")), &[], &[],
            Expect::Skipped(UnknownObjectDependency));
        diff_full("view_source", COPY, TGT_DDL,
            Some(("CREATE VIEW source_rows AS SELECT 7::BIGINT AS id, 'a'::VARCHAR AS payload", "SELECT 1")),
            &[], &[], Expect::Skipped(UnknownObjectDependency));
        diff_full("missing_source", COPY, TGT_DDL, None, &[], &[],
            Expect::Skipped(UnknownObjectDependency));
        diff_full("temp_table_source", COPY, TGT_DDL,
            Some(("CREATE TEMP TABLE source_rows(id BIGINT, payload VARCHAR)", SRC_DATA)), &[], &[],
            Expect::Skipped(UnknownObjectDependency));

        // ---- 参数化游标 / 记录槽 ----
        diff_full("parameterized_cursor",
            "CREATE PROCEDURE copy_rows(p_min IN BIGINT) IS BEGIN \
             FOR r IN SELECT id, payload FROM source_rows WHERE id > p_min LOOP \
             INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;",
            TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[], &[SpinValue::Int(0)],
            Expect::Skipped(UnprovenExpression));
        diff_full("record_name_collides_param",
            &COPY.replace("copy_rows IS", "copy_rows(r OUT RECORD) IS"),
            TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[], &[SpinValue::Null],
            Expect::Skipped(LiveOutState));

        assert_empty_fails("边界矩阵");
    });
}

/// 正例：可证明的静态搬运必须 Converted，且 OFF/ON 逐值等价。
#[test]
fn probe_positive_controls() {
    big_stack(|| {
        diff("plain_copy", COPY, Expect::Converted);
        diff("mixed_case_unquoted",
            &COPY.replace("source_rows", "Source_Rows").replace("target_rows", "Target_Rows"),
            Expect::Converted);
        // SELECT 侧小写带引号列名：tokenizer 去引号后与无引号等价，实测 Converted
        // （设计 §8.6 声称"拒绝带引号名字"，对 SELECT 侧小写 quoted 不成立）。
        diff("quoted_select_columns",
            &COPY.replace("SELECT id, payload", "SELECT \"id\", \"payload\""),
            Expect::Converted);
        diff_full("insert_column_reordered",
            &COPY.replace("INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload)",
                "INSERT INTO target_rows(payload, id) VALUES (r.payload, r.id)"),
            TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Converted);
        diff_full("select_extra_unused_column",
            &COPY.replace("INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload)",
                "INSERT INTO target_rows(id) VALUES (r.id)"),
            "CREATE TABLE target_rows(id BIGINT)", Some((SRC_DDL, SRC_DATA)), &[], &[], Expect::Converted);
        diff_full("empty_source", COPY, TGT_DDL,
            Some((SRC_DDL, "SELECT 1")), &[], &[], Expect::Converted);

        // batch_commit 同开：集合化统一拒绝，走 batch 路径，结果仍等价。
        let mut off = setup(TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[]);
        off.register(COPY).unwrap();
        let off_res = off.call("copy_rows", &[]);
        let off_obs = observe(&off, off_res);
        let mut on = setup(TGT_DDL, Some((SRC_DDL, SRC_DATA)), &[]);
        on.register(COPY).unwrap();
        on.set_oriented = true;
        on.batch_commit = true;
        let on_res = on.call("copy_rows", &[]);
        let on_obs = observe(&on, on_res);
        if off_obs != on_obs {
            record_fail(format!("[batch_commit_combo] 观察面不一致 OFF={off_obs:?} ON={on_obs:?}"));
        } else if on.set_oriented_reports()[0].status
            != (Status::Skipped { reason: SkipReason::BatchCommitEnabled })
        {
            record_fail(format!("[batch_commit_combo] 期望 BatchCommitEnabled，实际 {:?}", on.set_oriented_reports()[0].status));
        } else {
            println!("PASS [batch_commit_combo] -> Skipped(BatchCommitEnabled)");
        }
        assert_empty_fails("正例");
    });
}

/// 大小写敏感边界特征化：混合大小写带引号列名是否破坏 OFF/ON 等价。
#[test]
fn probe_case_sensitivity_edges() {
    big_stack(|| {
        // 列名混合大小写（DuckDB 按 quoted 原样存储），过程用 quoted 混合大小写引用。
        characterize(
            "quoted_mixed_case_columns",
            "CREATE PROCEDURE copy_rows IS BEGIN \
             FOR r IN SELECT \"Id\", \"Payload\" FROM source_rows LOOP \
             INSERT INTO target_rows(\"Id\", \"Payload\") VALUES (r.\"Id\", r.\"Payload\"); END LOOP; END;",
            "CREATE TABLE target_rows(\"Id\" BIGINT, \"Payload\" VARCHAR)",
            Some(("CREATE TABLE source_rows(\"Id\" BIGINT, \"Payload\" VARCHAR)", SRC_DATA)),
            &[],
        );
        // 表名混合大小写 quoted，列名小写。
        characterize(
            "quoted_mixed_case_table",
            "CREATE PROCEDURE copy_rows IS BEGIN \
             FOR r IN SELECT id, payload FROM \"Source_Rows\" LOOP \
             INSERT INTO target_rows(id, payload) VALUES (r.id, r.payload); END LOOP; END;",
            TGT_DDL,
            Some(("CREATE TABLE \"Source_Rows\"(id BIGINT, payload VARCHAR)", SRC_DATA)),
            &[],
        );
        // INSERT 侧小写 quoted 列名（SELECT 侧无引号）。
        characterize(
            "quoted_insert_columns",
            &COPY.replace("target_rows(id, payload)", "target_rows(\"id\", \"payload\")"),
            TGT_DDL,
            Some((SRC_DDL, SRC_DATA)),
            &[],
        );
        assert_empty_fails("大小写边界");
    });
}

#[derive(Default)]
struct Trace(Vec<TraceEvent>);
impl TraceSink for Trace {
    fn on_event(&mut self, e: TraceEvent) {
        self.0.push(e);
    }
}

/// RBAR 消除方向：OFF 逐行 N 次 INSERT SqlExec + N+1 次 Fetch；ON 单条 SqlExec、零 Fetch。
#[test]
fn probe_trace_roundtrip_counts() {
    big_stack(|| {
        let n = 1000usize;
        let data = format!("INSERT INTO source_rows SELECT i, 'p' FROM range({n}) t(i)");
        for set in [false, true] {
            let mut eng = setup(TGT_DDL, Some((SRC_DDL, &data)), &[]);
            eng.register(COPY).unwrap();
            eng.set_oriented = set;
            eng.step_budget = 100_000_000;
            let mut trace = Trace::default();
            eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
            let sql_execs = trace.0.iter().filter(|e| matches!(e, TraceEvent::SqlExec { .. })).count();
            let fetches = trace.0.iter().filter(|e| matches!(e, TraceEvent::Fetch { .. })).count();
            let rows = eng.ex.query("SELECT COUNT(*) FROM target_rows", &BindMap::new())
                .unwrap().fetch_many(1)[0][0].clone();
            println!("set_oriented={set}: SqlExec={sql_execs} Fetch={fetches} rows={rows:?}");
            if set {
                assert_eq!(sql_execs, 1, "ON 应只有 1 条业务 SqlExec");
                assert_eq!(fetches, 0, "ON 不应有逐行 Fetch");
            } else {
                assert_eq!(sql_execs, n, "OFF 应逐行 N 条 INSERT");
                assert_eq!(fetches, n + 1, "OFF 应 N+1 次 Fetch");
            }
            assert_eq!(format!("{rows:?}"), format!("{:?}", SpinValue::Int(n as i64)));
        }
    });
}

/// 性能方向基准（多轮、冷快照）：对照 CIDR2024 的 RBAR 消除预期。打印不判红。
#[test]
fn probe_perf_rbar_elimination() {
    big_stack(|| {
        println!("\n=== PR#48 RBAR 消除基准（DuckDB 1.5.5 bundled, in-memory, 单线程调用, debug 构建）===");
        println!("规模 | 模式 | 三轮墙钟(ms) | 业务SqlExec | 备注");
        for &n in &[3usize, 1000, 20000, 100000] {
            let data = format!("INSERT INTO source_rows SELECT i, 'payload' FROM range({n}) t(i)");
            for set in [false, true] {
                let mut times = Vec::new();
                let mut sql_count = 0usize;
                for _ in 0..3 {
                    let mut eng = setup(TGT_DDL, Some((SRC_DDL, &data)), &[]);
                    eng.register(COPY).unwrap();
                    eng.set_oriented = set;
                    eng.step_budget = 100_000_000;
                    let mut trace = Trace::default();
                    let t0 = std::time::Instant::now();
                    eng.call_with_sink("copy_rows", &[], &mut trace).unwrap();
                    times.push(t0.elapsed().as_secs_f64() * 1000.0);
                    sql_count = trace.0.iter().filter(|e| matches!(e, TraceEvent::SqlExec { .. })).count();
                }
                let mode = if set { "ON " } else { "OFF" };
                let note = if set { "含2次元数据查询(不入trace)" } else { "逐行INSERT" };
                println!("{n:>6} | {mode} | {times:?} | {sql_count} | {note}");
            }
        }
    });
}