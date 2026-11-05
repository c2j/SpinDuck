//! 解释器主循环：显式步进计数；SQL 语句经 SqlExecutor 接缝下发（M3）。

use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor, TraceEvent, TraceSink};
use crate::ir::{AssignTarget, IrProg, IrStmt, VarName};
use crate::runtime::frame::Frame;
use crate::runtime::frame::{eval, eval_cond};
use crate::value::SpinValue;

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
    run_with_trace(prog, frame, ex, &mut TraceNoop)
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
    loop {
        let mut ctx = ExecCtx {
            ex,
            sink,
            cat,
            depth,
        };
        match run_inner(prog, frame, &mut ctx, &mut traps, &mut pc, &mut steps) {
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
                            frame.set(
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
struct ExecCtx<'a> {
    ex: &'a dyn SqlExecutor,
    sink: &'a mut dyn TraceSink,
    cat: &'a crate::catalog::RoutineCatalog,
    depth: usize,
}

/// 内层执行驱动（traps/pc/steps 由外层持有，支持跨重试持久化）。
fn run_inner(
    prog: &IrProg,
    frame: &mut Frame,
    ctx: &mut ExecCtx<'_>,
    traps: &mut Vec<TrapFrame>,
    pc: &mut usize,
    steps: &mut u64,
) -> Result<RunOutcome, EngineError> {
    let ex: &dyn SqlExecutor = ctx.ex;
    let cat: &crate::catalog::RoutineCatalog = ctx.cat;
    let depth: usize = ctx.depth;
    let sink: &mut dyn TraceSink = ctx.sink;
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
        sink.on_event(spinduck_trace_event(st));
        match st {
            IrStmt::Nop => *pc += 1,
            // 事务语句即时作用于当前事务并重开（最外层统一边界见 engine.call）；
            // 不再是空操作——异常处理器中的 ROLLBACK 由此具备撤销已执行 DML 的语义
            IrStmt::TxCommit => {
                ex.execute("COMMIT", &BindMap::new())?;
                ex.execute("BEGIN TRANSACTION", &BindMap::new())?;
                *pc += 1;
            }
            IrStmt::TxRollback => {
                ex.execute("ROLLBACK", &BindMap::new())?;
                ex.execute("BEGIN TRANSACTION", &BindMap::new())?;
                *pc += 1;
            }
            IrStmt::Assign { target, expr } => {
                let v = {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    eval(expr, &env)?
                };
                assign(frame, target, v)?;
                *pc += 1;
            }
            IrStmt::Jump(to) => *pc = *to,
            IrStmt::JumpIfNot { cond, to } => {
                let t = {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    eval_cond(cond, &env)?
                };
                *pc = if t == Some(true) { pc_cur + 1 } else { *to };
            }
            IrStmt::JumpIf { cond, to } => {
                let t = {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    eval_cond(cond, &env)?
                };
                *pc = if t == Some(true) { *to } else { pc_cur + 1 };
            }
            IrStmt::SqlQuery {
                sql,
                binds,
                into,
                fn_sites,
            } => {
                materialize_fn_sites(fn_sites, frame, ex, cat, depth)?;
                let bound = resolve_binds(binds, frame)?;
                sink.on_event(TraceEvent::SqlExec {
                    sql: sql.clone(),
                    binds: bound.clone(),
                    elapsed_us: 0,
                });
                let mut stream = ex.query(sql, &bound)?;
                let rows = stream.fetch_many(2);
                match rows.len() {
                    0 => return Err(EngineError::no_data_found()),
                    n if n > 1 => return Err(EngineError::too_many_rows()),
                    1 => {
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
                            assign(frame, target, v)?;
                        }
                        *pc += 1;
                    }
                    _ => unreachable!(),
                }
            }
            IrStmt::SqlExec {
                sql,
                binds,
                fn_sites,
            } => {
                materialize_fn_sites(fn_sites, frame, ex, cat, depth)?;
                let bound = resolve_binds(binds, frame)?;
                sink.on_event(TraceEvent::SqlExec {
                    sql: sql.clone(),
                    binds: bound.clone(),
                    elapsed_us: 0,
                });
                ex.execute(sql, &bound)?;
                *pc += 1;
            }
            IrStmt::SqlExecDynamic { text_expr } => {
                let v = {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    eval(text_expr, &env)?
                };
                eprintln!("[ARM] text={:?}", v.as_str_loose());
                let (sql, _) = prepare_dynamic_sql(&v.as_str_loose(), frame, ex, cat, depth)?;
                sink.on_event(TraceEvent::SqlExec {
                    sql: sql.clone(),
                    binds: BindMap::new(),
                    elapsed_us: 0,
                });
                ex.execute(&sql, &BindMap::new())?;
                *pc += 1;
            }
            IrStmt::CallRoutine {
                name,
                args,
                out_targets,
            } => {
                if depth > 100 {
                    return Err(EngineError::invariant(63, "call depth exceeded (100)"));
                }
                let routine = cat
                    .lookup(name)
                    .ok_or_else(|| {
                        EngineError::business(150, format!("routine {name} not registered"))
                    })?
                    .clone();
                if args.len() != routine.params.len() {
                    return Err(EngineError::business(
                        151,
                        format!(
                            "{} expects {} args, got {}",
                            name,
                            routine.params.len(),
                            args.len()
                        ),
                    ));
                }
                let mut callee = Frame::new();
                for (p, ir_a) in routine.params.iter().zip(args) {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    let v = eval(ir_a, &env)?;
                    callee.set(&VarName(p.name.to_ascii_lowercase()), v);
                }
                let mut noop = crate::exec::TraceNoopSink;
                let out =
                    run_with_catalog(&routine.prog, &mut callee, ex, &mut noop, cat, depth + 1)?;
                // OUT/INOUT 写回：按参数序对应 out_targets（None = 丢弃）
                for (i, p) in routine.params.iter().enumerate() {
                    if p.mode == crate::catalog::ParamMode::In {
                        continue;
                    }
                    if let Some(Some(t)) = out_targets.get(i) {
                        if let Some(v) = callee.get(&VarName(p.name.to_ascii_lowercase())) {
                            frame.assign(t, v.clone())?;
                        }
                    }
                }
                // 过程的 RETURN <expr> 在过程语境下不产生调用方值（v1）
                let _ = out;
                *pc += 1;
            }
            IrStmt::Perform { expr } => {
                {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    eval(expr, &env)?;
                } // 求值并丢弃
                *pc += 1;
            }
            IrStmt::OpenCursor { cursor, source } => {
                use crate::ir::CursorSource;
                let (sql, binds) = match source {
                    CursorSource::Static {
                        sql,
                        binds,
                        fn_sites,
                    } => {
                        materialize_fn_sites(fn_sites, frame, ex, cat, depth)?;
                        let m = resolve_binds(binds, frame)?;
                        (sql.clone(), m)
                    }
                    CursorSource::Dynamic { query_expr } => {
                        // 动态 OPEN FOR：与 EXECUTE IMMEDIATE 同一条管线（统一收口）
                        let v = {
                            let env = crate::runtime::frame::EvalEnv {
                                frame,
                                ex,
                                cat,
                                depth,
                            };
                            eval(query_expr, &env)?
                        };
                        let (sql, _) =
                            prepare_dynamic_sql(&v.as_str_loose(), frame, ex, cat, depth)?;
                        (sql, BindMap::new())
                    }
                };
                let stream = ex.query(&sql, &binds)?;
                frame.open_cursor(cursor, crate::runtime::cursor::CursorState::open(stream));
                sink.on_event(TraceEvent::CursorOpen { schema: Vec::new() });
                *pc += 1;
            }
            IrStmt::Fetch {
                cursor,
                targets,
                record,
            } => {
                let fetch_schema = frame.cursor_schema(cursor);
                let row = frame.cursor_mut(cursor)?.fetch()?;
                match row {
                    Some(r) if *record => {
                        // FOR rec IN <query>：整行按 schema 建 RECORD（GaussDB 语义）
                        let fields = fetch_schema
                            .iter()
                            .cloned()
                            .zip(r.iter().cloned())
                            .collect::<Vec<_>>();
                        let target = targets.first().ok_or_else(|| {
                            EngineError::invariant(47, "FOR-record fetch requires one INTO target")
                        })?;
                        assign(frame, target, SpinValue::Record(fields))?;
                        sink.on_event(TraceEvent::Fetch { row: Some(1) });
                    }
                    Some(r) => {
                        // 位置对位写槽；INTO 目标多于列 → 报错
                        if targets.len() > r.len() {
                            return Err(EngineError::business(
                                126,
                                format!(
                                    "FETCH INTO targets ({}) exceed columns ({})",
                                    targets.len(),
                                    r.len()
                                ),
                            ));
                        }
                        // %ROWTYPE 记录目标：按结果 schema 建 RECORD（首取时，§7.3.2）
                        for t in targets {
                            if let crate::ir::AssignTarget::Field { rec, .. } = t {
                                frame.ensure_record(rec, &fetch_schema);
                            }
                        }
                        for (i, target) in targets.iter().enumerate() {
                            assign(frame, target, r[i].clone())?;
                        }
                        sink.on_event(TraceEvent::Fetch { row: Some(1) });
                    }
                    None => {
                        sink.on_event(TraceEvent::Fetch { row: None });
                    }
                }
                *pc += 1;
            }
            IrStmt::CloseCursor { cursor } => {
                frame.close_cursor(cursor)?;
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
            IrStmt::Raise { message } => {
                let v = {
                    let env = crate::runtime::frame::EvalEnv {
                        frame,
                        ex,
                        cat,
                        depth,
                    };
                    eval(message, &env)?
                };
                return Err(EngineError::business(200, v.as_str_loose()));
            }
            IrStmt::Return { value } => {
                let v = match value {
                    Some(e) => {
                        let env = crate::runtime::frame::EvalEnv {
                            frame,
                            ex,
                            cat,
                            depth,
                        };
                        Some(eval(e, &env)?)
                    }
                    None => None,
                };
                return Ok(RunOutcome::Returned(v));
            }
        }
    }
}

fn assign(frame: &mut Frame, target: &AssignTarget, v: SpinValue) -> Result<(), EngineError> {
    frame.assign(target, v)
}

/// 动态 SQL 统一管线（决策 2025-11-05 收口）：文本求值 → ogsql 解析 →
/// 规则改名 → 方言变换 → 函数下沉规划 → 物化缓存。EXECUTE IMMEDIATE 与
/// OPEN FOR 共用；解析失败显式报错（BIZ-061），不执行未解析文本。
fn prepare_dynamic_sql(
    text: &str,
    frame: &Frame,
    ex: &dyn SqlExecutor,
    cat: &crate::catalog::RoutineCatalog,
    depth: usize,
) -> Result<(String, Vec<crate::sql::FnSite>), EngineError> {
    eprintln!("[DYNT] enter len={}", text.len());
    let tokens = ogsql_parser::token::tokenizer::Tokenizer::new(text)
        .tokenize()
        .map_err(|e| EngineError::business(61, format!("动态 SQL tokenize 失败: {e}")))?;
    let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
    if stmts.is_empty() {
        let head: String = text.chars().take(80).collect();
        return Err(EngineError::business(
            61,
            format!("动态 SQL 解析失败（ogsql-parser 不支持的形态请向其上游提 issue）：{head}"),
        ));
    }
    let fmt = ogsql_parser::formatter::SqlFormatter::new();
    let sql = stmts
        .iter()
        .map(|st| fmt.format_statement(st))
        .collect::<Vec<_>>()
        .join(";\n");
    // 规则改名（nvl→coalesce 等）与静态语句同口径
    let named =
        crate::sql::rewrite_tokens_with_rules(&sql, &Default::default(), cat.sql_functions())?;
    let sql = crate::sql::apply_ast_transforms(&named.text)?;
    let (marked, sites) = crate::sql::plan_sites(
        &sql,
        &Default::default(),
        &Default::default(),
        &cat.routine_names(),
    )?;
    eprintln!("[DYNT] planned sites={}", sites.len());
    materialize_fn_sites(&sites, frame, ex, cat, depth)?;
    Ok((marked, sites))
}

/// SQL 上下文例程调用下沉物化（§8.3 路径②）。
///
/// 语句文本中的调用点已在编译期替换为参数缓存子查询；此处在执行前逐元组
/// 调用例程、填充缓存表（拓扑序保证依赖表先建）。
fn materialize_fn_sites(
    sites: &[crate::sql::FnSite],
    frame: &Frame,
    ex: &dyn SqlExecutor,
    cat: &crate::catalog::RoutineCatalog,
    depth: usize,
) -> Result<(), EngineError> {
    let max = crate::sql::sqlfn::MAX_TUPLES;
    for site in sites {
        let binds = resolve_binds(&site.binds, frame)?;
        let mut st = ex.query(&site.distinct_sql, &binds)?;
        let rows = st.fetch_many(max + 1);
        if rows.len() > max {
            return Err(EngineError::business(
                157,
                format!("例程 {} 的 SQL 上下文参数元组超过上界 {max}", site.name),
            ));
        }
        let arity = site.args.len();
        let mut cols = vec!["v VARCHAR".to_string()];
        for j in 0..arity {
            cols.push(format!("a{j} VARCHAR"));
        }
        ex.execute(
            &format!(
                "CREATE OR REPLACE TEMP TABLE {} ({})",
                site.table,
                cols.join(", ")
            ),
            &BindMap::new(),
        )?;
        for chunk in rows.chunks(500) {
            let mut tuples = Vec::with_capacity(chunk.len());
            for row in chunk {
                let v = call_routine_value(cat, &site.name, row, ex, depth)?;
                let mut items = vec![sql_literal(&v)];
                for cell in row {
                    items.push(sql_literal(cell));
                }
                tuples.push(format!("({})", items.join(", ")));
            }
            ex.execute(
                &format!("INSERT INTO {} VALUES {}", site.table, tuples.join(", ")),
                &BindMap::new(),
            )?;
        }
    }
    Ok(())
}

/// 按值调用已注册函数（SQL 上下文下沉用；OUT 写回不适用）。
fn call_routine_value(
    cat: &crate::catalog::RoutineCatalog,
    name: &str,
    args: &[SpinValue],
    ex: &dyn SqlExecutor,
    depth: usize,
) -> Result<SpinValue, EngineError> {
    let routine = cat
        .lookup(name)
        .ok_or_else(|| EngineError::business(150, format!("routine {name} not registered")))?
        .clone();
    if args.len() != routine.params.len() {
        return Err(EngineError::business(
            151,
            format!(
                "{} expects {} args, got {}",
                name,
                routine.params.len(),
                args.len()
            ),
        ));
    }
    let mut callee = Frame::new();
    for (p, v) in routine.params.iter().zip(args) {
        callee.set(&VarName(p.name.to_ascii_lowercase()), v.clone());
    }
    let mut noop = crate::exec::TraceNoopSink;
    let out = run_with_catalog(&routine.prog, &mut callee, ex, &mut noop, cat, depth + 1)?;
    Ok(match out {
        RunOutcome::Returned(Some(v)) => v,
        _ => SpinValue::Null,
    })
}

/// SpinValue → SQL 字面量（文本型转义；数值/布尔按文本直出）。
fn sql_literal(v: &SpinValue) -> String {
    match v {
        SpinValue::Null => "NULL".to_string(),
        SpinValue::Str(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.as_str_loose(),
    }
}

/// 按名绑定表解析：占位符名 → 帧内变量当前值（未定义变量 = ENG 域断言失败，
/// 因为编译期预扫描已保证集合一致）。
pub(crate) fn resolve_binds(
    binds: &[(String, crate::ir::BindSrc)],
    frame: &Frame,
) -> Result<BindMap, EngineError> {
    let mut m = BindMap::new();
    for (ph, src) in binds {
        let v = match src {
            crate::ir::BindSrc::Var(var) => frame.get(var).cloned().ok_or_else(|| {
                EngineError::invariant(44, format!("bind var {} undefined at runtime", var.0))
            })?,
            crate::ir::BindSrc::Field { rec, field } => {
                // 记录字段：帧内 RECORD 按字段名（小写）取值
                let field_key = field.to_ascii_lowercase();
                match frame.get(rec) {
                    Some(SpinValue::Record(fields)) => fields
                        .iter()
                        .find(|(k, _)| k.to_ascii_lowercase() == field_key)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| {
                            EngineError::invariant(
                                45,
                                format!("record {} has no field {}", rec.0, field),
                            )
                        })?,
                    Some(other) => {
                        return Err(EngineError::invariant(
                            45,
                            format!("{} is not a record: {:?}", rec.0, other),
                        ));
                    }
                    None => {
                        return Err(EngineError::invariant(
                            44,
                            format!("bind record {} undefined at runtime", rec.0),
                        ));
                    }
                }
            }
        };
        m.insert(ph.clone(), v);
    }
    Ok(m)
}

fn spinduck_trace_event(st: &IrStmt) -> TraceEvent {
    TraceEvent::IrStmt {
        variant: variant_name(st),
    }
}

fn variant_name(st: &IrStmt) -> &'static str {
    match st {
        IrStmt::Nop => "Nop",
        IrStmt::SqlExecDynamic { .. } => "SqlExecDynamic",
        IrStmt::Assign { .. } => "Assign",
        IrStmt::Jump(_) => "Jump",
        IrStmt::JumpIfNot { .. } => "JumpIfNot",
        IrStmt::JumpIf { .. } => "JumpIf",
        IrStmt::Raise { .. } => "Raise",
        IrStmt::Return { .. } => "Return",
        IrStmt::TxCommit => "TxCommit",
        IrStmt::TxRollback => "TxRollback",
        IrStmt::SqlQuery { .. } => "SqlQuery",
        IrStmt::SqlExec { .. } => "SqlExec",
        IrStmt::Perform { .. } => "Perform",
        IrStmt::OpenCursor { .. } => "OpenCursor",
        IrStmt::Fetch { .. } => "Fetch",
        IrStmt::CloseCursor { .. } => "CloseCursor",
        IrStmt::CallRoutine { .. } => "CallRoutine",
        IrStmt::PushTrap { .. } => "PushTrap",
        IrStmt::PopTrap => "PopTrap",
    }
}

struct TraceNoop;
impl TraceSink for TraceNoop {
    fn on_event(&mut self, _event: TraceEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FakeExecutor;
    use crate::ir::compile_stmts;
    use crate::ir::VarName;
    use crate::value::SpinValue;
    use ogsql_parser::ast::plpgsql::PlStatement;

    fn run_pl(stmts: Vec<PlStatement>) -> (Frame, Result<RunOutcome, EngineError>) {
        run_pl_with(stmts, &FakeExecutor::default())
    }

    fn run_pl_with(
        stmts: Vec<PlStatement>,
        ex: &FakeExecutor,
    ) -> (Frame, Result<RunOutcome, EngineError>) {
        let prog = compile_stmts(&stmts).expect("compile");
        let mut f = Frame::new();
        let out = run(&prog, &mut f, ex);
        (f, out)
    }

    fn var(name: &str) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::PlVariable(vec![ident(name)])
    }

    fn ident(name: &str) -> ogsql_parser::ast::Ident {
        ogsql_parser::ast::Ident {
            value: name.to_string(),
            quote_style: None,
        }
    }

    fn lit_str(s: &str) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::String(s.to_string()))
    }

    fn lit_int(i: i64) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(i))
    }

    fn bin(
        l: ogsql_parser::ast::Expr,
        op: &str,
        r: ogsql_parser::ast::Expr,
    ) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::BinaryOp {
            left: Box::new(l),
            op: op.to_string(),
            right: Box::new(r),
        }
    }

    fn assign(target: ogsql_parser::ast::Expr, e: ogsql_parser::ast::Expr) -> PlStatement {
        PlStatement::Assignment {
            target,
            expression: e,
        }
    }

    #[test]
    fn should_assign_variables_with_loose_string_coercion() {
        let (f, out) = run_pl(vec![
            assign(var("v_name"), lit_str("prc_trd_hz_byfund")),
            assign(var("v_step"), lit_int(1)),
        ]);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_name".into())),
            Some(&SpinValue::Str("prc_trd_hz_byfund".into()))
        );
        assert_eq!(f.get(&VarName("v_step".into())), Some(&SpinValue::Int(1)));
    }

    #[test]
    fn should_concat_strings_for_log_message_pattern() {
        let (f, out) = run_pl(vec![assign(
            var("v_msg"),
            bin(lit_str("删除出错!"), "||", lit_str("SQLERRM")),
        )]);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_msg".into())),
            Some(&SpinValue::Str("删除出错!SQLERRM".into()))
        );
    }

    #[test]
    fn should_run_if_else_branches() {
        use ogsql_parser::ast::plpgsql::{PlElsif, PlIfStmt};
        let stmt = PlStatement::If(ogsql_parser::ast::Spanned::new(
            PlIfStmt {
                condition: bin(var("v_cnt"), "=", lit_int(0)),
                then_stmts: vec![assign(var("v_result"), lit_str("zero"))],
                elsifs: vec![PlElsif {
                    condition: bin(var("v_cnt"), ">", lit_int(0)),
                    stmts: vec![assign(var("v_result"), lit_str("pos"))],
                }],
                else_stmts: vec![assign(var("v_result"), lit_str("neg"))],
            },
            None,
        ));
        let (f, out) = run_pl(vec![assign(var("v_cnt"), lit_int(5)), stmt]);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_result".into())),
            Some(&SpinValue::Str("pos".into()))
        );
    }

    #[test]
    fn should_exit_loop_on_notfound_pattern() {
        // 模拟游标循环骨架：v_i 计数，EXIT WHEN v_i >= 3（三值逻辑外的确定性路径）
        let body = vec![
            assign(var("v_i"), bin(var("v_i"), "+", lit_int(1))),
            PlStatement::Exit {
                label: None,
                condition: Some(bin(var("v_i"), ">=", lit_int(3))),
            },
        ];
        let stmt = PlStatement::Loop(ogsql_parser::ast::Spanned::new(
            ogsql_parser::ast::plpgsql::PlLoopStmt {
                label: None,
                body,
                end_label: None,
            },
            None,
        ));
        let (f, out) = run_pl(vec![assign(var("v_i"), lit_int(0)), stmt]);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(f.get(&VarName("v_i".into())), Some(&SpinValue::Int(3)));
    }

    #[test]
    fn should_stop_infinite_loop_at_step_budget() {
        let stmt = PlStatement::Loop(ogsql_parser::ast::Spanned::new(
            ogsql_parser::ast::plpgsql::PlLoopStmt {
                label: None,
                body: vec![],
                end_label: None,
            },
            None,
        ));
        let (_, out) = run_pl(vec![assign(var("v_i"), lit_int(0)), stmt]);
        let err = out.unwrap_err();
        assert_eq!(err.code.to_string(), "ENG-0032");
    }

    #[test]
    fn should_raise_business_error_with_message() {
        use ogsql_parser::ast::plpgsql::PlRaiseStmt;
        use ogsql_parser::ast::Spanned;
        let stmt = PlStatement::Raise(Spanned::new(
            PlRaiseStmt {
                level: None,
                message: Some("清除dat_trd_repurchase数据出错".into()),
                sqlstate: None,
                params: vec![],
                options: vec![],
                condname: None,
            },
            None,
        ));
        let (_, out) = run_pl(vec![stmt]);
        let err = out.unwrap_err();
        assert_eq!(err.domain, crate::error::FaultDomain::Business);
        assert!(err.message.contains("清除dat_trd_repurchase"));
    }

    #[test]
    fn should_return_value() {
        let (_, out) = run_pl(vec![PlStatement::Return {
            expression: Some(lit_str("done")),
        }]);
        assert_eq!(
            out.unwrap(),
            RunOutcome::Returned(Some(SpinValue::Str("done".into())))
        );
    }

    #[test]
    fn should_select_into_single_row_assigns_positionally() {
        use ogsql_parser::parser::Parser;
        use ogsql_parser::token::tokenizer::Tokenizer;
        let sql =
            "SELECT t.fund_code, t.cjsl INTO v_fund_code, v_cjsl FROM dat WHERE bcrq = p_i_date";
        let tokens = Tokenizer::new(sql).tokenize().unwrap();
        let parsed = Parser::new(tokens).parse().remove(0);
        let stmt = PlStatement::SqlStatement {
            span: None,
            sql_text: sql.to_string(),
            statement: Box::new(parsed),
        };
        let ex = FakeExecutor::with_rows(
            vec!["fund_code".to_string(), "cjsl".to_string()],
            vec![vec![SpinValue::Str("F0001".into()), SpinValue::Int(100)]],
        );
        let pre = assign(var("p_i_date"), lit_str("20240105"));
        let (f, out) = run_pl_with(vec![pre, stmt], &ex);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_fund_code".into())),
            Some(&SpinValue::Str("F0001".into()))
        );
        assert_eq!(f.get(&VarName("v_cjsl".into())), Some(&SpinValue::Int(100)));
        // 改写产物：INTO 摘除 + 变量占位符化
        let (sql, binds) = &ex.calls()[0];
        assert!(sql.contains("$v_p_i_date"), "sql: {sql}");
        assert!(!sql.to_uppercase().contains(" INTO "));
        assert_eq!(binds["$v_p_i_date"], SpinValue::Str("20240105".into()));
    }

    #[test]
    fn should_raise_no_data_found_on_zero_rows() {
        use ogsql_parser::parser::Parser;
        use ogsql_parser::token::tokenizer::Tokenizer;
        let sql = "SELECT t.flag INTO v_flag FROM swh_routing WHERE t.k = p_i_date";
        let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
            .parse()
            .remove(0);
        let stmt = PlStatement::SqlStatement {
            span: None,
            sql_text: sql.to_string(),
            statement: Box::new(parsed),
        };
        let ex = FakeExecutor::default(); // 无脚本 → 空结果
        let (_, out) = run_pl_with(vec![stmt], &ex);
        let err = out.unwrap_err();
        assert_eq!(err.code.to_string(), "BIZ-0100");
    }

    #[test]
    fn should_raise_too_many_rows_on_multi_rows() {
        use ogsql_parser::parser::Parser;
        use ogsql_parser::token::tokenizer::Tokenizer;
        let sql = "SELECT t.flag INTO v_flag FROM swh_routing WHERE t.k = p_i_date";
        let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
            .parse()
            .remove(0);
        let stmt = PlStatement::SqlStatement {
            span: None,
            sql_text: sql.to_string(),
            statement: Box::new(parsed),
        };
        let ex = FakeExecutor::with_rows(
            vec!["flag".to_string()],
            vec![
                vec![SpinValue::Str("0".into())],
                vec![SpinValue::Str("1".into())],
            ],
        );
        let (_, out) = run_pl_with(vec![stmt], &ex);
        assert_eq!(out.unwrap_err().code.to_string(), "BIZ-0101");
    }

    #[test]
    fn should_execute_dml_with_named_binds() {
        use ogsql_parser::parser::Parser;
        use ogsql_parser::token::tokenizer::Tokenizer;
        let sql = "DELETE FROM dat_fund_cjqs WHERE bcrq = p_i_date AND scdm = '001'";
        let parsed = Parser::new(Tokenizer::new(sql).tokenize().unwrap())
            .parse()
            .remove(0);
        let stmt = PlStatement::SqlStatement {
            span: None,
            sql_text: sql.to_string(),
            statement: Box::new(parsed),
        };
        let ex = FakeExecutor::default();
        // p_i_date 为过程参数：M6 CALL 注入帧；此处以赋值前置等价模拟（参数即帧变量）
        let pre = assign(var("p_i_date"), lit_str("20240105"));
        let (_, out) = run_pl_with(vec![pre, stmt], &ex);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        let (sql, binds) = &ex.calls()[0];
        assert!(sql.starts_with("DELETE FROM"));
        assert_eq!(binds["$v_p_i_date"], SpinValue::Str("20240105".into()));
    }

    #[test]
    fn should_reject_sql_statements_at_m2_scope() {
        // M2 范围外：SQL 类语句**编译期**显式拒绝（M3 纳入）
        let err = compile_stmts(&[PlStatement::Sql("SELECT 1".into())]).unwrap_err();
        assert_eq!(err.domain, crate::error::FaultDomain::Engine);
        assert!(err.message.contains("not yet supported"));
    }
}

#[cfg(test)]
mod m5 {
    use super::*;
    use crate::exec::FakeExecutor;
    use crate::ir::compile_stmts;
    use crate::ir::VarName;

    use crate::runtime::{Frame, RunOutcome};

    use ogsql_parser::ast::plpgsql::PlStatement;
    use ogsql_parser::ast::plpgsql::{PlBlock, PlStatement as PlStmtT};
    fn ident(name: &str) -> ogsql_parser::ast::Ident {
        ogsql_parser::ast::Ident {
            value: name.to_string(),
            quote_style: None,
        }
    }
    fn var(name: &str) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::PlVariable(vec![ident(name)])
    }
    fn lit_str(s: &str) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::String(s.to_string()))
    }
    fn lit_int(i: i64) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(i))
    }
    fn assign(target: ogsql_parser::ast::Expr, e: ogsql_parser::ast::Expr) -> PlStatement {
        PlStatement::Assignment {
            target,
            expression: e,
        }
    }
    fn run_pl_with(
        stmts: Vec<PlStatement>,
        ex: &FakeExecutor,
    ) -> (Frame, Result<RunOutcome, EngineError>) {
        let prog = compile_stmts(&stmts).expect("compile");
        let mut f = Frame::new();
        let out = run(&prog, &mut f, ex);
        (f, out)
    }
    fn exc_block(body: Vec<PlStmtT>, handlers: Vec<(Vec<&str>, Vec<PlStmtT>)>) -> PlStmtT {
        use ogsql_parser::ast::plpgsql::{PlExceptionBlock, PlExceptionHandler};
        use ogsql_parser::ast::Spanned;
        PlStmtT::Block(Spanned::new(
            PlBlock {
                label: None,
                declarations: vec![],
                body,
                exception_block: Some(PlExceptionBlock {
                    handlers: handlers
                        .into_iter()
                        .map(|(conds, stmts)| PlExceptionHandler {
                            conditions: conds.iter().map(|c| c.to_string()).collect(),
                            statements: stmts,
                        })
                        .collect(),
                }),
                end_label: None,
            },
            None,
        ))
    }

    fn select_into(var_name: &str, cond: &str) -> PlStatement {
        use ogsql_parser::parser::Parser;
        use ogsql_parser::token::tokenizer::Tokenizer;
        let sql = format!("SELECT t.flag INTO {var_name} FROM swh_routing WHERE t.k = {cond}");
        let parsed = Parser::new(Tokenizer::new(&sql).tokenize().unwrap())
            .parse()
            .remove(0);
        PlStatement::SqlStatement {
            span: None,
            sql_text: sql,
            statement: Box::new(parsed),
        }
    }

    #[test]
    fn should_catch_no_data_found_with_named_handler() {
        let ex = FakeExecutor::default(); // 0 行 → NO_DATA_FOUND
        let pre = assign(var("p_i_date"), lit_str("20240105"));
        let block = exc_block(
            vec![select_into("v_flag", "p_i_date")],
            vec![(
                vec!["no_data_found"],
                vec![assign(var("v_flag"), lit_str("fallback"))],
            )],
        );
        let after = assign(var("v_after"), lit_str("yes"));
        let (f, out) = run_pl_with(vec![pre, block, after], &ex);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_flag".into())),
            Some(&SpinValue::Str("fallback".into()))
        );
        // 处理器完成后继续块外语句
        assert_eq!(
            f.get(&VarName("v_after".into())),
            Some(&SpinValue::Str("yes".into()))
        );
    }

    #[test]
    fn should_propagate_unmatched_error_to_outer_others() {
        let ex = FakeExecutor::default();
        let pre = assign(var("p_i_date"), lit_str("20240105"));
        // 内层：只声明 too_many_rows；实际是 no_data_found → 不匹配 → 上抛
        let inner = exc_block(
            vec![select_into("v_inner", "p_i_date")],
            vec![(
                vec!["too_many_rows"],
                vec![assign(var("v_inner"), lit_str("x"))],
            )],
        );
        // 外层：WHEN OTHERS 兜底
        let outer = exc_block(
            vec![inner],
            vec![(vec!["others"], vec![assign(var("v_handled"), lit_int(1))])],
        );
        let (f, out) = run_pl_with(vec![pre, outer], &ex);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_handled".into())),
            Some(&SpinValue::Int(1))
        );
    }

    #[test]
    fn should_re_raise_from_handler_to_outer_trap() {
        let ex = FakeExecutor::default();
        let pre = assign(var("p_i_date"), lit_str("20240105"));
        // 内层处理器捕获后再次 RAISE → 向外层传播
        let inner = exc_block(
            vec![select_into("v_inner", "p_i_date")],
            vec![(
                vec!["no_data_found"],
                vec![PlStatement::Raise(ogsql_parser::ast::Spanned::new(
                    ogsql_parser::ast::plpgsql::PlRaiseStmt {
                        level: None,
                        message: Some("re-raised".into()),
                        params: vec![],
                        options: vec![],
                        condname: None,
                        sqlstate: None,
                    },
                    None,
                ))],
            )],
        );
        let outer = exc_block(
            vec![inner],
            vec![(
                vec!["others"],
                vec![assign(var("v_outer"), lit_str("outer_caught"))],
            )],
        );
        let (f, out) = run_pl_with(vec![pre, outer], &ex);
        assert_eq!(out.unwrap(), RunOutcome::Completed);
        assert_eq!(
            f.get(&VarName("v_outer".into())),
            Some(&SpinValue::Str("outer_caught".into()))
        );
    }

    #[test]
    fn should_stop_propagation_when_no_handler_anywhere() {
        let ex = FakeExecutor::default();
        let pre = assign(var("p_i_date"), lit_str("20240105"));
        let (_, out) = run_pl_with(vec![pre, select_into("v_flag", "p_i_date")], &ex);
        let err = out.unwrap_err();
        assert_eq!(err.code.to_string(), "BIZ-0100");
    }
}

#[cfg(all(test, feature = "native"))]
mod kernel_tests {
    use super::*;
    use crate::ir::{compile_block_with, VarName};
    use crate::rules::RuleRegistry;
    use crate::runtime::Frame;
    use crate::value::SpinValue;
    use ogsql_parser::ast::plpgsql::{
        PlBlock, PlCursorDecl, PlDataType, PlDeclaration, PlStatement, PlVarDecl,
    };
    use ogsql_parser::parser::Parser;
    use ogsql_parser::token::tokenizer::Tokenizer;

    fn parse_statement(sql: &str) -> ogsql_parser::ast::Statement {
        let tokens = Tokenizer::new(sql).tokenize().unwrap();
        Parser::new(tokens).parse().remove(0)
    }

    fn ident(name: &str) -> ogsql_parser::ast::Ident {
        ogsql_parser::ast::Ident {
            value: name.to_string(),
            quote_style: None,
        }
    }

    fn var(name: &str) -> ogsql_parser::ast::Expr {
        ogsql_parser::ast::Expr::PlVariable(vec![ident(name)])
    }

    fn assign(target: ogsql_parser::ast::Expr, e: ogsql_parser::ast::Expr) -> PlStatement {
        PlStatement::Assignment {
            target,
            expression: e,
        }
    }

    /// 端到端探针（真实内核）：静态游标 OPEN/FETCH/EXIT 循环 + 记录字段访问。
    /// 对应清算过程主循环形态（§7.3），行为口径对齐 scripts/sim_prc_trd_hz_byfund.py。
    /// M6 端到端（真实内核）：Engine 注册 PACKAGE BODY 式过程 → CALL（OUT 写回）
    /// → 过程间互调（CALL 另一过程）→ SQL 内 PL 函数经 UDF 桥接（threads=1）。
    #[test]
    fn should_call_procs_with_out_params_and_udf_bridge_on_real_kernel() {
        use crate::engine::Engine;
        use crate::exec::TraceNoopSink;
        use crate::ir::VarName;
        use crate::runtime::Frame;
        use crate::value::SpinValue;

        let mut eng = Engine::new(
            Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.register("CREATE TABLE dat(a VARCHAR, n INT)").unwrap();
        eng.register("INSERT INTO dat VALUES ('x', 2)").unwrap();
        eng.register("INSERT INTO dat VALUES ('y', 3)").unwrap();
        // 函数：SQL 内将被桥接（threads=1 约束下直接 UDF 通道）
        eng.register(
            "CREATE OR REPLACE FUNCTION fnc_double(p IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN p * 2; END;",
        )
        .unwrap();
        // 过程：IN/OUT + 调用函数（SQL 内 UDF 桥接路径）
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_calc(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 2; END;",
        )
        .unwrap();
        // 过程：CALL 另一过程（ProcedureCall 互调）
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_fwd(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN prc_calc(p_i, p_o); END;",
        )
        .unwrap();

        let r = eng
            .call("prc_fwd", &[SpinValue::Int(21), SpinValue::Null])
            .unwrap();
        assert_eq!(
            r.outs.get("p_o"),
            Some(&SpinValue::Int(42)),
            "OUT 写回: {:?}",
            r.outs
        );
        // 函数 PL 直调（Engine::call → RETURN 值）
        let fr = eng.call("fnc_double", &[SpinValue::Int(21)]).unwrap();
        assert_eq!(fr.return_value, Some(SpinValue::Int(42)));
        let _ = TraceNoopSink;
        let _ = Frame::new();
        let _ = VarName("x".into());
    }

    /// 端到端探针：显式游标空集 + EXIT WHEN %NOTFOUND + 记录引用（语料
    /// prc_trd_hz_byfund 风险金循环形态）：空集时循环体一次都不执行。
    #[test]
    fn should_skip_loop_body_when_cursor_is_empty() {
        use crate::engine::Engine;
        use crate::rules::RuleRegistry;

        let mut eng = Engine::new(
            Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.ex
            .execute("CREATE TABLE venture(a VARCHAR)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_empty IS \
             DECLARE \
               CURSOR c_v IS SELECT a FROM venture WHERE 1 = 0; \
               r c_v%ROWTYPE; \
               v VARCHAR2(10) := ''; \
             BEGIN \
               OPEN c_v; \
               LOOP \
                 FETCH c_v INTO r; \
                 EXIT WHEN c_v%NOTFOUND; \
                 v := 'x' || r.a; \
               END LOOP; \
               CLOSE c_v; \
             END;",
        )
        .unwrap();
        let out = eng.call("prc_empty", &[]).unwrap();
        assert_eq!(
            out.outs.len(),
            0,
            "空集不应进入循环体（否则 r 未赋值即报错）"
        );
    }

    /// 端到端探针：SQL 上下文例程调用下沉（§8.3 ②）——UPDATE/SELECT 中的
    /// PL 函数按参数元组缓存化，INSERT VALUES 行内求值；结果必须与逐行语义一致。
    #[test]
    fn should_execute_pl_function_calls_inside_sql_statements() {
        use crate::engine::Engine;
        use crate::rules::RuleRegistry;
        use crate::value::SpinValue;

        let mut eng = Engine::new(
            Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        let ex = &mut eng.ex;
        ex.execute("CREATE TABLE items(k VARCHAR, v INT)", &BindMap::new())
            .unwrap();
        ex.execute(
            "CREATE TABLE lookup_tbl(k VARCHAR, tag VARCHAR)",
            &BindMap::new(),
        )
        .unwrap();
        ex.execute(
            "INSERT INTO items VALUES ('a', 1), ('b', 2)",
            &BindMap::new(),
        )
        .unwrap();
        ex.execute(
            "INSERT INTO lookup_tbl VALUES ('a', 'A'), ('b', 'B')",
            &BindMap::new(),
        )
        .unwrap();
        eng.register(
            "CREATE OR REPLACE FUNCTION fnc_tag(p VARCHAR2) RETURN VARCHAR2 IS \
             v VARCHAR2(10); \
             BEGIN \
               SELECT tag INTO v FROM lookup_tbl WHERE k = p; \
               RETURN v; \
             END;",
        )
        .unwrap();
        // 集合形态：UPDATE ... SET v = length(fnc(k)) * 10
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_upd IS \
             BEGIN \
               UPDATE items SET v = length(fnc_tag(k)) * 10 WHERE v = 1; \
               COMMIT; \
             END;",
        )
        .unwrap();
        eng.call("prc_upd", &[]).unwrap();
        // 行内形态：INSERT ... VALUES (v, length(fnc(v)) * 100)
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_ins IS \
             DECLARE vk VARCHAR2(10) := 'a'; \
             BEGIN \
               INSERT INTO items(k, v) VALUES (vk, length(fnc_tag(vk)) * 100); \
               COMMIT; \
             END;",
        )
        .unwrap();
        eng.call("prc_ins", &[]).unwrap();

        let mut rs = eng
            .ex
            .query("SELECT k, v FROM items ORDER BY k, v", &BindMap::new())
            .unwrap();
        let rows: Vec<Vec<String>> = rs
            .fetch_many(10)
            .iter()
            .map(|r| r.iter().map(|v| v.as_str_loose()).collect())
            .collect();
        assert_eq!(
            rows,
            vec![
                vec!["a".to_string(), "10".to_string()],
                vec!["a".to_string(), "100".to_string()],
                vec!["b".to_string(), "2".to_string()],
            ],
            "UPDATE 命中行按 fnc 结果更新、INSERT 行内求值；未命中行不变"
        );
        let _ = SpinValue::Null;
    }

    /// 端到端探针：@标记函数整体下沉（SqlFn）——嵌套日期算术的中间类型
    /// （TIMESTAMP）保留在 SQL 侧，不经 SpinValue 文本化（语料 step4 形态）。
    #[test]
    fn should_evaluate_nested_date_functions_via_sqlfn_on_real_kernel() {
        use crate::engine::Engine;
        use crate::rules::RuleRegistry;
        use crate::value::SpinValue;

        let rules = RuleRegistry::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
        )
        .expect("加载种子规则");
        let mut eng = Engine::new(
            Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
            rules,
        );
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_datefn(p_o OUT VARCHAR2) IS \
             DECLARE v_days NUMBER(3) := 90; \
             BEGIN \
               p_o := to_char(to_date('20250715', 'YYYYMMDD') + v_days, 'YYYYMMDD'); \
             END;",
        )
        .unwrap();
        let r = eng.call("prc_datefn", &[SpinValue::Null]).unwrap();
        assert_eq!(
            r.outs.get("p_o"),
            Some(&SpinValue::Str("20251013".into())),
            "20250715 + 90 天 = 20251013"
        );
    }

    /// 端到端探针：`rownum = 1` 首行谓词改写为 LIMIT 1（语料 42 处，§9 规则）。
    /// 覆盖两种语料形态：平铺 WHERE 与"内层 ORDER BY 子查询 + 外层 rownum"。
    #[test]
    fn should_rewrite_rownum_first_row_predicate_on_real_kernel() {
        use crate::engine::Engine;
        use crate::rules::RuleRegistry;
        use crate::value::SpinValue;

        // patterns_rownum_first_row 默认即 true（DuckDB 无 rownum，不改写必然报错）
        let rules = RuleRegistry::default();
        let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE t(k VARCHAR, v INT)", &BindMap::new())
            .unwrap();
        for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
            ex.execute(
                &format!("INSERT INTO t VALUES ('{k}', {v})"),
                &BindMap::new(),
            )
            .unwrap();
        }
        let mut eng = Engine::new(Box::new(ex), rules);

        // 形态 1：平铺 WHERE rownum = 1（有序 LIMIT 语义）
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_first(p_o OUT VARCHAR) IS \
             BEGIN SELECT k INTO p_o FROM t WHERE rownum = 1 ORDER BY k; END;",
        )
        .unwrap();
        let r = eng.call("prc_first", &[SpinValue::Null]).unwrap();
        assert_eq!(r.outs.get("p_o"), Some(&SpinValue::Str("a".into())));

        // 形态 2：语料典型——内层 ORDER BY 子查询 + 外层 rownum = 1
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_top(p_o OUT INT) IS \
             BEGIN SELECT v INTO p_o FROM (SELECT v FROM t ORDER BY v DESC) WHERE rownum = 1; \
             END;",
        )
        .unwrap();
        let r = eng.call("prc_top", &[SpinValue::Null]).unwrap();
        assert_eq!(
            r.outs.get("p_o"),
            Some(&SpinValue::Int(3)),
            "内层 DESC 首行"
        );
    }

    /// 端到端探针：PL 上下文的内建函数下沉内核（ABS/SUBSTR/COALESCE/ROUND）。
    #[test]
    fn should_evaluate_builtin_functions_in_pl_context_on_real_kernel() {
        use crate::engine::Engine;
        use crate::value::SpinValue;

        let rules = RuleRegistry::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
        )
        .expect("加载种子规则");
        let mut eng = Engine::new(
            Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
            rules,
        );
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_builtin(p_o OUT VARCHAR2) IS \
             DECLARE v_s VARCHAR2(20); \
             BEGIN \
               v_s := SUBSTR('ABCDEF', 1, 2) || '00' || ABS(-3) || ROUND(2.345, 2); \
               p_o := v_s; \
             END;",
        )
        .unwrap();
        let r = eng.call("prc_builtin", &[SpinValue::Null]).unwrap();
        let got = r.outs.get("p_o").cloned().unwrap_or(SpinValue::Null);
        assert_eq!(
            got.as_str_loose(),
            "AB0032.35",
            "内建函数应在 PL 上下文内经内核求值"
        );
    }

    /// 端到端探针：FOR rec IN <query>（隐式游标脱糖，§7.3.4）真实内核语义。
    #[test]
    fn should_run_for_loop_over_query_on_real_kernel() {
        use crate::engine::Engine;
        use crate::value::SpinValue;

        let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE dat(a VARCHAR, n INT)", &BindMap::new())
            .unwrap();
        for i in 1..=3 {
            ex.execute(
                &format!("INSERT INTO dat VALUES ('r{i}', {i})"),
                &BindMap::new(),
            )
            .unwrap();
        }
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_for(p_o OUT NUMBER) IS \
             DECLARE v_sum NUMBER := 0; \
             BEGIN \
               FOR r IN (SELECT n FROM dat ORDER BY n) LOOP \
                 v_sum := v_sum + r.n; \
               END LOOP; \
               p_o := v_sum; \
             END;",
        )
        .unwrap();
        let r = eng
            .call("prc_for", &[SpinValue::Null])
            .expect("FOR rec IN query 应可执行");
        assert_eq!(r.outs.get("p_o"), Some(&SpinValue::Int(6)), "{:?}", r.outs);
    }

    #[test]
    fn should_run_cursor_loop_with_record_field_access_on_real_kernel() {
        let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE dat(a VARCHAR, n INT)", &BindMap::new())
            .unwrap();
        for i in 1..=3 {
            ex.execute(
                &format!("INSERT INTO dat VALUES ('row{i}', {i})"),
                &BindMap::new(),
            )
            .unwrap();
        }

        let sel = parse_statement("SELECT a, n FROM dat ORDER BY n");
        let block = PlBlock {
            label: None,
            declarations: vec![
                PlDeclaration::Variable(PlVarDecl {
                    name: "v_sum".into(),
                    data_type: PlDataType::TypeName("NUMBER".into()),
                    default: Some(ogsql_parser::ast::Expr::Literal(
                        ogsql_parser::ast::Literal::Integer(0),
                    )),
                    constant: false,
                    not_null: false,
                    collate: None,
                }),
                PlDeclaration::Cursor(PlCursorDecl {
                    name: "c_dat".into(),
                    arguments: vec![],
                    return_type: None,
                    query: "SELECT a, n FROM dat ORDER BY n".into(),
                    parsed_query: Some(Box::new(sel)),
                    scrollable: false,
                }),
            ],
            body: vec![
                assign(
                    var("v_sum"),
                    ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(0)),
                ),
                assign(
                    var("r_n"),
                    ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(0)),
                ),
                PlStatement::Open(ogsql_parser::ast::Spanned::new(
                    ogsql_parser::ast::plpgsql::PlOpenStmt {
                        cursor: var("c_dat"),
                        kind: ogsql_parser::ast::plpgsql::PlOpenKind::Simple { arguments: vec![] },
                    },
                    None,
                )),
                PlStatement::Loop(ogsql_parser::ast::Spanned::new(
                    ogsql_parser::ast::plpgsql::PlLoopStmt {
                        label: None,
                        body: vec![
                            PlStatement::Fetch(ogsql_parser::ast::Spanned::new(
                                ogsql_parser::ast::plpgsql::PlFetchStmt {
                                    cursor: var("c_dat"),
                                    direction: None,
                                    bulk_collect: false,
                                    into: vec![
                                        ogsql_parser::ast::Expr::ColumnRef(vec![
                                            ident("r"),
                                            ident("a"),
                                        ]),
                                        ogsql_parser::ast::Expr::ColumnRef(vec![
                                            ident("r"),
                                            ident("n"),
                                        ]),
                                    ],
                                },
                                None,
                            )),
                            PlStatement::Exit {
                                label: None,
                                condition: Some(ogsql_parser::ast::Expr::CursorAttribute {
                                    cursor: Box::new(var("c_dat")),
                                    attribute: ogsql_parser::ast::CursorAttributeKind::NotFound,
                                }),
                            },
                            assign(
                                var("v_sum"),
                                ogsql_parser::ast::Expr::BinaryOp {
                                    left: Box::new(var("v_sum")),
                                    op: "+".into(),
                                    right: Box::new(ogsql_parser::ast::Expr::ColumnRef(vec![
                                        ident("r"),
                                        ident("n"),
                                    ])),
                                },
                            ),
                        ],
                        end_label: None,
                    },
                    None,
                )),
                PlStatement::Close {
                    cursor: var("c_dat"),
                },
            ],
            exception_block: None,
            end_label: None,
        };

        let prog = compile_block_with(&block, &RuleRegistry::default()).unwrap();
        let mut frame = Frame::new();
        let out = run(&prog, &mut frame, &ex).unwrap();
        assert_eq!(out, RunOutcome::Completed);
        assert_eq!(
            frame.get(&VarName("v_sum".into())),
            Some(&SpinValue::Int(6)) // 1+2+3
        );
    }
}
