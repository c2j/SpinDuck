//! `explain`：把例程编译成 IR 并以稳定文本形式打印（调试/回归用，非执行）。
//!
//! 输出契约：每行一条指令 `<pc>\t<op>\t<detail>`，`detail` 只含稳定信息
//! （变量名/标签/常量），不含地址或指针，便于 diff 与黄金比对。
//!
//! `--trace <日志>` 时在契约之外追加第四列（关联形态 A）：每条静态 SQL 行尾给出该文本在
//! 真实运行里的命中次数与绑定跨度，动态语句标注"文本在运行期"，末尾反向报出"日志里跑了、
//! 本 IR 里没有"的语句。不给 `--trace` 时输出逐字不变。

use crate::error::EngineError;
use crate::ir::{AssignTarget, BinOp, CursorSource, IrExpr, IrProg, IrStmt};
use crate::trace::{annotation, flat, histogram, sql_id, unmatched, Hit};
use crate::value::SpinValue;
use std::collections::{BTreeMap, BTreeSet};

/// 渲染程序为逐行文本。
pub fn render(prog: &IrProg) -> Vec<String> {
    lines(prog).into_iter().map(|l| l.text).collect()
}

/// 一条指令：稳定行 + 对齐审计日志所需的指纹。
struct IrLine {
    text: String,
    /// 编译期确定的 SQL 文本指纹（`None` = 该行没有静态 SQL）。
    sql_id: Option<String>,
    /// 文本要到运行期才知道（EXECUTE IMMEDIATE / OPEN FOR 的动态源）。
    dynamic: bool,
}

fn lines(prog: &IrProg) -> Vec<IrLine> {
    prog.code
        .iter()
        .enumerate()
        .map(|(pc, st)| {
            let (op, detail) = describe(st);
            IrLine {
                text: format!("{pc}\t{op}\t{detail}"),
                sql_id: static_sql(st).map(sql_id),
                dynamic: is_dynamic(st),
            }
        })
        .collect()
}

/// 该指令携带的静态 SQL（`explain` ↔ `trace` 的关联面）。
fn static_sql(st: &IrStmt) -> Option<&str> {
    match st {
        IrStmt::SqlQuery { sql, .. } => Some(sql),
        IrStmt::SqlExec { sql, .. } => Some(sql),
        IrStmt::OpenCursor {
            source: CursorSource::Static { sql, .. },
            ..
        } => Some(sql),
        IrStmt::SqlExecDynamic { .. }
        | IrStmt::OpenCursor {
            source: CursorSource::Dynamic { .. },
            ..
        }
        | IrStmt::Nop
        | IrStmt::Assign { .. }
        | IrStmt::Jump(_)
        | IrStmt::JumpIfNot { .. }
        | IrStmt::JumpIf { .. }
        | IrStmt::Raise { .. }
        | IrStmt::Return { .. }
        | IrStmt::TxCommit
        | IrStmt::TxRollback
        | IrStmt::Perform { .. }
        | IrStmt::Fetch { .. }
        | IrStmt::CloseCursor { .. }
        | IrStmt::PushTrap { .. }
        | IrStmt::PopTrap
        | IrStmt::CallRoutine { .. } => None,
    }
}

/// 语句文本只在运行期存在的指令（动态 SQL）。
fn is_dynamic(st: &IrStmt) -> bool {
    matches!(
        st,
        IrStmt::SqlExecDynamic { .. }
            | IrStmt::OpenCursor {
                source: CursorSource::Dynamic { .. },
                ..
            }
    )
}

fn describe(st: &IrStmt) -> (&'static str, String) {
    match st {
        IrStmt::Nop => ("nop", String::new()),
        IrStmt::SqlExecDynamic { .. } => ("execute_dynamic", String::new()),
        IrStmt::Assign { target, expr } => (
            "assign",
            format!("{} := {}", target_text(target), expr_text(expr)),
        ),
        IrStmt::Jump(to) => ("jump", to.to_string()),
        IrStmt::JumpIfNot { cond, to } => ("jump_if_not", format!("{} -> {to}", expr_text(cond))),
        IrStmt::JumpIf { cond, to } => ("jump_if", format!("{} -> {to}", expr_text(cond))),
        IrStmt::Raise { message } => (
            "raise",
            message
                .as_ref()
                .map(expr_text)
                .unwrap_or_else(|| "<current>".to_string()),
        ),
        IrStmt::Return { value } => (
            "return",
            value
                .as_ref()
                .map(expr_text)
                .unwrap_or_else(|| "null".to_string()),
        ),
        IrStmt::TxCommit => ("tx_commit", String::new()),
        IrStmt::TxRollback => ("tx_rollback", String::new()),
        IrStmt::SqlQuery { sql, into, .. } => (
            "sql_query",
            format!(
                "{} into [{}]",
                one_line(sql),
                into.iter().map(target_text).collect::<Vec<_>>().join(", ")
            ),
        ),
        IrStmt::SqlExec { sql, .. } => ("sql_exec", one_line(sql)),
        IrStmt::Perform { expr } => ("perform", expr_text(expr)),
        IrStmt::OpenCursor { cursor, source } => (
            "open_cursor",
            format!(
                "{} {}",
                cursor.0,
                match source {
                    CursorSource::Static { sql, .. } => format!("static[{}]", one_line(sql)),
                    CursorSource::Dynamic { .. } => "dynamic".to_string(),
                }
            ),
        ),
        IrStmt::Fetch {
            cursor,
            targets,
            record,
        } => (
            "fetch",
            format!(
                "{} -> [{}]{}",
                cursor.0,
                targets
                    .iter()
                    .map(target_text)
                    .collect::<Vec<_>>()
                    .join(", "),
                if *record { " as record" } else { "" }
            ),
        ),
        IrStmt::CloseCursor { cursor } => ("close_cursor", cursor.0.clone()),
        IrStmt::PushTrap { entries } => (
            "push_trap",
            entries
                .iter()
                .map(|e| format!("[{}]..{}", e.conds.join("|"), e.body_end))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        IrStmt::PopTrap => ("pop_trap", String::new()),
        IrStmt::CallRoutine {
            name,
            args,
            out_targets,
        } => (
            "call",
            format!(
                "{}({}) out[{}]",
                name,
                args.iter().map(expr_text).collect::<Vec<_>>().join(", "),
                out_targets
                    .iter()
                    .map(|t| t
                        .as_ref()
                        .map(target_text)
                        .unwrap_or_else(|| "-".to_string()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    }
}

fn target_text(t: &AssignTarget) -> String {
    match t {
        AssignTarget::Var(v) => v.0.clone(),
        AssignTarget::Field { rec, field } => format!("{}.{}", rec.0, field),
    }
}

fn expr_text(e: &IrExpr) -> String {
    match e {
        IrExpr::Const(v) => const_text(v),
        IrExpr::Var(v) => v.0.clone(),
        IrExpr::Field { rec, field } => format!("{}.{}", rec.0, field),
        IrExpr::Bin { op, l, r } => format!("({} {} {})", expr_text(l), op_text(*op), expr_text(r)),
        IrExpr::Not(x) => format!("(not {})", expr_text(x)),
        IrExpr::IsNull(x) => format!("({} is null)", expr_text(x)),
        IrExpr::FuncCall { name, args } => format!(
            "{}({})",
            name,
            args.iter().map(expr_text).collect::<Vec<_>>().join(", ")
        ),
        IrExpr::CursorAttr { cursor, kind } => format!("{}%{:?}", cursor.0, kind),
        IrExpr::SqlFn { name, args } => format!(
            "{}({}) [sql]",
            name,
            args.iter().map(expr_text).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn op_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Concat => "||",
        BinOp::Eq => "=",
        BinOp::Ne => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
    }
}

fn const_text(v: &SpinValue) -> String {
    match v {
        SpinValue::Null => "null".to_string(),
        SpinValue::Bool(b) => b.to_string(),
        SpinValue::Int(i) => i.to_string(),
        SpinValue::Decimal(_, _, _) => v.display(),
        SpinValue::Str(s) => format!("'{s}'"),
        SpinValue::Blob(_) => "<blob>".to_string(),
        SpinValue::Record(_) => "<record>".to_string(),
    }
}

/// 与 trace 的 `sql_id` 同一归一口径（都走 [`flat`]）——explain ↔ trace 按指纹对齐的前提。
fn one_line(sql: &str) -> String {
    flat(sql)
}

/// 编译例程（不执行）：`--file` 的源码文本 + 例程名 → IR 程序。
fn compile_routine(sql_text: &str, routine: &str) -> Result<IrProg, EngineError> {
    let mut eng = crate::engine::Engine::new(
        Box::new(crate::cli::NoopExecutor),
        crate::rules::RuleRegistry::default(),
    );
    eng.register(sql_text)?;
    let r = eng
        .catalog
        .lookup(routine)
        .ok_or_else(|| EngineError::business(150, format!("routine {routine} not registered")))?;
    Ok(r.prog.clone())
}

/// 供 CLI 使用：读取 SQL 文件、注册例程、渲染指定例程的 IR。
pub fn explain_routine(sql_text: &str, routine: &str) -> Result<Vec<String>, EngineError> {
    Ok(render(&compile_routine(sql_text, routine)?))
}

/// 供 CLI 使用（`--trace`）：IR 行 + 该次真实运行的命中注记 + 反向报警。
pub fn explain_traced(
    sql_text: &str,
    routine: &str,
    log: &str,
) -> Result<Vec<String>, EngineError> {
    let hits = histogram(log);
    Ok(align(&compile_routine(sql_text, routine)?, &hits))
}

/// 直方图贴到 IR 行上；`sqlid` 相同的多条指令共享同一命中数（静态文本口径）。
fn align(prog: &IrProg, hits: &BTreeMap<String, Hit>) -> Vec<String> {
    let mut ir_ids: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::with_capacity(prog.code.len() + 2);
    for l in lines(prog) {
        match (&l.sql_id, l.dynamic) {
            (Some(id), _) => {
                ir_ids.insert(id.clone());
                out.push(format!("{}\t{}", l.text, annotation(id, hits)));
            }
            (None, true) => out.push(format!("{}\t[trace 动态：文本在运行期]", l.text)),
            (None, false) => out.push(l.text),
        }
    }
    let ran = ir_ids.iter().filter(|id| hits.contains_key(*id)).count();
    out.push(format!(
        "[trace] 对齐 {ran}/{} 条静态 SQL · 日志 {} 个 sqlid",
        ir_ids.len(),
        hits.len()
    ));
    let orphans = unmatched(hits, &ir_ids);
    if !orphans.is_empty() {
        out.push(format!(
            "[trace] 未对齐 {} 条（动态 SQL / 攒批合并文本 / 被调例程自己的语句）:",
            orphans.len()
        ));
        for (id, h) in orphans {
            out.push(format!("[trace]   {id} ×{} :: {}", h.count, h.head));
        }
    }
    out
}
