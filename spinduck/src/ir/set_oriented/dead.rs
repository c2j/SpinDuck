//! 死存储消除：全程序无读取变量的常量赋值可整体消除（语料 v_step_no
//! 形态）；OUT/INOUT 形参写回的可观察性由 plan() 出口活跃检查兜底。

use super::{BindSrc, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use crate::value::SpinValue;
use std::collections::BTreeSet;

/// 常量赋值可消除判据：变量全程序无读取，且槽位强转无损——
/// 非 Varchar 槽 coerce_for_slot 原样保留；Varchar 槽仅 Str/Null 常量可证。
pub(super) fn eliminable(prog: &IrProg, v: &VarName, c: &SpinValue) -> bool {
    let slot_ok = prog.var_types.get(&v.0) != Some(&crate::types::SpinType::Varchar)
        || matches!(c, SpinValue::Str(_) | SpinValue::Null);
    slot_ok && !read_set(prog).contains(v)
}

/// 全程序读集（穷尽扫描：表达式、绑定、游标属性、调用实参与写回目标）。
/// out_targets 保守计读：IN OUT 安装时读取调用方原值。
fn read_set(prog: &IrProg) -> BTreeSet<VarName> {
    let mut out = BTreeSet::new();
    for stmt in &prog.code {
        stmt_reads(stmt, &mut out);
    }
    out
}

/// 变量是否仅在允许 pc 集合内被读取（其余指令一律无读取）——部分兜底
/// 中间变量的组内局部死证明：允许 pc 为同组 CASE 写汇（操作数读取），
/// INSERT 绑定/出口后缀/路由条件/他组查询等任何其它读取均判非死。
pub(super) fn read_only_within(prog: &IrProg, v: &VarName, allowed_pcs: &[usize]) -> bool {
    for (pc, stmt) in prog.code.iter().enumerate() {
        if allowed_pcs.contains(&pc) {
            continue;
        }
        let mut reads = BTreeSet::new();
        stmt_reads(stmt, &mut reads);
        if reads.contains(v) {
            return false;
        }
    }
    true
}

fn stmt_reads(stmt: &IrStmt, out: &mut BTreeSet<VarName>) {
    match stmt {
        IrStmt::Assign { expr, .. }
        | IrStmt::Perform { expr }
        | IrStmt::FoldedQuery { expr, .. } => expr_reads(expr, out),
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => expr_reads(cond, out),
        IrStmt::Raise { message } | IrStmt::Return { value: message } => {
            if let Some(e) = message {
                expr_reads(e, out);
            }
        }
        IrStmt::SqlExec { binds, .. } | IrStmt::SqlQuery { binds, .. } => bind_reads(binds, out),
        IrStmt::SqlExecDynamic { text_expr, .. } => expr_reads(text_expr, out),
        IrStmt::OpenCursor { source, .. } => match source {
            CursorSource::Static { binds, .. } => bind_reads(binds, out),
            CursorSource::Dynamic { query_expr } => expr_reads(query_expr, out),
        },
        IrStmt::CallRoutine {
            args, out_targets, ..
        } => {
            for a in args {
                expr_reads(a, out);
            }
            for t in out_targets.iter().flatten() {
                if let super::AssignTarget::Var(v) = t {
                    out.insert(v.clone());
                }
            }
        }
        IrStmt::Nop
        | IrStmt::Jump(_)
        | IrStmt::TxCommit
        | IrStmt::TxCommitIfWritten
        | IrStmt::TxRollback
        | IrStmt::Fetch { .. }
        | IrStmt::CloseCursor { .. }
        | IrStmt::PushTrap { .. }
        | IrStmt::PopTrap => {}
    }
}

fn bind_reads(binds: &[(String, BindSrc)], out: &mut BTreeSet<VarName>) {
    for (_, src) in binds {
        match src {
            BindSrc::Var(v) => {
                out.insert(v.clone());
            }
            BindSrc::Field { rec, .. } => {
                out.insert(rec.clone());
            }
            // Expr 绑定不含帧变量读（其基字段属游标记录，非 IR 变量）。
            BindSrc::Expr { .. } => {}
        }
    }
}

fn expr_reads(expr: &IrExpr, out: &mut BTreeSet<VarName>) {
    match expr {
        IrExpr::Const(_) => {}
        IrExpr::Var(v) => {
            out.insert(v.clone());
        }
        IrExpr::Field { rec, .. } => {
            out.insert(rec.clone());
        }
        IrExpr::Bin { l, r, .. } => {
            expr_reads(l, out);
            expr_reads(r, out);
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_reads(e, out),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(e) = operand {
                expr_reads(e, out);
            }
            for (w, v) in whens {
                expr_reads(w, out);
                expr_reads(v, out);
            }
            if let Some(e) = else_expr {
                expr_reads(e, out);
            }
        }
        IrExpr::FuncCall { args, .. } => {
            for a in args {
                expr_reads(a, out);
            }
        }
        IrExpr::SqlFn { args, prepared, .. } => {
            for a in args {
                expr_reads(a, out);
            }
            if let Ok(scalar) = prepared {
                bind_reads(&scalar.binds, out);
            }
        }
        IrExpr::CursorAttr { cursor, .. } => {
            out.insert(cursor.clone());
        }
        IrExpr::Lookup { args, default, .. } => {
            for (_, a) in args {
                expr_reads(a, out);
            }
            expr_reads(default, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{AssignTarget, ScalarSql};
    use std::collections::BTreeMap;

    fn prog_with(stmts: Vec<IrStmt>) -> IrProg {
        IrProg {
            code: stmts,
            var_types: BTreeMap::new(),
            local_ddl: Vec::new(),
        }
    }

    #[test]
    fn should_reject_dead_store_for_varchar_slot_with_non_str_const() {
        let mut prog = prog_with(vec![IrStmt::Assign {
            target: AssignTarget::Var(VarName("v".into())),
            expr: IrExpr::Const(SpinValue::Int(7)),
        }]);
        let v = VarName("v".into());
        // 未登记槽（%TYPE 等）coerce 原样保留，Int 常量可消除
        assert!(eliminable(&prog, &v, &SpinValue::Int(7)));
        prog.var_types
            .insert("v".into(), crate::types::SpinType::Varchar);
        assert!(!eliminable(&prog, &v, &SpinValue::Int(7)));
        assert!(eliminable(&prog, &v, &SpinValue::Str("7".into())));
        assert!(eliminable(&prog, &v, &SpinValue::Null));
    }

    #[test]
    fn should_count_sqlfn_binds_and_call_writeback_as_reads() {
        let v = VarName("v".into());
        let via_sqlfn = prog_with(vec![IrStmt::Perform {
            expr: IrExpr::SqlFn {
                name: "to_char".into(),
                args: vec![],
                prepared: Ok(ScalarSql {
                    sql: "SELECT to_char($v_v)".into(),
                    binds: vec![("$v_v".into(), BindSrc::Var(v.clone()))],
                }),
            },
        }]);
        assert!(!eliminable(&via_sqlfn, &v, &SpinValue::Null));
        let via_writeback = prog_with(vec![IrStmt::CallRoutine {
            name: "p".into(),
            args: vec![],
            out_targets: vec![Some(AssignTarget::Var(v.clone()))],
        }]);
        assert!(!eliminable(&via_writeback, &v, &SpinValue::Null));
        let unread = prog_with(vec![IrStmt::Assign {
            target: AssignTarget::Var(v.clone()),
            expr: IrExpr::Const(SpinValue::Int(1)),
        }]);
        assert!(eliminable(&unread, &v, &SpinValue::Int(1)));
    }
}
