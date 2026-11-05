//! `explain`：把例程编译成 IR 并以稳定文本形式打印（调试/回归用，非执行）。
//!
//! 输出契约：每行一条指令 `<pc>\t<op>\t<detail>`，`detail` 只含稳定信息
//! （变量名/标签/常量），不含地址或指针，便于 diff 与黄金比对。

use crate::error::EngineError;
use crate::ir::{AssignTarget, BinOp, CursorSource, IrExpr, IrProg, IrStmt};
use crate::value::SpinValue;

/// 渲染程序为逐行文本。
pub fn render(prog: &IrProg) -> Vec<String> {
    let mut out = Vec::with_capacity(prog.code.len());
    for (pc, st) in prog.code.iter().enumerate() {
        let (op, detail) = describe(st);
        out.push(format!("{pc}\t{op}\t{detail}"));
    }
    out
}

fn describe(st: &IrStmt) -> (&'static str, String) {
    match st {
        IrStmt::Nop => ("nop", String::new()),
        IrStmt::Assign { target, expr } => (
            "assign",
            format!("{} := {}", target_text(target), expr_text(expr)),
        ),
        IrStmt::Jump(to) => ("jump", to.to_string()),
        IrStmt::JumpIfNot { cond, to } => ("jump_if_not", format!("{} -> {to}", expr_text(cond))),
        IrStmt::JumpIf { cond, to } => ("jump_if", format!("{} -> {to}", expr_text(cond))),
        IrStmt::Raise { message } => ("raise", expr_text(message)),
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

fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 供 CLI 使用：读取 SQL 文件、注册例程、渲染指定例程的 IR。
pub fn explain_routine(sql_text: &str, routine: &str) -> Result<Vec<String>, EngineError> {
    let mut eng = crate::engine::Engine::new(
        Box::new(crate::cli::NoopExecutor),
        crate::rules::RuleRegistry::default(),
    );
    eng.register(sql_text)?;
    let r = eng
        .catalog
        .lookup(routine)
        .ok_or_else(|| EngineError::business(150, format!("routine {routine} not registered")))?;
    Ok(render(&r.prog))
}
