//! 解释器主循环：树遍历（显式步进计数），M2 无 SQL（SqlExecutor 尚未接入）。

use crate::error::EngineError;
use crate::exec::{TraceEvent, TraceSink};
use crate::ir::{AssignTarget, IrProg, IrStmt};
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

/// 运行 IR 程序（无 SQL；M3 起接入 SqlExecutor 下发改写后的 SQL）。
pub fn run(prog: &IrProg, frame: &mut Frame) -> Result<RunOutcome, EngineError> {
    run_with_trace(prog, frame, &mut TraceNoop)
}

/// 带 trace 的运行（§2.1/§10.5：core 一次埋点，CLI/监控共用）。
pub fn run_with_trace(
    prog: &IrProg,
    frame: &mut Frame,
    sink: &mut dyn TraceSink,
) -> Result<RunOutcome, EngineError> {
    let mut pc = 0usize;
    let mut steps = 0u64;
    loop {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(EngineError::invariant(
                32,
                format!("step budget exceeded ({MAX_STEPS}): possible infinite loop"),
            ));
        }
        let st = match prog.code.get(pc) {
            Some(st) => st,
            // 程序尾部：隐式完成（无 RETURN 的过程）
            None => return Ok(RunOutcome::Completed),
        };
        sink.on_event(spinduck_trace_event(st));
        match st {
            IrStmt::Nop | IrStmt::TxCommit | IrStmt::TxRollback => pc += 1,
            IrStmt::Assign { target, expr } => {
                let v = eval(expr, frame)?;
                assign(frame, target, v)?;
                pc += 1;
            }
            IrStmt::Jump(to) => pc = *to,
            IrStmt::JumpIfNot { cond, to } => {
                let t = eval_cond(cond, frame)?;
                pc = if t == Some(true) { pc + 1 } else { *to };
            }
            IrStmt::JumpIf { cond, to } => {
                let t = eval_cond(cond, frame)?;
                pc = if t == Some(true) { *to } else { pc + 1 };
            }
            IrStmt::Raise { message } => {
                let v = eval(message, frame)?;
                return Err(EngineError::business(200, v.as_str_loose()));
            }
            IrStmt::Return { value } => {
                let v = match value {
                    Some(e) => Some(eval(e, frame)?),
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

fn spinduck_trace_event(st: &IrStmt) -> TraceEvent {
    TraceEvent::IrStmt {
        variant: variant_name(st),
    }
}

fn variant_name(st: &IrStmt) -> &'static str {
    match st {
        IrStmt::Nop => "Nop",
        IrStmt::Assign { .. } => "Assign",
        IrStmt::Jump(_) => "Jump",
        IrStmt::JumpIfNot { .. } => "JumpIfNot",
        IrStmt::JumpIf { .. } => "JumpIf",
        IrStmt::Raise { .. } => "Raise",
        IrStmt::Return { .. } => "Return",
        IrStmt::TxCommit => "TxCommit",
        IrStmt::TxRollback => "TxRollback",
    }
}

struct TraceNoop;
impl TraceSink for TraceNoop {
    fn on_event(&mut self, _event: TraceEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::compile_stmts;
    use crate::value::SpinValue;
    use ogsql_parser::ast::plpgsql::PlStatement;

    fn run_pl(stmts: Vec<PlStatement>) -> (Frame, Result<RunOutcome, EngineError>) {
        let prog = compile_stmts(&stmts).expect("compile");
        let mut f = Frame::new();
        let out = run(&prog, &mut f);
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
    fn should_reject_sql_statements_at_m2_scope() {
        // M2 范围外：SQL 类语句**编译期**显式拒绝（M3 纳入）
        let err = compile_stmts(&[PlStatement::Sql("SELECT 1".into())]).unwrap_err();
        assert_eq!(err.domain, crate::error::FaultDomain::Engine);
        assert!(err.message.contains("not yet supported"));
    }
}
