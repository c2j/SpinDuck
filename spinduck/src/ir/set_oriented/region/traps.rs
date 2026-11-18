//! Trap 体准入与兜底结构识别（§4.2 / §4.5 效果准入 / §4.4 B1 常量兜底 /
//! §4.3 IF 路由型兜底闭包）：处理器体限制、连续 SELECT INTO 组与 IF 路由组
//! 的结构面裁决；关系化等价证明由 lookup 子系统消费这些结构面。

use super::super::SetOrientedMode;
use super::{CursorRegion, SkipReason};
use crate::ir::{AssignTarget, IrExpr, IrProg, IrStmt, VarName};

/// 模式感知效果准入（§4.5）：BatchAtomic 允许循环尾 TxCommit，以及跳过型
/// trap 内紧随 SqlExec 的 TxCommit（守卫块末端 INSERT+COMMIT；折叠以写入
/// 全定义证明为前提，trap 永不触发）。Strict 全拒。
pub(crate) fn admit_effects(
    prog: &IrProg,
    region: &CursorRegion,
    mode: SetOrientedMode,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<(), SkipReason> {
    let back_pc = (region.fetch_pc + 1..=region.close_pc)
        .find(|pc| matches!(prog.code[*pc], IrStmt::Jump(t) if t == region.fetch_pc))
        .ok_or(SkipReason::UnsupportedControlFlow)?;
    let tail_commit_pc = back_pc - 1;
    let mut trap_depth = 0usize;
    for pc in region.open_pc..=region.close_pc {
        match &prog.code[pc] {
            IrStmt::PushTrap { .. } => trap_depth += 1,
            IrStmt::PopTrap => trap_depth = trap_depth.saturating_sub(1),
            IrStmt::TxCommit => {
                let guarded_tail =
                    trap_depth > 0 && matches!(prog.code.get(pc - 1), Some(IrStmt::SqlExec { .. }));
                if mode == SetOrientedMode::BatchAtomic && (pc == tail_commit_pc || guarded_tail) {
                    continue;
                }
                return Err(SkipReason::TransactionBoundaryInsideLoop);
            }
            IrStmt::TxRollback => return Err(SkipReason::TransactionBoundaryInsideLoop),
            IrStmt::Raise { .. } => return Err(SkipReason::ExceptionScopePresent),
            IrStmt::CallRoutine {
                name,
                args,
                out_targets,
            } => {
                if !in_handler_body(prog, region, pc)
                    || !super::super::catalog_summary::call_is_noop(
                        catalog,
                        prog,
                        name,
                        args,
                        out_targets,
                    )
                {
                    return Err(SkipReason::UnsupportedCall);
                }
            }
            IrStmt::SqlExecDynamic { .. } => return Err(SkipReason::DynamicSql),
            _ => {}
        }
    }
    Ok(())
}

/// Trap 处理器体限制（§4.2）：跳过型（Nop / NoOp 调用 + Jump 回循环头），
/// 或与连续 SqlQuery 组配对的兜底型（同序常量赋值 + 续跑 Jump，§4.4 B1；
/// 关系化等价证明由 lookup::fallback_group_at 裁决）。其余处理器拒绝。
pub(crate) fn trap_bodies_admissible(
    prog: &IrProg,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<(), SkipReason> {
    for pc in region.open_pc..=region.close_pc {
        let IrStmt::PushTrap { entries } = &prog.code[pc] else {
            continue;
        };
        for entry in entries {
            if fallback_handler(prog, pc, entry) {
                continue;
            }
            for bpc in entry.body_start..entry.body_end {
                match prog.code.get(bpc) {
                    Some(IrStmt::Nop) => {}
                    Some(IrStmt::Jump(t)) if *t == region.fetch_pc => {}
                    Some(IrStmt::CallRoutine {
                        name,
                        args,
                        out_targets,
                    }) if super::super::catalog_summary::call_is_noop(
                        catalog,
                        prog,
                        name,
                        args,
                        out_targets,
                    ) => {}
                    _ => return Err(SkipReason::ExceptionScopePresent),
                }
            }
        }
    }
    Ok(())
}

/// pc 是否落在区域内某个 trap 处理器体区间（半开）内。
pub(crate) fn in_handler_body(prog: &IrProg, region: &CursorRegion, pc: usize) -> bool {
    (region.open_pc..=region.close_pc).any(|p| match &prog.code[p] {
        IrStmt::PushTrap { entries } => entries
            .iter()
            .any(|e| pc >= e.body_start && pc < e.body_end),
        _ => false,
    })
}

/// 兜底处理器结构面：同一 WHEN OTHERS 内 n 条连续 SELECT INTO +
/// PopTrap + Jump，处理器体为 n 条同序常量赋值 + 同目标 Jump。
/// 返回 (变量, 兜底常量) 序列；形态不符 = None。
pub(crate) fn fallback_group(
    prog: &IrProg,
    pc: usize,
    entry: &crate::ir::TrapEntry,
) -> Option<Vec<(VarName, crate::value::SpinValue)>> {
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return None;
    }
    let mut queries = Vec::new();
    while let Some(IrStmt::SqlQuery { into, fn_sites, .. }) = prog.code.get(pc + 1 + queries.len())
    {
        let [AssignTarget::Var(v)] = into.as_slice() else {
            return None;
        };
        if !fn_sites.is_empty() {
            return None;
        }
        queries.push(v.clone());
    }
    let n = queries.len();
    if n == 0 {
        return None;
    }
    let last = pc + n;
    let body_start = last + 3;
    let body_end = body_start + n;
    if entry.body_start != body_start || entry.body_end != body_end {
        return None;
    }
    if !matches!(prog.code.get(last + 1), Some(IrStmt::PopTrap)) {
        return None;
    }
    let join = body_end + 1;
    if !matches!(prog.code.get(last + 2), Some(IrStmt::Jump(t)) if *t == join)
        || !matches!(prog.code.get(body_end), Some(IrStmt::Jump(t)) if *t == join)
    {
        return None;
    }
    let mut out = Vec::new();
    for (i, qv) in queries.iter().enumerate() {
        let IrStmt::Assign {
            target: AssignTarget::Var(av),
            expr: IrExpr::Const(default),
        } = prog.code.get(body_start + i)?
        else {
            return None;
        };
        if av != qv {
            return None;
        }
        out.push((qv.clone(), default.clone()));
    }
    Some(out)
}

/// 兜底处理器结构面判定（trap 闭包准入用）。
fn fallback_handler(prog: &IrProg, pc: usize, entry: &crate::ir::TrapEntry) -> bool {
    fallback_group(prog, pc, entry).is_some() || fallback_if_group(prog, pc, entry).is_some()
}

/// IF 路由型常量兜底闭包（§4.3 + §4.4 B1）的结构面：PushTrap 主线首条为
/// `JumpIfNot(cond, ELSE)`，THEN 末端 Jump 回 PopTrap 处，ELSE 为 n 条连续
/// SELECT INTO，处理器体为 m 条常量赋值（1 ≤ m ≤ n，重置成员须为 ELSE 查询
/// 变量）+ 续跑 Jump。m = n 为全量兜底；m < n 为部分兜底，未重置成员的组内
/// 局部死证明由 lookup 子系统裁决。返回 IF 条件、ELSE 起始 pc、查询变量序列、
/// (重置变量, 兜底常量) 序列与续跑 pc；形态不符 = None。THEN 内容不可达
/// （入口守卫证明），其效果准入由 admit_effects 全局裁决。
pub(crate) struct IfFallback {
    pub cond: IrExpr,
    pub else_start: usize,
    pub query_vars: Vec<VarName>,
    pub resets: Vec<(VarName, crate::value::SpinValue)>,
    pub resume_pc: usize,
}

pub(crate) fn fallback_if_group(
    prog: &IrProg,
    pc: usize,
    entry: &crate::ir::TrapEntry,
) -> Option<IfFallback> {
    if entry.conds.len() != 1 || !entry.conds[0].eq_ignore_ascii_case("others") {
        return None;
    }
    let IrStmt::JumpIfNot { cond, to } = prog.code.get(pc + 1)? else {
        return None;
    };
    let e = *to;
    if e <= pc + 2 {
        return None;
    }
    let mut vars = Vec::new();
    while let Some(IrStmt::SqlQuery { into, fn_sites, .. }) = prog.code.get(e + vars.len()) {
        let [AssignTarget::Var(v)] = into.as_slice() else {
            return None;
        };
        if !fn_sites.is_empty() {
            return None;
        }
        vars.push(v.clone());
    }
    let n = vars.len();
    if n == 0 {
        return None;
    }
    let pop = e + n;
    if !matches!(prog.code.get(e - 1), Some(IrStmt::Jump(t)) if *t == pop)
        || !matches!(prog.code.get(pop), Some(IrStmt::PopTrap))
    {
        return None;
    }
    let body_start = pop + 2;
    if entry.body_start != body_start || entry.body_end <= body_start {
        return None;
    }
    let m = entry.body_end - body_start;
    if m > n {
        return None;
    }
    let resume = entry.body_end + 1;
    if !matches!(prog.code.get(pop + 1), Some(IrStmt::Jump(t)) if *t == resume)
        || !matches!(prog.code.get(entry.body_end), Some(IrStmt::Jump(t)) if *t == resume)
    {
        return None;
    }
    let resets = fallback_resets(prog, body_start..body_start + m, &vars)?;
    Some(IfFallback {
        cond: cond.clone(),
        else_start: e,
        query_vars: vars,
        resets,
        resume_pc: resume,
    })
}

fn fallback_resets(
    prog: &IrProg,
    range: std::ops::Range<usize>,
    vars: &[VarName],
) -> Option<Vec<(VarName, crate::value::SpinValue)>> {
    let mut resets = Vec::new();
    for pc in range {
        let IrStmt::Assign {
            target: AssignTarget::Var(av),
            expr: IrExpr::Const(default),
        } = prog.code.get(pc)?
        else {
            return None;
        };
        if !vars.contains(av) || resets.iter().any(|(rv, _)| rv == av) {
            return None;
        }
        resets.push((av.clone(), default.clone()));
    }
    Some(resets)
}

#[cfg(test)]
mod tests {
    use super::super::loop_prog;
    use super::*;
    use crate::ir::{BufferPolicy, CursorSource};
    use crate::runtime::cursor::CursorAttrKind;

    #[test]
    fn should_reject_tail_commit_in_strict_mode() {
        let mut prog = loop_prog(0, 0);
        prog.code.insert(4, IrStmt::TxCommit);
        prog.code[2] = IrStmt::JumpIf {
            cond: IrExpr::CursorAttr {
                cursor: VarName("c".into()),
                kind: CursorAttrKind::NotFound,
            },
            to: 7,
        };
        prog.code[5] = IrStmt::Jump(1);
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            admit_effects(&prog, &region, SetOrientedMode::Strict, &Default::default()),
            Err(SkipReason::TransactionBoundaryInsideLoop)
        );
    }

    #[test]
    fn should_allow_tail_commit_in_batch_atomic_mode() {
        let mut prog = loop_prog(0, 0);
        prog.code.insert(4, IrStmt::TxCommit);
        prog.code[2] = IrStmt::JumpIf {
            cond: IrExpr::CursorAttr {
                cursor: VarName("c".into()),
                kind: CursorAttrKind::NotFound,
            },
            to: 7,
        };
        prog.code[5] = IrStmt::Jump(1);
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            admit_effects(
                &prog,
                &region,
                SetOrientedMode::BatchAtomic,
                &Default::default()
            ),
            Ok(())
        );
    }

    #[test]
    fn should_reject_mid_body_commit_even_in_batch_atomic() {
        let mut prog = loop_prog(0, 0);
        prog.code.insert(3, IrStmt::TxCommit);
        prog.code[2] = IrStmt::JumpIf {
            cond: IrExpr::CursorAttr {
                cursor: VarName("c".into()),
                kind: CursorAttrKind::NotFound,
            },
            to: 7,
        };
        prog.code[5] = IrStmt::Jump(1);
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            admit_effects(
                &prog,
                &region,
                SetOrientedMode::BatchAtomic,
                &Default::default()
            ),
            Err(SkipReason::TransactionBoundaryInsideLoop)
        );
    }

    #[test]
    fn should_reject_rollback_in_any_mode() {
        let mut prog = loop_prog(0, 0);
        prog.code.insert(4, IrStmt::TxRollback);
        prog.code[2] = IrStmt::JumpIf {
            cond: IrExpr::CursorAttr {
                cursor: VarName("c".into()),
                kind: CursorAttrKind::NotFound,
            },
            to: 7,
        };
        prog.code[5] = IrStmt::Jump(1);
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            admit_effects(
                &prog,
                &region,
                SetOrientedMode::BatchAtomic,
                &Default::default()
            ),
            Err(SkipReason::TransactionBoundaryInsideLoop)
        );
    }

    #[test]
    fn should_pass_trap_body_with_nop_and_jump_to_fetch() {
        // 使用 mod.rs 中的嵌套 Trap 测试 fixture（handler = Nop + Jump(1)）
        let cursor = VarName("c".into());
        let code = vec![
            IrStmt::OpenCursor {
                cursor: cursor.clone(),
                source: CursorSource::Static {
                    sql: "SELECT a FROM src".into(),
                    binds: Vec::new(),
                    fn_sites: Vec::new(),
                },
            },
            IrStmt::Fetch {
                cursor: cursor.clone(),
                targets: vec![AssignTarget::Var(VarName("rec".into()))],
                record: true,
            },
            IrStmt::JumpIf {
                cond: IrExpr::CursorAttr {
                    cursor: cursor.clone(),
                    kind: CursorAttrKind::NotFound,
                },
                to: 11,
            },
            IrStmt::PushTrap {
                entries: vec![crate::ir::TrapEntry {
                    conds: vec!["others".into()],
                    body_start: 7,
                    body_end: 9,
                }],
            },
            IrStmt::SqlExec {
                sql: "INSERT INTO dst VALUES ($v_rec__a)".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
                buffer_policy: BufferPolicy::Immediate,
            },
            IrStmt::PopTrap,
            IrStmt::Jump(9),
            IrStmt::Nop,
            IrStmt::Jump(1),
            IrStmt::Jump(1),
            IrStmt::CloseCursor {
                cursor: cursor.clone(),
            },
            IrStmt::Nop,
        ];
        let prog = IrProg {
            code,
            var_types: Default::default(),
            local_ddl: Vec::new(),
        };
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            trap_bodies_admissible(&prog, &region, &Default::default()),
            Ok(())
        );
    }

    #[test]
    fn should_reject_trap_body_with_assign() {
        let cursor = VarName("c".into());
        let code = vec![
            IrStmt::OpenCursor {
                cursor: cursor.clone(),
                source: CursorSource::Static {
                    sql: "SELECT a FROM src".into(),
                    binds: Vec::new(),
                    fn_sites: Vec::new(),
                },
            },
            IrStmt::Fetch {
                cursor: cursor.clone(),
                targets: vec![AssignTarget::Var(VarName("rec".into()))],
                record: true,
            },
            IrStmt::JumpIf {
                cond: IrExpr::CursorAttr {
                    cursor: cursor.clone(),
                    kind: CursorAttrKind::NotFound,
                },
                to: 9,
            },
            IrStmt::PushTrap {
                entries: vec![crate::ir::TrapEntry {
                    conds: vec!["others".into()],
                    body_start: 6,
                    body_end: 7,
                }],
            },
            IrStmt::SqlExec {
                sql: "INSERT INTO dst VALUES ($v_rec__a)".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
                buffer_policy: BufferPolicy::Immediate,
            },
            IrStmt::PopTrap,
            IrStmt::Assign {
                target: AssignTarget::Var(VarName("v".into())),
                expr: IrExpr::Const(crate::value::SpinValue::Int(0)),
            },
            IrStmt::Jump(1),
            IrStmt::CloseCursor {
                cursor: cursor.clone(),
            },
            IrStmt::Nop,
        ];
        let prog = IrProg {
            code,
            var_types: Default::default(),
            local_ddl: Vec::new(),
        };
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            trap_bodies_admissible(&prog, &region, &Default::default()),
            Err(SkipReason::ExceptionScopePresent)
        );
    }
}
