//! 解释器主循环：显式步进计数；SQL 语句经 SqlExecutor 接缝下发（M3）。
//!
//! 规模拆分（AGENTS §3）：本文件只保留运行入口、trap 展开主循环与语句分发；
//! 各语句族的执行体在同级 `runtime/interp/*.rs`，测试模块同样拆到该目录
//! ——只搬移代码，行为与断言一字不改。

use crate::error::EngineError;
use crate::exec::{SqlExecutor, TraceSink};
use crate::ir::{AssignTarget, IrProg, IrStmt, VarName};
use crate::runtime::frame::Frame;
use crate::value::SpinValue;

mod binds;
mod cursors;
mod dynamic;
mod flow;
mod routine;
mod sqlstmts;
mod trace;

pub(crate) use binds::resolve_binds;
use trace::spinduck_trace_event;

/// 单次运行结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Completed,
    Returned(Option<SpinValue>),
}

const MAX_STEPS: u64 = 1_000_000;

/// 运行 IR 程序（SQL 语句经 executor 下发；测试注入 FakeExecutor）。
pub fn run(
    prog: &IrProg,
    frame: &mut Frame,
    ex: &dyn SqlExecutor,
) -> Result<RunOutcome, EngineError> {
    run_with_trace(prog, frame, ex, &mut trace::TraceNoop)
}

/// 异常处理帧（§3.5）：处理器表 + resume PC（PopTrap 后的 Jump(resume)）。
struct TrapFrame {
    entries: Vec<crate::ir::TrapEntry>,
}

/// 带 trace 的运行（§2.1/§10.5：core 一次埋点，CLI/监控共用）。
/// 错误模型（§3.5）：指令错误 → 自顶向下找匹配处理器 → pc=处理器体重入主循环；
/// 无匹配 → 向外层 trap 展开，最终无处理器则返回错误。
pub fn run_with_trace(
    prog: &IrProg,
    frame: &mut Frame,
    ex: &dyn SqlExecutor,
    sink: &mut dyn TraceSink,
) -> Result<RunOutcome, EngineError> {
    let cat = crate::catalog::RoutineCatalog::new();
    run_with_catalog(prog, frame, ex, sink, &cat, 0)
}

/// 带例程目录与深度的运行（M6：CALL/FuncCall 例程直调）。
pub fn run_with_catalog(
    prog: &IrProg,
    frame: &mut Frame,
    ex: &dyn SqlExecutor,
    sink: &mut dyn TraceSink,
    cat: &crate::catalog::RoutineCatalog,
    depth: usize,
) -> Result<RunOutcome, EngineError> {
    let mut traps: Vec<TrapFrame> = Vec::new();
    let mut pc = 0usize;
    let mut steps = 0u64;
    let mut env = Env {
        frame,
        ex,
        sink,
        cat,
        depth,
    };
    loop {
        match run_inner(prog, &mut env, &mut traps, &mut pc, &mut steps) {
            Ok(out) => return Ok(out),
            Err(e) => {
                // 自顶向下找匹配处理器；进入处理器前写 SQLERRM/SQLCODE 帧变量
                let mut handled = false;
                while !traps.is_empty() {
                    // 循环条件保证非空；仍以模式匹配防御（§5：内核状态一律 ? / 显式错误）
                    let hit = traps.last().and_then(|top| {
                        top.entries
                            .iter()
                            .find(|en| en.conds.iter().any(|c| crate::error::cond_matches(c, &e)))
                    });
                    match hit {
                        Some(h) => {
                            // 消费该 trap（丢弃其上全部未弹出的内层 trap）并重入处理器体；
                            // 处理器可见 SQLERRM（错误消息）
                            let body_start = h.body_start;
                            traps.pop();
                            env.frame.set(
                                &VarName("__sqlerrm".to_string()),
                                SpinValue::Str(e.message.clone()),
                            );
                            pc = body_start;
                            handled = true;
                            break;
                        }
                        None => {
                            traps.pop(); // 无匹配 → 展开到外层
                        }
                    }
                }
                if !handled {
                    return Err(e);
                }
                // pc 已指向处理器体；handler 末尾的 Jump(resume) 负责跳回 resume
            }
        }
    }
}

/// 执行上下文（§3：≥5 参数一律结构体；接缝引用保持 `SqlExecutor`/`TraceSink`）。
pub(super) struct Env<'a> {
    pub frame: &'a mut Frame,
    pub ex: &'a dyn SqlExecutor,
    pub sink: &'a mut dyn TraceSink,
    pub cat: &'a crate::catalog::RoutineCatalog,
    pub depth: usize,
}

/// 内层执行驱动（traps/pc/steps 由外层持有，支持跨重试持久化）。
fn run_inner(
    prog: &IrProg,
    env: &mut Env<'_>,
    traps: &mut Vec<TrapFrame>,
    pc: &mut usize,
    steps: &mut u64,
) -> Result<RunOutcome, EngineError> {
    loop {
        *steps += 1;
        if *steps > MAX_STEPS {
            return Err(EngineError::invariant(
                32,
                format!("step budget exceeded ({MAX_STEPS}): possible infinite loop"),
            ));
        }
        let pc_cur = *pc;
        let st = match prog.code.get(pc_cur) {
            Some(st) => st,
            // 程序尾部：隐式完成（无 RETURN 的过程）
            None => return Ok(RunOutcome::Completed),
        };
        env.sink.on_event(spinduck_trace_event(st));
        match st {
            IrStmt::Nop => *pc += 1,
            // 事务语句即时作用于当前事务并重开（最外层统一边界见 engine.call）
            IrStmt::TxCommit => {
                flow::tx(env, true)?;
                *pc += 1;
            }
            IrStmt::TxRollback => {
                flow::tx(env, false)?;
                *pc += 1;
            }
            IrStmt::Assign { target, expr } => {
                flow::assign_step(env, target, expr)?;
                *pc += 1;
            }
            IrStmt::Jump(to) => *pc = *to,
            IrStmt::JumpIfNot { cond, to } => {
                let t = flow::cond(env, cond)?;
                *pc = if t == Some(true) { pc_cur + 1 } else { *to };
            }
            IrStmt::JumpIf { cond, to } => {
                let t = flow::cond(env, cond)?;
                *pc = if t == Some(true) { *to } else { pc_cur + 1 };
            }
            IrStmt::SqlQuery {
                sql,
                binds,
                into,
                fn_sites,
            } => {
                sqlstmts::query(env, sql, binds, into, fn_sites)?;
                *pc += 1;
            }
            IrStmt::SqlExec {
                sql,
                binds,
                fn_sites,
            } => {
                sqlstmts::exec(env, sql, binds, fn_sites)?;
                *pc += 1;
            }
            IrStmt::SqlExecDynamic {
                text_expr,
                into_targets,
            } => {
                sqlstmts::exec_dynamic(env, text_expr, into_targets)?;
                *pc += 1;
            }
            IrStmt::CallRoutine {
                name,
                args,
                out_targets,
            } => {
                routine::call(env, name, args, out_targets)?;
                *pc += 1;
            }
            IrStmt::Perform { expr } => {
                flow::perform(env, expr)?;
                *pc += 1;
            }
            IrStmt::OpenCursor { cursor, source } => {
                cursors::open(env, cursor, source)?;
                *pc += 1;
            }
            IrStmt::Fetch {
                cursor,
                targets,
                record,
            } => {
                cursors::fetch(env, cursor, targets, record)?;
                *pc += 1;
            }
            IrStmt::CloseCursor { cursor } => {
                cursors::close(env, cursor)?;
                *pc += 1;
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
            IrStmt::Raise { message } => return Err(flow::raise(env, message)?),
            IrStmt::Return { value } => return flow::return_step(env, value),
        }
    }
}

/// 写槽唯一入口（帧 `assign` 语义含 %ROWTYPE 字段与记录整体赋值）。
pub(super) fn assign(
    frame: &mut Frame,
    target: &AssignTarget,
    v: SpinValue,
) -> Result<(), EngineError> {
    frame.assign(target, v)
}

// 测试模块（随 §3 规模拆分搬出，断言一字未改）。
#[cfg(all(test, feature = "native"))]
mod kernel_tests;
#[cfg(test)]
mod m5;
#[cfg(test)]
mod tests;
