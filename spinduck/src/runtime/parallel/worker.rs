//! loop-parallel Worker 沙箱：$G_{\text{pure}}$ 单行纯前缀求值。
//!
//! 铁律（设计 §3.1/§5）：Worker **零数据库接触**——不持有 `SqlExecutor`、
//! 不执行 DuckDB SQL、不建临时表、不写 `WriteBuffer`。可缓存 `SqlQuery`
//! 一律经 `FrozenLookupSnapshot::probe` 命中冻结槽位；探测回退（`None`）
//! 或 `fn_sites` 非空时该行在**指令边界**交权给主线程续跑。
//!
//! 每行独立 `Frame` 克隆 + FETCH-record 绑定；写集以字段粒度有序日志
//! 返回主线程按 `row_idx` 顺序落位。任何指令错误 → `WorkerOutcome::Error`，
//! 该行结果整体丢弃、主线程从 `body_start_pc` 原生重跑（trap 分派、
//! `__sqlerrm`、步数预算全部由原解释器精确复现）。

use crate::catalog::{ParamMode, RoutineCatalog, RoutineKind};
use crate::error::EngineError;
use crate::exec::{BindMap, Row, TraceEvent};
use crate::ir::loop_parallel::PurePrefixPlan;
use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt};
use crate::lookup_cache::FrozenLookupSnapshot;
use crate::runtime::frame::Frame;
use crate::runtime::interp::{resolve_binds, spinduck_trace_event, TrapFrame};
use crate::runtime::parallel::calls::{
    routine_key, scalar_key, CallOutcome, CallSnapshot, PendingCall,
};
use crate::types::SpinType;
use crate::value::SpinValue;
use std::collections::BTreeMap;
use std::sync::Arc;

/// 一行 Worker 任务的只读输入（`std::thread::scope` 共享引用，须 `Sync`）。
pub(crate) struct WorkerJob<'a> {
    pub prog: &'a IrProg,
    pub plan: &'a PurePrefixPlan,
    /// 派发时刻主线程 `InterpState.frame` 的克隆基底（已含 `< row_idx`
    /// 各行 suffix 写——门禁 2 保证前缀不读这些携带值）。
    pub base_frame: &'a Frame,
    /// 游标物化行的只读共享存储；`row_idx` 为绝对下标。
    pub rows: &'a Arc<Vec<Row>>,
    /// 游标结果 schema（FETCH-record 建 RECORD 用）。
    pub cursor_schema: &'a [String],
    /// FETCH 的 record 目标（`AssignTarget::Var`）。
    pub record_target: &'a AssignTarget,
    /// 冻结查找快照（预热产物；`None` 探测 → 该行交权）。
    pub snapshot: &'a FrozenLookupSnapshot,
    /// 例程目录（`FuncCall` 物化分派：Function → 例程键，未命中 →
    /// 内建 prepared 标量键）。
    pub catalog: &'a RoutineCatalog,
    /// E2 调用物化快照（`FuncCall`/`SqlFn` 探测；miss → `NeedKeys`）。
    pub calls: &'a CallSnapshot,
    /// `var_types` + `implicit_assignment_loose`（`assign` 同口径转换）。
    pub var_types: &'a BTreeMap<String, SpinType>,
    pub loose: bool,
    /// `body_start_pc` 处的 trap 栈种子（`InterpState.traps` 克隆）。
    pub traps_seed: &'a [TrapFrame],
}

/// 一行 Worker 产出（按 `row_idx` 顺序由主线程合并）。
#[derive(Debug)]
pub(crate) struct WorkerRowResult {
    pub row_idx: usize,
    /// 实际执行的纯前缀指令数（交权/未执行指令不计）。
    pub steps: u64,
    /// 有序字段粒度写日志（`AssignTarget` + 已 coerce 值）。
    pub writes: Vec<(AssignTarget, SpinValue)>,
    /// 逐指令 trace 事件（与串行同序同内容；`SqlQuery` 命中快照时
    /// 同样产出 `SqlExec` 事件）。
    pub traces: Vec<TraceEvent>,
    /// 前缀执行后的 trap 栈（成功交权时主线程采纳——前缀内
    /// PushTrap/PopTrap 的净效果必须由主线程可见）。
    pub traps: Vec<TrapFrame>,
    pub outcome: WorkerOutcome,
}

/// Worker 单行结局。
#[derive(Debug)]
pub(crate) enum WorkerOutcome {
    /// 在 `pc`（∉ `g_pure`）交权：主线程从该 pc 原生续跑。
    Yield { pc: usize },
    /// 物化调用快照未命中：携带待执行请求交权，主线程补物化后
    /// 重派本行（已产出的步骤/写集/trace 一并作废——重派从头重算）。
    NeedKeys { request: PendingCall },
    /// `SqlQuery` 探测结果为权威错误行数（0 行 NO_DATA_FOUND、
    /// 多行 TOO_MANY_ROWS）：携带错误种类交权，主线程注入 trap
    /// 分派——探测已给出确定结果，无需原生重跑该查询（V8-E9）。
    QueryError { kind: QueryErrorKind },
    /// 纯前缀内指令错误：本行结果整体作废，主线程从
    /// `body_start_pc` 原生重跑（trap/`__sqlerrm`/预算精确复现）。
    Error,
}

/// `SqlQuery` 快照探测推出的确定性行数错误（与 `sqlstmts::write_into`
/// 的 0/>1 行分支同口径；`into` 超列等其余错误仍走 `Error` 原生重跑）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum QueryErrorKind {
    NoDataFound,
    TooManyRows,
}

impl QueryErrorKind {
    /// 与 `write_into` 完全同源的错误构造（trap 条件匹配依赖
    /// code/domain/message 逐字段一致）。
    pub(crate) fn into_error(self) -> EngineError {
        match self {
            Self::NoDataFound => EngineError::no_data_found(),
            Self::TooManyRows => EngineError::too_many_rows(),
        }
    }
}

/// 执行一行纯前缀。`row_idx` 为游标物化行绝对下标。
pub(crate) fn run_row(job: &WorkerJob<'_>, row_idx: usize) -> WorkerRowResult {
    let mut frame = job.base_frame.clone();
    let mut traps = job.traps_seed.to_vec();
    let mut writes = Vec::new();
    let mut traces = Vec::new();
    let mut steps = 0u64;

    // FETCH-record 绑定：与 cursors::fetch 的 record 形态同口径
    // （schema 名 ↔ 行值按位 zip 建 RECORD，再经 assign 写目标）。
    let record = SpinValue::Record(
        job.cursor_schema
            .iter()
            .cloned()
            .zip(job.rows[row_idx].iter().cloned())
            .collect(),
    );
    if assign_coerced(job, &mut frame, job.record_target, record).is_err() {
        return fail(row_idx);
    }

    let mut pc = job.plan.body_start_pc;
    loop {
        if !job.plan.g_pure.contains(&pc) {
            return WorkerRowResult {
                row_idx,
                steps,
                writes,
                traces,
                traps,
                outcome: WorkerOutcome::Yield { pc },
            };
        }
        let Some(stmt) = job.prog.code.get(pc) else {
            return fail(row_idx);
        };
        // SqlQuery 特判：先解析绑定 + 快照探测，再决定计步/埋点/执行——
        // Fallback 交权时该指令在串行侧尚未发生（步数、trace 由主线程
        // 原生产生）。fn_sites 非空同样交权（物化属主线程职责）。
        if let IrStmt::SqlQuery {
            sql,
            binds,
            into,
            fn_sites,
        } = stmt
        {
            if !fn_sites.is_empty() {
                return yield_at(row_idx, steps, writes, traces, traps, pc);
            }
            let bound = match resolve_binds(binds, &frame) {
                Ok(b) => b,
                Err(_) => return fail(row_idx),
            };
            let t = std::time::Instant::now();
            let probed = job.snapshot.probe(sql, &bound);
            let elapsed_us = t.elapsed().as_micros() as u64;
            match probed {
                Some(mut stream) => {
                    steps += 1;
                    // 与串行同序：interp 主循环先发 `IrStmt{SqlQuery}`，
                    // sqlstmts 再发 `SqlExec`（trace 契约，P1 审核）。
                    traces.push(spinduck_trace_event(stmt));
                    traces.push(TraceEvent::SqlExec {
                        sql: sql.clone(),
                        binds: bound,
                        elapsed_us,
                    });
                    let rows = stream.fetch_many(2);
                    // 探测结果权威（probe 回退已交权）：0/>1 行即
                    // `write_into` 必然抛的确定性错误——携带种类交权，
                    // 主线程注入 trap 分派，省去原生重跑该查询。
                    // 步数与 SqlExec trace 已计/已发，与串行失败路径一致。
                    let kind = if rows.is_empty() {
                        Some(QueryErrorKind::NoDataFound)
                    } else if rows.len() > 1 {
                        Some(QueryErrorKind::TooManyRows)
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        return WorkerRowResult {
                            row_idx,
                            steps,
                            writes,
                            traces,
                            traps,
                            outcome: WorkerOutcome::QueryError { kind },
                        };
                    }
                    if write_into(job, &mut frame, into, &rows, &mut writes).is_err() {
                        return fail(row_idx);
                    }
                    pc += 1;
                    continue;
                }
                None => return yield_at(row_idx, steps, writes, traces, traps, pc),
            }
        }
        steps += 1;
        traces.push(spinduck_trace_event(stmt));
        match exec_stmt(job, stmt, &mut frame, &mut traps, &mut writes, &mut pc) {
            Ok(()) => {}
            Err(SnapErr::Engine) => return fail(row_idx),
            Err(SnapErr::Yield) => {
                return yield_at(row_idx, steps, writes, traces, traps, pc);
            }
            Err(SnapErr::Need(request)) => {
                // 物化未命中：本行结果整体作废重派（步骤/写集/trace
                // 不携带——重派从头重算，避免半成品落位）。
                return WorkerRowResult {
                    row_idx,
                    steps: 0,
                    writes: Vec::new(),
                    traces: Vec::new(),
                    traps: Vec::new(),
                    outcome: WorkerOutcome::NeedKeys { request },
                };
            }
        }
    }
}

/// 交权出口（保留已产出步骤/写集/trace——主线程按序落位后原生续跑）。
fn yield_at(
    row_idx: usize,
    steps: u64,
    writes: Vec<(AssignTarget, SpinValue)>,
    traces: Vec<TraceEvent>,
    traps: Vec<TrapFrame>,
    pc: usize,
) -> WorkerRowResult {
    WorkerRowResult {
        row_idx,
        steps,
        writes,
        traces,
        traps,
        outcome: WorkerOutcome::Yield { pc },
    }
}

/// 整行作废出口（错误语义由主线程原生重跑精确复现）。
fn fail(row_idx: usize) -> WorkerRowResult {
    WorkerRowResult {
        row_idx,
        steps: 0,
        writes: Vec::new(),
        traces: Vec::new(),
        traps: Vec::new(),
        outcome: WorkerOutcome::Error,
    }
}

/// `eval_snap` 的失败分类：`Engine` = 行级错误（主线程原生重跑复现）；
/// `Yield` = 物化结果为 `Failed`（同上交权复现）；`Need` = 快照未命中
/// （主线程补物化后重派）。
enum SnapErr {
    /// 行级错误（载荷不读——主线程原生重跑精确复现错误语义）。
    Engine,
    Yield,
    Need(PendingCall),
}

impl From<EngineError> for SnapErr {
    fn from(_: EngineError) -> Self {
        Self::Engine
    }
}

/// 单条纯指令执行（`G_pure` 成员集：Nop/Assign/Perform/Jump*/
/// PushTrap/PopTrap；`SqlQuery` 在上层特判）。
fn exec_stmt(
    job: &WorkerJob<'_>,
    stmt: &IrStmt,
    frame: &mut Frame,
    traps: &mut Vec<TrapFrame>,
    writes: &mut Vec<(AssignTarget, SpinValue)>,
    pc: &mut usize,
) -> Result<(), SnapErr> {
    match stmt {
        IrStmt::Nop => *pc += 1,
        IrStmt::Assign { target, expr } => {
            let v = eval_snap(expr, frame, job, writes)?;
            let v = assign_coerced(job, frame, target, v)?;
            writes.push((target.clone(), v));
            *pc += 1;
        }
        IrStmt::Perform { expr } => {
            eval_snap(expr, frame, job, writes)?;
            *pc += 1;
        }
        IrStmt::Jump(to) => *pc = *to,
        IrStmt::JumpIf { cond, to } => {
            *pc = if eval_cond_snap(cond, frame, job, writes)? == Some(true) {
                *to
            } else {
                *pc + 1
            };
        }
        IrStmt::JumpIfNot { cond, to } => {
            *pc = if eval_cond_snap(cond, frame, job, writes)? == Some(true) {
                *pc + 1
            } else {
                *to
            };
        }
        IrStmt::PushTrap { entries } => {
            traps.push(TrapFrame {
                entries: entries.clone(),
            });
            *pc += 1;
        }
        IrStmt::PopTrap => {
            traps.pop();
            *pc += 1;
        }
        // G_pure 之外的语句种类：防御性报错（静态门禁保证不可达）。
        IrStmt::Raise { .. }
        | IrStmt::Return { .. }
        | IrStmt::TxCommit
        | IrStmt::TxCommitIfWritten
        | IrStmt::TxRollback
        | IrStmt::SqlQuery { .. }
        | IrStmt::FoldedQuery { .. }
        | IrStmt::SqlExec { .. }
        | IrStmt::SqlExecDynamic { .. }
        | IrStmt::OpenCursor { .. }
        | IrStmt::Fetch { .. }
        | IrStmt::CloseCursor { .. }
        | IrStmt::CallRoutine { .. } => {
            return Err(EngineError::invariant(
                66,
                "non-pure statement reached worker exec (loop-parallel gate violation)",
            )
            .into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// E2 快照求值：`eval_pure` 的物化扩展——`FuncCall`/`SqlFn` 经
// `CallSnapshot` 探测，未命中抛 `SnapErr::Need` 交权主线程补物化。
// ---------------------------------------------------------------------------

/// 快照版表达式求值（`eval_pure` 同三值语义 + 调用物化）。
/// `writes` 承接 `FuncCall` OUT/INOUT 写回的字段粒度日志。
fn eval_snap(
    e: &IrExpr,
    frame: &mut Frame,
    job: &WorkerJob<'_>,
    writes: &mut Vec<(AssignTarget, SpinValue)>,
) -> Result<SpinValue, SnapErr> {
    match e {
        IrExpr::Const(v) => Ok(v.clone()),
        IrExpr::Var(name) => frame.get(name).cloned().ok_or(SnapErr::Engine),
        IrExpr::Field { rec, field } => frame
            .record_field(rec, field)
            .cloned()
            .map_err(|_| SnapErr::Engine),
        IrExpr::IsNull(inner) => {
            let v = eval_snap(inner, frame, job, writes)?;
            Ok(SpinValue::Bool(matches!(v, SpinValue::Null)))
        }
        IrExpr::Not(inner) => match eval_snap(inner, frame, job, writes)? {
            SpinValue::Null => Ok(SpinValue::Null),
            SpinValue::Bool(b) => Ok(SpinValue::Bool(!b)),
            _other => Err(SnapErr::Engine),
        },
        IrExpr::Bin { op, l, r } => {
            let l = eval_snap(l, frame, job, writes)?;
            let r = eval_snap(r, frame, job, writes)?;
            crate::runtime::eval::eval_bin(*op, &l, &r).map_err(|_| SnapErr::Engine)
        }
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => eval_case_snap(
            operand.as_deref(),
            whens,
            else_expr.as_deref(),
            frame,
            job,
            writes,
        ),
        IrExpr::SqlFn { prepared, .. } => eval_sqlfn_snap(prepared, frame, job),
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => eval_funccall_snap(name, args, prepared, frame, job, writes),
        // 静态门禁保证不可达；防御性按行错误处理（主线程原生复现）。
        IrExpr::CursorAttr { .. } | IrExpr::Lookup { .. } => Err(SnapErr::Engine),
    }
}

/// `eval_snap` 的 CASE 分支（与 `eval_case_pure` 同三值语义）。
fn eval_case_snap(
    operand: Option<&IrExpr>,
    whens: &[(IrExpr, IrExpr)],
    else_expr: Option<&IrExpr>,
    frame: &mut Frame,
    job: &WorkerJob<'_>,
    writes: &mut Vec<(AssignTarget, SpinValue)>,
) -> Result<SpinValue, SnapErr> {
    let selected = operand
        .map(|e| eval_snap(e, frame, job, writes))
        .transpose()?;
    for (condition, result) in whens {
        let hit = match &selected {
            Some(value) => {
                crate::runtime::eval::eval_bin(
                    crate::ir::BinOp::Eq,
                    value,
                    &eval_snap(condition, frame, job, writes)?,
                )
                .map_err(|_| SnapErr::Engine)?
                    == SpinValue::Bool(true)
            }
            None => eval_cond_snap(condition, frame, job, writes)? == Some(true),
        };
        if hit {
            return eval_snap(result, frame, job, writes);
        }
    }
    match else_expr {
        Some(e) => eval_snap(e, frame, job, writes),
        None => Ok(SpinValue::Null),
    }
}

/// `eval_snap` 的三值条件（`JumpIf*` 用）。
fn eval_cond_snap(
    e: &IrExpr,
    frame: &mut Frame,
    job: &WorkerJob<'_>,
    writes: &mut Vec<(AssignTarget, SpinValue)>,
) -> Result<Option<bool>, SnapErr> {
    match eval_snap(e, frame, job, writes)? {
        SpinValue::Null => Ok(None),
        SpinValue::Bool(b) => Ok(Some(b)),
        _other => Err(SnapErr::Engine),
    }
}

/// `SqlFn` 快照路径：解析模板绑定 → 标量键探测。
fn eval_sqlfn_snap(
    prepared: &Result<crate::ir::ScalarSql, EngineError>,
    frame: &mut Frame,
    job: &WorkerJob<'_>,
) -> Result<SpinValue, SnapErr> {
    let template = prepared.as_ref().map_err(|_| SnapErr::Engine)?;
    let binds = resolve_binds(&template.binds, frame).map_err(|_| SnapErr::Engine)?;
    probe_scalar(job, &template.sql, binds)
}

/// `FuncCall` 快照路径：先求值全部实参（`eval_funccall` 同序），
/// 再按目录分派——Function 例程键 / 内建 prepared 标量键。
fn eval_funccall_snap(
    name: &str,
    args: &[IrExpr],
    prepared: &Result<String, EngineError>,
    frame: &mut Frame,
    job: &WorkerJob<'_>,
    writes: &mut Vec<(AssignTarget, SpinValue)>,
) -> Result<SpinValue, SnapErr> {
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_snap(a, frame, job, writes)?);
    }
    match job.catalog.lookup(name) {
        Some(r) if r.kind == RoutineKind::Function => {
            let key = routine_key(name, &vals);
            match job.calls.probe(&key) {
                Some(CallOutcome::Routine { ret, outs }) => {
                    apply_call_outs(frame, writes, r, args, &outs)?;
                    Ok(ret)
                }
                Some(CallOutcome::Failed) | Some(CallOutcome::Scalar(_)) => Err(SnapErr::Yield),
                None => Err(SnapErr::Need(PendingCall::Routine {
                    name: name.to_string(),
                    args: vals,
                })),
            }
        }
        // Procedure 等非 Function：静态已拒，防御性交权。
        Some(_) => Err(SnapErr::Yield),
        None => {
            let sql = prepared.as_ref().map_err(|_| SnapErr::Engine)?.clone();
            let mut binds = BindMap::new();
            for (i, v) in vals.into_iter().enumerate() {
                binds.insert(format!("$p{i}"), v);
            }
            probe_scalar(job, &sql, binds)
        }
    }
}

/// 标量键探测：命中取值；`Failed`/类型不符交权；未命中 `Need`。
fn probe_scalar(job: &WorkerJob<'_>, sql: &str, binds: BindMap) -> Result<SpinValue, SnapErr> {
    let key = scalar_key(sql, &binds);
    let req = PendingCall::Scalar {
        sql: sql.to_string(),
        binds,
    };
    match job.calls.probe(&key) {
        Some(CallOutcome::Scalar(v)) => Ok(v),
        Some(CallOutcome::Failed) | Some(CallOutcome::Routine { .. }) => Err(SnapErr::Yield),
        None => Err(SnapErr::Need(req)),
    }
}

/// 例程 OUT/INOUT 写回（`eval_funccall` 同口径）：仅 `mode != In`
/// 形参、实参为 Var/Field 目标时写回调用方槽位；写值记入写日志。
fn apply_call_outs(
    frame: &mut Frame,
    writes: &mut Vec<(AssignTarget, SpinValue)>,
    routine: &crate::catalog::CompiledRoutine,
    args: &[IrExpr],
    outs: &[Option<SpinValue>],
) -> Result<(), SnapErr> {
    for (i, p) in routine.params.iter().enumerate() {
        if p.mode == ParamMode::In {
            continue;
        }
        let target = match args.get(i) {
            Some(IrExpr::Var(v)) => AssignTarget::Var(v.clone()),
            Some(IrExpr::Field { rec, field }) => AssignTarget::Field {
                rec: rec.clone(),
                field: field.clone(),
            },
            _ => continue,
        };
        if let Some(v) = outs.get(i).and_then(|o| o.as_ref()) {
            frame.assign(&target, v.clone())?;
            writes.push((target, v.clone()));
        }
    }
    Ok(())
}

/// `assign` 同口径：Var 目标按 `var_types` + `implicit_assignment_loose`
/// coerce；Field 目标原样写入。返回写入值供写日志记录。
fn assign_coerced(
    job: &WorkerJob<'_>,
    frame: &mut Frame,
    target: &AssignTarget,
    v: SpinValue,
) -> Result<SpinValue, EngineError> {
    let v = match target {
        AssignTarget::Var(name) => {
            crate::value::coerce_for_slot(&v, job.var_types.get(&name.0), job.loose)?
        }
        AssignTarget::Field { .. } => v,
    };
    frame.assign(target, v.clone())?;
    Ok(v)
}

/// SELECT INTO 行数语义（与 `sqlstmts::write_into` 同口径，零数据库版）：
/// 0 行 NO_DATA_FOUND / >1 行 TOO_MANY_ROWS / INTO 超列 BIZ-0126。
fn write_into(
    job: &WorkerJob<'_>,
    frame: &mut Frame,
    into: &[AssignTarget],
    rows: &[Row],
    writes: &mut Vec<(AssignTarget, SpinValue)>,
) -> Result<(), EngineError> {
    if rows.is_empty() {
        return Err(EngineError::no_data_found());
    }
    if rows.len() > 1 {
        return Err(EngineError::too_many_rows());
    }
    let row = &rows[0];
    if into.len() > row.len() {
        return Err(EngineError::business(
            126,
            format!(
                "INTO targets ({}) exceed columns ({})",
                into.len(),
                row.len()
            ),
        ));
    }
    for (i, target) in into.iter().enumerate() {
        let v = row.get(i).cloned().unwrap_or(SpinValue::Null);
        let v = assign_coerced(job, frame, target, v)?;
        writes.push((target.clone(), v));
    }
    Ok(())
}
