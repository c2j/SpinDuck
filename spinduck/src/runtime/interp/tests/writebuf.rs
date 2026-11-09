//! writebuf / batch_commit 行为测试（§3 规模拆分：自 tests.rs 拆出，
//! 共享 helper 经 `use super::*` 取自 mod.rs，行为断言不变）。
use super::*;

#[test]
fn should_coalesce_inserts_and_commits_when_batch_commit_on() {
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(
        vec![
            assign(var("v_i"), lit_int(0)),
            PlStatement::Loop(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlLoopStmt {
                    label: None,
                    body: count_loop_body(),
                    end_label: None,
                },
                None,
            )),
        ],
        &ex,
        true,
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let commits = ex.calls().iter().filter(|(s, _)| s == "COMMIT").count();
    let merged = ex
        .calls()
        .iter()
        .find(|(s, _)| s.starts_with("INSERT"))
        .map(|(sql, _)| sql.clone());
    assert_eq!(commits, 0, "开关 ON 时过程内 COMMIT 必须被吞并");
    let insert_count = ex
        .calls()
        .iter()
        .filter(|(s, _)| s.starts_with("INSERT"))
        .count();
    assert_eq!(insert_count, 1, "同表 INSERT 应合并为一条多值语句");
    let tuples = merged
        .as_deref()
        .map(|s| s.matches('(').count())
        .unwrap_or(0);
    // 3 个行元组 + 1 个列清单括号
    assert_eq!(tuples, 4, "多值语句应含 3 行元组 + 列清单: {tuples}");
}

#[test]
fn should_commit_immediately_when_batch_commit_off() {
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(
        vec![
            assign(var("v_i"), lit_int(0)),
            PlStatement::Loop(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlLoopStmt {
                    label: None,
                    body: count_loop_body(),
                    end_label: None,
                },
                None,
            )),
        ],
        &ex,
        false,
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let commits = ex.calls().iter().filter(|(s, _)| s == "COMMIT").count();
    let inserts = ex
        .calls()
        .iter()
        .filter(|(s, _)| s.starts_with("INSERT"))
        .count();
    assert_eq!(commits, 3, "开关 OFF（默认）逐行 COMMIT 语义不变");
    assert_eq!(inserts, 3, "开关 OFF 逐行下发");
}

#[test]
fn should_flush_buffer_before_touching_table_when_batch_commit_on() {
    // 读自己写：INSERT t 后 DELETE t（触及缓冲表）→ 必须先刷盘再下发 DELETE
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let del_sql = "DELETE FROM t WHERE a = 'x'";
    let parsed = Parser::new(Tokenizer::new(del_sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let del = PlStatement::SqlStatement {
        span: None,
        sql_text: del_sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::default();
    let stmts = vec![insert_stmt("t", "'x'"), del];
    let (_, out) = run_pl_batch(stmts, &ex, true);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    assert!(calls[0].0.starts_with("INSERT"), "缓冲 INSERT 先刷盘");
    assert!(calls[1].0.starts_with("DELETE"), "DELETE 后于刷盘下发");
    assert_eq!(calls.len(), 2);
}

#[test]
fn should_flush_buffer_before_touching_table_when_insert_has_no_space_before_cols() {
    // 复核应修 3：`INSERT INTO t(a) VALUES …`（表名与列清单之间无空格）的
    // tables() 曾取到 "t(a," → 永不命中 → 读自己写漏刷（golden case_split_agg
    // 的 routine.sql 即此写法）。表名 token 必须按 '(' 截断。
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let ins_sql = "INSERT INTO t(a) VALUES ('x')";
    let ins = PlStatement::SqlStatement {
        span: None,
        sql_text: ins_sql.to_string(),
        statement: Box::new(
            Parser::new(Tokenizer::new(ins_sql).tokenize().unwrap())
                .parse()
                .remove(0),
        ),
    };
    let del_sql = "DELETE FROM t WHERE a = 'x'";
    let del = PlStatement::SqlStatement {
        span: None,
        sql_text: del_sql.to_string(),
        statement: Box::new(
            Parser::new(Tokenizer::new(del_sql).tokenize().unwrap())
                .parse()
                .remove(0),
        ),
    };
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(vec![ins, del], &ex, true);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    assert!(
        calls[0].0.starts_with("INSERT"),
        "无空格形态也必须命中缓冲表并先刷盘，实际下发顺序: {calls:?}"
    );
    assert!(calls[1].0.starts_with("DELETE"), "DELETE 后于刷盘下发");
}

struct FailOnPattern {
    pattern: &'static str,
    ok_execs: std::cell::Cell<u32>,
}
impl SqlExecutor for FailOnPattern {
    fn query(
        &self,
        _sql: &str,
        _binds: &BindMap,
    ) -> Result<crate::exec::RowStream, crate::error::EngineError> {
        Ok(crate::exec::RowStream::new(vec![], vec![]))
    }
    fn execute(
        &self,
        sql: &str,
        _binds: &BindMap,
    ) -> Result<crate::exec::ExecOutcome, EngineError> {
        if sql.contains(self.pattern) {
            return Err(crate::error::EngineError::kernel(
                10,
                "constraint violation".to_string(),
            ));
        }
        self.ok_execs.set(self.ok_execs.get() + 1);
        Ok(crate::exec::ExecOutcome { rows_affected: 1 })
    }
}

#[test]
fn should_replay_batch_and_skip_failing_row_when_batch_commit_on() {
    // 批内含坏行：合并语句失败 → 回退逐行 → 坏行跳过、好行落库、不误伤调用结果
    let ex = std::rc::Rc::new(FailOnPattern {
        pattern: "'bad'",
        ok_execs: std::cell::Cell::new(0),
    });
    let stmts = vec![
        insert_stmt("t", "'good1'"),
        insert_stmt("t", "'bad'"),
        insert_stmt("t", "'good2'"),
    ];
    let rc: std::rc::Rc<dyn SqlExecutor> = ex.clone();
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &*rc,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    assert_eq!(ex.ok_execs.get(), 2, "合并失败后逐行重放：两个好行落库");
}

#[test]
fn should_fall_back_to_immediate_exec_for_insert_select_when_batch_commit_on() {
    // 评审阻塞项 2：INSERT … SELECT 是形态覆盖门禁已断言的合法形态，
    // 开关 ON 时不可缓冲 → 回落即时下发（不得 ENG-0033 上抛）
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;
    let sql = "INSERT INTO t2 (a) SELECT a FROM t1";
    let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
        .parse()
        .remove(0);
    let ins_sel = PlStatement::SqlStatement {
        span: None,
        sql_text: sql.to_string(),
        statement: Box::new(parsed),
    };
    let ex = FakeExecutor::default();
    let (_, out) = run_pl_batch(vec![insert_stmt("t1", "'x'"), ins_sel], &ex, true);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    // 复核建议 5：回落分支的「先刷盘」也要有断言保护——预置的 t1 缓冲行
    // 必须先于 INSERT…SELECT 下发（删掉该分支的 flush 会在此变红）
    assert_eq!(
        calls.len(),
        2,
        "恰好两条下发：t1 缓冲合并 INSERT 先刷盘 + INSERT…SELECT 即时下发，实际: {calls:?}"
    );
    assert!(
        calls[0].0.starts_with("INSERT INTO t1"),
        "缓冲的 t1 行先刷盘，实际: {calls:?}"
    );
    assert!(
        calls[1].0.starts_with("INSERT INTO t2"),
        "回落路径必须真实下发 INSERT…SELECT"
    );
}

#[test]
fn should_fall_back_to_immediate_exec_for_insert_with_fn_sites_when_batch_commit_on() {
    // 复核阻塞项：带 sqlfn 下沉（fn_sites 非空）的 INSERT 不可攒批——
    // 攒批路径跳过 sqlstmts::exec，materialize_fn_sites 不执行，
    // sd_fnN 缓存表要么缺失（KERNEL-0010）要么滞留旧元组（静默写错值）。
    // 准入条件：fn_sites 为空才可攒，非空必须回落即时下发。
    let site = crate::sql::FnSite {
        table: "sd_fn0".into(),
        name: "fnc_x".into(),
        args: vec!["v_i".into()],
        distinct_sql: "SELECT DISTINCT 1 AS sd_v, 1 AS sd_a0".into(),
        binds: vec![],
    };
    let fn_insert = IrStmt::SqlExec {
        sql: "INSERT INTO t (a) VALUES (1)".into(),
        binds: vec![],
        fn_sites: vec![site],
    };
    let ex = FakeExecutor::default();
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    // 复核建议 5：预置同表缓冲行使回落分支的 flush_if_touches 真实执行，
    // 并断言顺序（先刷盘 → 再物化 → 再下发 INSERT）
    let prog = IrProg {
        code: vec![
            IrStmt::SqlExec {
                sql: "INSERT INTO t (a) VALUES ('x')".into(),
                binds: vec![],
                fn_sites: vec![],
            },
            fn_insert,
        ],
    };
    let out = run_with_catalog(
        &prog,
        &mut f,
        &ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let calls = ex.calls();
    let ins_pos = calls
        .iter()
        .position(|(s, _)| s.starts_with("INSERT INTO t"))
        .expect("带下沉的 INSERT 必须原样即时下发，不得进入缓冲");
    let mat_pos = calls
        .iter()
        .position(|(s, _)| s.contains("CREATE OR REPLACE TEMP TABLE sd_fn0"))
        .expect("fn_sites 必须先物化（temp table 建立）");
    assert!(
        ins_pos < mat_pos,
        "缓冲的 t 行必须先刷盘（calls[0]），再物化 sd_fn0；实际: {calls:?}"
    );
    assert!(
        calls[0].0.starts_with("INSERT INTO t"),
        "首条下发必须是刷盘的合并 INSERT，实际: {calls:?}"
    );
}

/// 事件收集 sink（writebuf 刷盘事件断言用）：SqlExec 记 SQL 文本、
/// BatchSkip 记「跳过行数|首错文本」。
struct VecSink {
    events: std::cell::RefCell<Vec<String>>,
}

impl VecSink {
    fn new() -> Self {
        Self {
            events: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn snapshot(&self) -> Vec<String> {
        self.events.borrow().clone()
    }
}

impl crate::exec::TraceSink for VecSink {
    fn on_event(&mut self, event: crate::exec::TraceEvent) {
        match event {
            crate::exec::TraceEvent::SqlExec { sql, .. } => self.events.borrow_mut().push(sql),
            crate::exec::TraceEvent::BatchSkip { rows, first_error } => self
                .events
                .borrow_mut()
                .push(format!("SKIP {rows}|{first_error}")),
            // 显式分支（§3 穷尽纪律）：新事件进断言面前必须先在此表意
            crate::exec::TraceEvent::CursorOpen { .. } => {}
            crate::exec::TraceEvent::Fetch { .. } => {}
            crate::exec::TraceEvent::UdfCall { .. } => {}
            crate::exec::TraceEvent::IrStmt { .. } => {}
        }
    }
}

#[test]
fn should_emit_sql_exec_event_for_merged_insert_at_flush_point() {
    // 复核应修 B：被合并的 INSERT 在 OFF 路径每条语句发一次 SqlExec 事件，
    // ON 路径刷盘只调 ex.execute——JSONL 面上这些写入完全消失，排障盲区。
    // 刷盘点必须补发与 OFF 同构的事件。
    let ex = FakeExecutor::default();
    let prog = compile_stmts(&[insert_stmt("t", "'x'"), commit_stmt()]).unwrap();
    let mut f = Frame::new();
    let mut sink = VecSink::new();
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let events = sink.snapshot();
    assert!(
        events.iter().any(|s| s.starts_with("INSERT INTO t")),
        "刷盘点必须补发合并 INSERT 的 SqlExec 事件，实际: {events:?}"
    );
}

#[test]
fn should_emit_batch_skip_event_when_replay_drops_rows() {
    // 复核应修 2：回放期被跳过的坏行原样吞掉——JSONL 面上看不出 ON 模式
    // 少了一批数据。补发 BatchSkip{rows, first_error} 事件使丢行可见，
    // 并能把「业务异常行跳过」与引擎缺陷区分开。
    let ex = FailOnPattern {
        pattern: "'bad'",
        ok_execs: std::cell::Cell::new(0),
    };
    let stmts = vec![
        insert_stmt("t", "'good1'"),
        insert_stmt("t", "'bad'"),
        insert_stmt("t", "'good2'"),
    ];
    let prog = compile_stmts(&stmts).expect("compile");
    let mut f = Frame::new();
    let mut sink = VecSink::new();
    let cat = crate::catalog::RoutineCatalog::new();
    let out = run_with_catalog(
        &prog,
        &mut f,
        &ex,
        &mut sink,
        &cat,
        0,
        crate::runtime::RunLimits {
            budget: DEFAULT_STEP_BUDGET,
            batch_commit: true,
        },
    );
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    let events = sink.snapshot();
    // 第四轮②：first_error 必须携带错误码（KERNEL-0010），使「引擎缺陷 vs
    // 业务跳行」不靠读文本猜——§5 故障域标签在产生点标注
    assert!(
        events
            .iter()
            .any(|s| s.starts_with("SKIP 1|KERNEL-") && s.contains("constraint violation")),
        "回放跳行必须发 BatchSkip 事件（行数 + 带错误码的首错），实际: {events:?}"
    );
}

#[test]
fn should_flush_partial_batch_when_write_buf_cap_reached() {
    // 复核应修 ⑤：WRITE_BUF_CAP「满则先刷盘」是文档语义①的一部分，
    // 却无任何测试。经可注入上限（§2 接缝第 3 条）锁行为：cap=2 时第 3 行
    // 攒入前先刷出前两行 → 两次下发，各自为一条合并 INSERT。
    let prog = compile_stmts(&[
        insert_stmt("t", "'1'"),
        insert_stmt("t", "'2'"),
        insert_stmt("t", "'3'"),
    ])
    .expect("compile");
    let ex = FakeExecutor::default();
    let mut f = Frame::new();
    let mut sink = crate::exec::TraceNoopSink;
    let cat = crate::catalog::RoutineCatalog::new();
    let mut env = Env {
        frame: &mut f,
        ex: &ex,
        sink: &mut sink,
        cat: &cat,
        depth: 0,
        budget: DEFAULT_STEP_BUDGET,
        batch: true,
        wbuf: crate::runtime::interp::writebuf::shared_buffer_with_cap(2),
        current_error: None,
    };
    let mut traps = Vec::new();
    let mut pc = 0usize;
    let mut steps = 0u64;
    let out = crate::runtime::interp::run_inner(&prog, &mut env, &mut traps, &mut pc, &mut steps);
    assert_eq!(out.unwrap(), RunOutcome::Completed);
    // run_inner 的出口刷盘在外层 run() 里；此处驱动同一刷盘点收尾第 3 行
    crate::runtime::interp::writebuf::flush_all(&mut env).unwrap();
    let calls = ex.calls();
    assert_eq!(
        calls.len(),
        2,
        "cap=2：第 3 行攒入前先刷出前两行，实际: {calls:?}"
    );
    assert!(
        calls[0].0.contains("('1')") && calls[0].0.contains("('2')"),
        "首次刷盘是一条两行合并 INSERT，实际: {calls:?}"
    );
    assert!(
        calls[1].0.contains("('3')"),
        "第 3 行随出口刷盘，实际: {calls:?}"
    );
}
