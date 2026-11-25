//! loop-parallel Worker 沙箱行为测试（设计 §3.1/§3.2）：
//! 纯前缀求值、record 绑定、快照命中/交权、错误整行作废、trap 栈回传。

use super::calls::CallSnapshot;
use super::worker::{run_row, WorkerJob, WorkerOutcome, WorkerRowResult};
use crate::catalog::RoutineCatalog;
use crate::exec::{BindMap, ExecOutcome, Row, RowStream, SqlExecutor, TraceEvent};
use crate::ir::loop_parallel::PurePrefixPlan;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{AssignTarget, BindSrc, IrExpr, IrProg, IrStmt, TrapEntry, VarName};
use crate::lookup_cache::{FrozenLookupSnapshot, LookupExecutor, LookupState, PrewarmOutcome};
use crate::runtime::frame::Frame;
use crate::runtime::interp::TrapFrame;
use crate::types::SpinType;
use crate::value::SpinValue;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

fn vn(s: &str) -> VarName {
    VarName(s.into())
}

fn e_var(s: &str) -> IrExpr {
    IrExpr::Var(vn(s))
}

fn e_int(n: i64) -> IrExpr {
    IrExpr::Const(SpinValue::Int(n))
}

fn assign_var(target: &str, expr: IrExpr) -> IrStmt {
    IrStmt::Assign {
        target: AssignTarget::Var(vn(target)),
        expr,
    }
}

/// 手工计划：`g_pure` 即给定集合，`body_start_pc = 0`，交权点由
/// `g_pure` 成员资格推导（首个 ∉ 集合的 pc）。
fn plan(g_pure: &[usize]) -> PurePrefixPlan {
    PurePrefixPlan {
        region: CursorRegion {
            open_pc: usize::MAX,
            fetch_pc: usize::MAX,
            close_pc: usize::MAX,
            cursor: vn("c"),
            record: vn("rec"),
        },
        cond_pc: usize::MAX,
        body_start_pc: 0,
        exit_pc: usize::MAX,
        g_pure: g_pure.iter().copied().collect::<BTreeSet<_>>(),
        max_prefix_steps: 1_000,
        queries: Vec::new(),
    }
}

fn prog(code: Vec<IrStmt>) -> IrProg {
    IrProg {
        code,
        var_types: BTreeMap::new(),
        local_ddl: Vec::new(),
    }
}

fn empty_snapshot() -> FrozenLookupSnapshot {
    FrozenLookupSnapshot::default()
}

/// WorkerJob 的借用宿主：schema/record_target/var_types 需活得比 job 久。
struct Ctx {
    schema: Vec<String>,
    record_target: AssignTarget,
    var_types: BTreeMap<String, SpinType>,
    catalog: RoutineCatalog,
    calls: CallSnapshot,
}

impl Ctx {
    fn new() -> Self {
        Self {
            schema: vec!["a".to_string()],
            record_target: AssignTarget::Var(vn("rec")),
            var_types: BTreeMap::new(),
            catalog: RoutineCatalog::new(),
            calls: CallSnapshot::new(),
        }
    }

    fn job<'a>(
        &'a self,
        prog: &'a IrProg,
        plan: &'a PurePrefixPlan,
        frame: &'a Frame,
        rows: &'a Arc<Vec<Row>>,
        snapshot: &'a FrozenLookupSnapshot,
        traps: &'a [TrapFrame],
    ) -> WorkerJob<'a> {
        WorkerJob {
            prog,
            plan,
            base_frame: frame,
            rows,
            cursor_schema: &self.schema,
            record_target: &self.record_target,
            snapshot,
            catalog: &self.catalog,
            calls: &self.calls,
            var_types: &self.var_types,
            loose: false,
            traps_seed: traps,
        }
    }
}

fn rows_arc(rows: Vec<Row>) -> Arc<Vec<Row>> {
    Arc::new(rows)
}

/// 写日志断言：`(target, value)` 对逐字段比对（AssignTarget 无 PartialEq）。
fn assert_writes(res: &WorkerRowResult, expected: &[(&str, SpinValue)]) {
    assert_eq!(res.writes.len(), expected.len(), "writes: {:?}", res.writes);
    for (i, (name, value)) in expected.iter().enumerate() {
        match &res.writes[i].0 {
            AssignTarget::Var(v) => assert_eq!(v.0, *name),
            other => panic!("write[{i}] target 不是 Var: {other:?}"),
        }
        assert_eq!(&res.writes[i].1, value, "write[{i}] value");
    }
}

#[test]
fn should_execute_pure_assignments_and_yield_at_boundary() {
    // pc0: x = 1；pc1: y = x + 2；pc2: SqlExec（∉ g_pure）→ 交权。
    let p = prog(vec![
        assign_var("x", e_int(1)),
        assign_var(
            "y",
            IrExpr::Bin {
                op: crate::ir::BinOp::Add,
                l: Box::new(e_var("x")),
                r: Box::new(e_int(2)),
            },
        ),
        IrStmt::SqlExec {
            sql: "UPDATE t SET a = 1".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
            buffer_policy: crate::ir::BufferPolicy::Immediate,
        },
    ]);
    let plan = plan(&[0, 1]);
    let ctx = Ctx::new();
    let frame = Frame::new();
    let rows = rows_arc(vec![vec![SpinValue::Int(9)]]);
    let snap = empty_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert_eq!(res.steps, 2);
    assert_writes(&res, &[("x", SpinValue::Int(1)), ("y", SpinValue::Int(3))]);
    assert_eq!(res.traces.len(), 2);
    assert!(matches!(res.traces[0], TraceEvent::IrStmt { .. }));
    assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 2 }));
    assert!(res.traps.is_empty());
}

#[test]
fn should_bind_fetch_record_and_read_fields() {
    // pc0: z = rec.a（record 绑定后字段可读）；pc1 交权。
    let p = prog(vec![
        assign_var(
            "z",
            IrExpr::Field {
                rec: vn("rec"),
                field: "a".into(),
            },
        ),
        IrStmt::Nop,
    ]);
    let plan = plan(&[0]);
    let ctx = Ctx::new();
    let frame = Frame::new();
    let rows = rows_arc(vec![vec![SpinValue::Int(7)]]);
    let snap = empty_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert_writes(&res, &[("z", SpinValue::Int(7))]);
    assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 1 }));
}

#[test]
fn should_yield_on_snapshot_miss_without_executing_query() {
    // pc0: 可缓存 SqlQuery；空快照 → probe None → 交权，步数/trace 皆零。
    let p = prog(vec![
        IrStmt::SqlQuery {
            sql: "SELECT s FROM t_form WHERE id = $v_p".into(),
            binds: vec![("$v_p".to_string(), BindSrc::Var(vn("p")))],
            into: vec![AssignTarget::Var(vn("v"))],
            fn_sites: Vec::new(),
        },
        assign_var("w", e_var("v")),
    ]);
    let plan = plan(&[0, 1]);
    let ctx = Ctx::new();
    let mut frame = Frame::new();
    frame.set(&vn("p"), SpinValue::Int(1));
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = empty_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert_eq!(res.steps, 0);
    assert!(res.writes.is_empty());
    assert!(res.traces.is_empty());
    assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 0 }));
}

// ---- 快照命中：LookupExecutor + 假执行器预热（§5.1/§5.2）----

const FORM_SQL: &str = "SELECT s FROM t_form WHERE id = $v_p";

struct FakeExec {
    rows: Vec<Row>,
}

impl SqlExecutor for FakeExec {
    fn query(&self, _sql: &str, _binds: &BindMap) -> Result<RowStream, crate::error::EngineError> {
        Ok(RowStream::new(vec!["s".into()], self.rows.clone()))
    }
    fn execute(
        &self,
        _sql: &str,
        _binds: &BindMap,
    ) -> Result<ExecOutcome, crate::error::EngineError> {
        Ok(ExecOutcome { rows_affected: 0 })
    }
}

fn frozen_form_snapshot() -> FrozenLookupSnapshot {
    frozen_form_snapshot_rows(vec![vec![SpinValue::Int(1), SpinValue::Str("hit".into())]])
}

fn frozen_form_snapshot_rows(rows: Vec<Row>) -> FrozenLookupSnapshot {
    let ex = LookupExecutor::new(Rc::new(FakeExec { rows }), Rc::new(LookupState::default()));
    let mut binds = BindMap::new();
    binds.insert("$v_p".to_string(), SpinValue::Int(1));
    assert!(matches!(
        ex.try_prewarm_only(FORM_SQL, &binds),
        PrewarmOutcome::Frozen
    ));
    ex.export_frozen()
}

#[test]
fn should_serve_sqlquery_from_frozen_snapshot() {
    // pc0: SELECT INTO（快照命中）；pc1: w = v；pc2 交权。
    let p = prog(vec![
        IrStmt::SqlQuery {
            sql: FORM_SQL.into(),
            binds: vec![("$v_p".to_string(), BindSrc::Var(vn("p")))],
            into: vec![AssignTarget::Var(vn("v"))],
            fn_sites: Vec::new(),
        },
        assign_var("w", e_var("v")),
        IrStmt::Nop,
    ]);
    let plan = plan(&[0, 1]);
    let ctx = Ctx::new();
    let mut frame = Frame::new();
    frame.set(&vn("p"), SpinValue::Int(1));
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = frozen_form_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert_eq!(res.steps, 2);
    assert_writes(
        &res,
        &[
            ("v", SpinValue::Str("hit".into())),
            ("w", SpinValue::Str("hit".into())),
        ],
    );
    // trace：与串行同序——IrStmt(SqlQuery) 在先，SqlExec 快照命中口径
    // 随后，IrStmt(Assign) 收尾（P1 审核：补齐缺失的 IrStmt 事件）。
    assert!(matches!(
        res.traces[0],
        TraceEvent::IrStmt {
            variant: "SqlQuery"
        }
    ));
    assert!(matches!(res.traces[1], TraceEvent::SqlExec { .. }));
    assert!(matches!(res.traces[2], TraceEvent::IrStmt { .. }));
    assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 2 }));
}

#[test]
fn should_discard_whole_row_on_pure_error() {
    // pc0: x = missing（未定义变量 → 求值错）；结果整体作废。
    let p = prog(vec![
        assign_var("x", e_var("missing")),
        assign_var("y", e_int(1)),
    ]);
    let plan = plan(&[0, 1]);
    let ctx = Ctx::new();
    let frame = Frame::new();
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = empty_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert!(matches!(res.outcome, WorkerOutcome::Error));
    assert_eq!(res.steps, 0);
    assert!(res.writes.is_empty());
    assert!(res.traces.is_empty());
}

#[test]
fn should_echo_trap_stack_after_balanced_push_pop() {
    // pc0 PushTrap / pc1 PopTrap / pc2 交权：净效果为零，trap 栈回传种子。
    let p = prog(vec![
        IrStmt::PushTrap {
            entries: vec![TrapEntry {
                conds: vec!["OTHERS".into()],
                body_start: 0,
                body_end: 0,
            }],
        },
        IrStmt::PopTrap,
        IrStmt::Nop,
    ]);
    let plan = plan(&[0, 1]);
    let ctx = Ctx::new();
    let frame = Frame::new();
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = empty_snapshot();
    let traps = vec![TrapFrame {
        entries: vec![TrapEntry {
            conds: vec!["X".into()],
            body_start: 9,
            body_end: 9,
        }],
    }];
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert_eq!(res.traps.len(), 1, "种子帧应原样回传");
    assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 2 }));
    assert_eq!(res.steps, 2);
}

#[test]
fn should_return_unbalanced_trap_stack_to_main() {
    // pc0 PushTrap 无配对 PopTrap → 交权时 trap 栈须携带新增帧。
    let p = prog(vec![
        IrStmt::PushTrap {
            entries: vec![TrapEntry {
                conds: vec!["OTHERS".into()],
                body_start: 0,
                body_end: 0,
            }],
        },
        IrStmt::Nop,
    ]);
    let plan = plan(&[0]);
    let ctx = Ctx::new();
    let frame = Frame::new();
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = empty_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert_eq!(res.traps.len(), 1);
    assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 1 }));
}

// ---- 派发池：chunk 并行 + row_idx 排序 + 线程数等价 ----

use super::pool::{dispatch_chunk, ChunkJob};

#[test]
fn should_dispatch_chunk_with_ordered_results_across_threads() {
    // 4 行：x = rec.a + row_idx 无关（每行独立求值同值）。
    let p = prog(vec![
        assign_var(
            "x",
            IrExpr::Bin {
                op: crate::ir::BinOp::Add,
                l: Box::new(IrExpr::Field {
                    rec: vn("rec"),
                    field: "a".into(),
                }),
                r: Box::new(e_int(1)),
            },
        ),
        IrStmt::Nop,
    ]);
    let plan = plan(&[0]);
    let ctx = Ctx::new();
    let ctx_frame = Frame::new();
    let rows = rows_arc(vec![
        vec![SpinValue::Int(10)],
        vec![SpinValue::Int(20)],
        vec![SpinValue::Int(30)],
        vec![SpinValue::Int(40)],
    ]);
    let snap = empty_snapshot();
    let traps = Vec::new();
    let indices = [0usize, 1, 2, 3];
    let expected = [
        SpinValue::Int(11),
        SpinValue::Int(21),
        SpinValue::Int(31),
        SpinValue::Int(41),
    ];
    for threads in [1usize, 4] {
        let out = dispatch_chunk(&ChunkJob {
            prog: &p,
            plan: &plan,
            base_frame: &ctx_frame,
            rows: &rows,
            cursor_schema: ctx.schema.as_slice(),
            record_target: &ctx.record_target,
            snapshot: &snap,
            catalog: &ctx.catalog,
            calls: &ctx.calls,
            var_types: &ctx.var_types,
            loose: false,
            traps_seed: &traps,
            indices: &indices,
            threads,
        });
        assert_eq!(out.len(), 4);
        for (i, res) in out.iter().enumerate() {
            assert_eq!(res.row_idx, i, "threads={threads} 结果须按 row_idx 排序");
            assert_writes(res, &[("x", expected[i].clone())]);
            assert!(matches!(res.outcome, WorkerOutcome::Yield { pc: 1 }));
        }
    }
}

#[test]
fn should_keep_worker_types_send_sync() {
    // R4-P2-5：std::thread::scope 派发要求负载 Send + Sync。
    fn need<T: Send + Sync>() {}
    need::<SpinValue>();
    need::<Frame>();
    need::<FrozenLookupSnapshot>();
    need::<WorkerRowResult>();
    need::<Arc<Vec<Row>>>();
    let _ = std::marker::PhantomData::<SpinType>;
    let _ = std::marker::PhantomData::<BindMap>;
}

// ---- V8-E9：探测权威错误行数 → QueryError（主线程注入 trap 分派）----

fn sqlquery_stmt() -> IrStmt {
    IrStmt::SqlQuery {
        sql: FORM_SQL.into(),
        binds: vec![("$v_p".to_string(), BindSrc::Var(vn("p")))],
        into: vec![AssignTarget::Var(vn("v"))],
        fn_sites: Vec::new(),
    }
}

#[test]
fn should_return_queryerror_ndf_when_probe_bucket_empty() {
    // 键不在冻结索引 → 探测 0 行 = NO_DATA_FOUND 确定性结局；
    // 步数与 SqlExec trace 已计/已发（与串行失败路径同口径）。
    let p = prog(vec![sqlquery_stmt(), assign_var("w", e_var("v"))]);
    let plan = plan(&[0, 1]);
    let ctx = Ctx::new();
    let mut frame = Frame::new();
    frame.set(&vn("p"), SpinValue::Int(999));
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = frozen_form_snapshot();
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert!(matches!(
        res.outcome,
        WorkerOutcome::QueryError {
            kind: super::worker::QueryErrorKind::NoDataFound
        }
    ));
    assert_eq!(res.steps, 1);
    assert!(res.writes.is_empty());
    // 与串行同序：IrStmt(SqlQuery) 先于 SqlExec（步数/trace 已计已发，
    // 错误行数结局由主线程注入 trap 分派）。
    assert!(matches!(
        res.traces[0],
        TraceEvent::IrStmt {
            variant: "SqlQuery"
        }
    ));
    assert!(matches!(res.traces[1], TraceEvent::SqlExec { .. }));
}

#[test]
fn should_return_queryerror_tmr_when_probe_returns_two_rows() {
    // 冻结桶含 2 行 → SELECT INTO 必然 TOO_MANY_ROWS。
    let p = prog(vec![sqlquery_stmt()]);
    let plan = plan(&[0]);
    let ctx = Ctx::new();
    let mut frame = Frame::new();
    frame.set(&vn("p"), SpinValue::Int(1));
    let rows = rows_arc(vec![vec![SpinValue::Int(0)]]);
    let snap = frozen_form_snapshot_rows(vec![
        vec![SpinValue::Int(1), SpinValue::Str("a".into())],
        vec![SpinValue::Int(1), SpinValue::Str("b".into())],
    ]);
    let traps = Vec::new();
    let res = run_row(&ctx.job(&p, &plan, &frame, &rows, &snap, &traps), 0);
    assert!(matches!(
        res.outcome,
        WorkerOutcome::QueryError {
            kind: super::worker::QueryErrorKind::TooManyRows
        }
    ));
    assert_eq!(res.steps, 1);
    assert!(res.writes.is_empty());
}
