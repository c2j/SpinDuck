//! 含 Trap 闭包的 SESE 游标区域抽取（V2 §4.2；原始 IR 只读）。

mod proof;
mod traps;
pub(crate) use proof::*;
pub(crate) use traps::*;

use super::SkipReason;
use crate::ir::{AssignTarget, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use crate::runtime::cursor::CursorAttrKind;

/// 已识别的游标主干区域（PC 均为原始 `IrProg.code` 下标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorRegion {
    pub open_pc: usize,
    pub fetch_pc: usize,
    /// `JumpIf %NOTFOUND` 的目标，即 `CloseCursor` 所在 PC。
    pub close_pc: usize,
    pub cursor: VarName,
    pub record: VarName,
}

/// 从整段 IR 中抽取唯一静态游标循环区域；前缀/后缀允许存在但不在本步证明。
pub(crate) fn extract(prog: &IrProg) -> Result<CursorRegion, SkipReason> {
    let mut found: Option<CursorRegion> = None;
    for (open_pc, stmt) in prog.code.iter().enumerate() {
        let IrStmt::OpenCursor {
            cursor,
            source: CursorSource::Static { .. },
        } = stmt
        else {
            continue;
        };
        if found.is_some() {
            return Err(SkipReason::UnsupportedControlFlow);
        }
        let region = recognize(prog, open_pc, cursor)?;
        reject_external_entry_edges(prog, &region)?;
        reject_internal_exit_edges(prog, &region)?;
        found = Some(region);
    }
    found.ok_or(SkipReason::UnsupportedControlFlow)
}

/// 识别 `OPEN/FETCH/JumpIf %NOTFOUND/…/Jump(fetch)/CLOSE` 主干。
fn recognize(prog: &IrProg, open_pc: usize, cursor: &VarName) -> Result<CursorRegion, SkipReason> {
    let fetch_pc = open_pc + 1;
    let jump_pc = open_pc + 2;
    let Some(IrStmt::Fetch {
        cursor: fetched,
        targets,
        record: true,
    }) = prog.code.get(fetch_pc)
    else {
        return Err(SkipReason::UnsupportedControlFlow);
    };
    let [AssignTarget::Var(record)] = targets.as_slice() else {
        return Err(SkipReason::LiveOutState);
    };
    if fetched != cursor {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    let Some(IrStmt::JumpIf {
        cond:
            IrExpr::CursorAttr {
                cursor: tested,
                kind: CursorAttrKind::NotFound,
            },
        to,
    }) = prog.code.get(jump_pc)
    else {
        return Err(SkipReason::UnsupportedControlFlow);
    };
    if tested != cursor {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    let back_pc = (jump_pc + 1..prog.code.len())
        .find(|pc| {
            matches!(prog.code[*pc], IrStmt::Jump(target) if target == fetch_pc)
                && matches!(prog.code.get(pc + 1), Some(IrStmt::CloseCursor { cursor: c }) if c == cursor)
        })
        .ok_or(SkipReason::UnsupportedControlFlow)?;
    let close_pc = back_pc + 1;
    // 编译器两种出口布局：有后缀时 `end` 标签标在 CLOSE 之后（%NOTFOUND
    // 跳 close_pc+1）；循环终结例程（CLOSE 为末指令）时标在 CLOSE 本身
    // （%NOTFOUND 跳 close_pc）。两者皆以 close_pc+1 为区域出口续跑点。
    if (*to != close_pc + 1 && *to != close_pc)
        || !matches!(prog.code.get(close_pc), Some(IrStmt::CloseCursor { cursor: closed }) if closed == cursor)
    {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    Ok(CursorRegion {
        open_pc,
        fetch_pc,
        close_pc,
        cursor: cursor.clone(),
        record: record.clone(),
    })
}

/// 区域内（含处理器）不得存在跳出区域的跳转；仅允许回循环头与 `%NOTFOUND` 正常出口。
fn reject_internal_exit_edges(prog: &IrProg, region: &CursorRegion) -> Result<(), SkipReason> {
    let exit_pc = region.close_pc + 1;
    let outside = |pc: usize| pc < region.open_pc || pc > region.close_pc;
    for stmt in &prog.code[region.open_pc..=region.close_pc] {
        match stmt {
            IrStmt::JumpIf { to, .. } if *to == exit_pc => {}
            IrStmt::Jump(to) | IrStmt::JumpIf { to, .. } | IrStmt::JumpIfNot { to, .. } => {
                if outside(*to) {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
            }
            IrStmt::PushTrap { entries }
                if entries
                    .iter()
                    .any(|e| outside(e.body_start) || outside(e.body_end)) =>
            {
                return Err(SkipReason::UnsupportedControlFlow);
            }
            _ => {}
        }
    }
    Ok(())
}

/// 区域外任何跳转/处理器不得跳入除 `open_pc` 以外的区域内部 PC。
fn reject_external_entry_edges(prog: &IrProg, region: &CursorRegion) -> Result<(), SkipReason> {
    let inside = |pc: usize| pc > region.open_pc && pc <= region.close_pc;
    for (pc, stmt) in prog.code.iter().enumerate() {
        if pc >= region.open_pc && pc <= region.close_pc {
            continue;
        }
        match stmt {
            IrStmt::Jump(to) | IrStmt::JumpIf { to, .. } | IrStmt::JumpIfNot { to, .. } => {
                if inside(*to) {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
            }
            IrStmt::PushTrap { entries }
                if entries
                    .iter()
                    .any(|e| inside(e.body_start) || inside(e.body_end)) =>
            {
                return Err(SkipReason::UnsupportedControlFlow);
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn loop_prog(prefix: usize, suffix: usize) -> IrProg {
    let open = prefix;
    let fetch = open + 1;
    let close = open + 5;
    let mut code = vec![IrStmt::Nop; prefix];
    code.push(IrStmt::OpenCursor {
        cursor: VarName("c".into()),
        source: CursorSource::Static {
            sql: "SELECT a FROM src".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
        },
    });
    code.push(IrStmt::Fetch {
        cursor: VarName("c".into()),
        targets: vec![AssignTarget::Var(VarName("rec".into()))],
        record: true,
    });
    code.push(IrStmt::JumpIf {
        cond: IrExpr::CursorAttr {
            cursor: VarName("c".into()),
            kind: CursorAttrKind::NotFound,
        },
        to: close + 1,
    });
    code.push(IrStmt::SqlExec {
        sql: "INSERT INTO dst VALUES ($v_rec__a)".into(),
        binds: vec![(
            "$v_rec__a".into(),
            crate::ir::BindSrc::Field {
                rec: VarName("rec".into()),
                field: "a".into(),
            },
        )],
        fn_sites: Vec::new(),
    });
    code.push(IrStmt::Jump(fetch));
    code.push(IrStmt::CloseCursor {
        cursor: VarName("c".into()),
    });
    code.extend(std::iter::repeat_n(IrStmt::Nop, suffix));
    IrProg {
        code,
        var_types: Default::default(),
        local_ddl: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_extract_cursor_loop_with_prefix_and_suffix() {
        let prog = loop_prog(1, 1);
        let region = extract(&prog).expect("should extract cursor loop");
        assert_eq!(region.open_pc, 1);
        assert_eq!(region.fetch_pc, 2);
        assert_eq!(region.close_pc, 6);
        assert_eq!(region.cursor, VarName("c".into()));
        assert_eq!(region.record, VarName("rec".into()));
    }

    #[test]
    fn should_reject_jump_into_region_body() {
        let mut prog = loop_prog(1, 0);
        prog.code[0] = IrStmt::Jump(4);
        assert_eq!(extract(&prog), Err(SkipReason::UnsupportedControlFlow));
    }

    #[test]
    fn should_reject_jump_out_of_region() {
        let mut prog = loop_prog(0, 1);
        prog.code[3] = IrStmt::Jump(6);
        assert_eq!(extract(&prog), Err(SkipReason::UnsupportedControlFlow));
    }

    #[test]
    fn should_reject_trap_handler_body_outside_region() {
        let mut prog = loop_prog(0, 1);
        prog.code[3] = IrStmt::PushTrap {
            entries: vec![crate::ir::TrapEntry {
                conds: vec!["others".into()],
                body_start: 4,
                body_end: 7,
            }],
        };
        assert_eq!(extract(&prog), Err(SkipReason::UnsupportedControlFlow));
    }

    #[test]
    fn should_extract_region_when_notfound_jumps_to_close_cursor_itself() {
        // 编译器在循环终结例程（CLOSE 后无后缀）时把 %NOTFOUND 标到
        // CloseCursor 所在 PC（而非 close_pc+1）；两种出口布局均须识别。
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
                to: 5,
            },
            IrStmt::SqlExec {
                sql: "INSERT INTO dst VALUES ($v_rec__a)".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
            IrStmt::Jump(1),
            IrStmt::CloseCursor {
                cursor: cursor.clone(),
            },
        ];
        let prog = IrProg {
            code,
            var_types: Default::default(),
            local_ddl: Vec::new(),
        };
        let region = extract(&prog).expect("close-cursor exit layout should be accepted");
        assert_eq!(region.open_pc, 0);
        assert_eq!(region.fetch_pc, 1);
        assert_eq!(region.close_pc, 5);
    }

    #[test]
    fn should_extract_region_with_nested_trap_handler_and_resume() {
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
                    body_end: 8,
                }],
            },
            IrStmt::SqlExec {
                sql: "INSERT INTO dst VALUES ($v_rec__a)".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            },
            IrStmt::PopTrap,
            IrStmt::Jump(9),
            IrStmt::Nop,
            IrStmt::Jump(9),
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
        let region = extract(&prog).expect("trap closure should be accepted");
        assert_eq!(region.open_pc, 0);
        assert_eq!(region.close_pc, 10);
    }
}
