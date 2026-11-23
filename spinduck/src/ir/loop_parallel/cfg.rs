//! 循环区域 CFG 底座：结构后继 + Trap 分派边（静态门禁共享）。
//!
//! 语义对齐 `runtime/interp.rs` 的分派规则：`PushTrap` 压栈、`PopTrap`
//! 弹栈；错误分派自栈顶向外逐帧匹配 `TrapEntry.conds`，命中即跳
//! `body_start`。静态分析无法判定 conds 命中，故对**任一活跃帧的全部
//! 条目**保守建边（may-analysis：多出的边只会让证明更保守，不会放行）。

use crate::ir::{IrProg, IrStmt};

/// 预计算的邻接表：`adj` = 结构后继 ∪ trap 分派目标（保守 may 边，
/// 供反重入证明使用）；`struct_adj` = 纯结构后继（Worker 实际执行
/// 流——沙箱无 trap 分派，处理器体永不属于 `G_pure` 候选）。
pub(crate) struct Cfg<'a> {
    adj: Vec<Vec<usize>>,
    struct_adj: Vec<Vec<usize>>,
    _prog: std::marker::PhantomData<&'a IrProg>,
}

impl<'a> Cfg<'a> {
    pub(crate) fn new(prog: &'a IrProg) -> Self {
        let mut struct_adj = Vec::with_capacity(prog.code.len());
        for pc in 0..prog.code.len() {
            struct_adj.push(struct_succs(prog, pc));
        }
        let mut adj = struct_adj.clone();
        for (pc, target) in trap_dispatch_edges(prog) {
            adj[pc].push(target);
        }
        for list in &mut adj {
            list.sort_unstable();
            list.dedup();
        }
        Self {
            adj,
            struct_adj,
            _prog: std::marker::PhantomData,
        }
    }

    /// `pc` 的全部静态后继（结构边 + trap 分派边）。
    pub(crate) fn succs(&self, pc: usize) -> &[usize] {
        self.adj.get(pc).map(Vec::as_slice).unwrap_or(&[])
    }

    /// `pc` 的纯结构后继（不含 trap 分派边）——`G_pure` 构建、
    /// 拓扑序与最长路径的口径：Worker 只能沿结构流执行。
    pub(crate) fn struct_succs(&self, pc: usize) -> &[usize] {
        self.struct_adj.get(pc).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// 结构后继（不含 trap 分派边）：跳转、条件跳、顺序落入、终止指令。
fn struct_succs(prog: &IrProg, pc: usize) -> Vec<usize> {
    let Some(stmt) = prog.code.get(pc) else {
        return Vec::new();
    };
    let n = prog.code.len();
    match stmt {
        IrStmt::Jump(to) => {
            if *to < n {
                vec![*to]
            } else {
                Vec::new()
            }
        }
        IrStmt::JumpIf { to, .. } | IrStmt::JumpIfNot { to, .. } => {
            let mut out: Vec<usize> = [pc + 1, *to].into_iter().filter(|t| *t < n).collect();
            out.sort_unstable();
            out.dedup();
            out
        }
        IrStmt::Raise { .. } | IrStmt::Return { .. } => Vec::new(),
        _ => {
            if pc + 1 < n {
                vec![pc + 1]
            } else {
                Vec::new()
            }
        }
    }
}

/// trap 分派边 `(触发 pc, 处理器 body_start)`：对每个 pc，凡栈中仍活跃的
/// `PushTrap` 帧，其全部 `TrapEntry` 都可能成为分派目标。
///
/// `PushTrap` 自身指令不受保护（压栈后才生效）；`PopTrap` 指令仍处于
/// 被保护区间内（多出边只更保守）。处理器体执行时其自身帧已被弹出，
/// 仅外层帧继续生效——与 `interp.rs` 的 `traps.pop()` 语义一致。
fn trap_dispatch_edges(prog: &IrProg) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    let mut stack: Vec<&[crate::ir::TrapEntry]> = Vec::new();
    for (pc, stmt) in prog.code.iter().enumerate() {
        for entries in &stack {
            for entry in entries.iter() {
                edges.push((pc, entry.body_start));
            }
        }
        match stmt {
            IrStmt::PushTrap { entries } => stack.push(entries),
            IrStmt::PopTrap => {
                stack.pop();
            }
            _ => {}
        }
    }
    edges.retain(|(_, to)| *to < prog.code.len());
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrProg, IrStmt, TrapEntry};

    fn prog_of(code: Vec<IrStmt>) -> IrProg {
        IrProg {
            code,
            var_types: Default::default(),
            local_ddl: Vec::new(),
        }
    }

    #[test]
    fn should_follow_structural_successors() {
        let prog = prog_of(vec![
            IrStmt::Nop,
            IrStmt::Jump(4),
            IrStmt::JumpIfNot {
                cond: crate::ir::IrExpr::Const(crate::value::SpinValue::Bool(true)),
                to: 0,
            },
            IrStmt::Nop,
            IrStmt::Return { value: None },
        ]);
        let cfg = Cfg::new(&prog);
        assert_eq!(cfg.succs(0), &[1]);
        assert_eq!(cfg.succs(1), &[4]);
        assert_eq!(cfg.succs(2), &[0, 3]); // 邻接表按目标 PC 升序规整
        assert!(cfg.succs(4).is_empty());
    }

    #[test]
    fn should_add_trap_dispatch_edges_for_active_frames() {
        // PushTrap@0 保护 [1..3)，PopTrap@3；处理器体在 [6,7)。
        let prog = prog_of(vec![
            IrStmt::PushTrap {
                entries: vec![TrapEntry {
                    conds: vec!["others".into()],
                    body_start: 6,
                    body_end: 7,
                }],
            },
            IrStmt::Nop,
            IrStmt::Nop,
            IrStmt::PopTrap,
            IrStmt::Nop,
            IrStmt::Jump(3),
            IrStmt::Nop,
        ]);
        let cfg = Cfg::new(&prog);
        // 受保护区间内的每条指令都可能分派到 body_start=6；PopTrap 之后不再有边。
        assert!(cfg.succs(1).contains(&6));
        assert!(cfg.succs(2).contains(&6));
        assert!(cfg.succs(3).contains(&6));
        assert!(!cfg.succs(4).contains(&6));
        assert!(!cfg.succs(0).contains(&6), "PushTrap 自身不受保护");
    }

    #[test]
    fn should_pop_nested_trap_frames_in_stack_order() {
        // 内层 PushTrap@2 的 PopTrap@4 只弹内层；外层保护持续到 PopTrap@6。
        let prog = prog_of(vec![
            IrStmt::PushTrap {
                entries: vec![TrapEntry {
                    conds: vec!["others".into()],
                    body_start: 8,
                    body_end: 9,
                }],
            },
            IrStmt::Nop,
            IrStmt::PushTrap {
                entries: vec![TrapEntry {
                    conds: vec!["others".into()],
                    body_start: 7,
                    body_end: 8,
                }],
            },
            IrStmt::Nop,
            IrStmt::PopTrap,
            IrStmt::Nop,
            IrStmt::PopTrap,
            IrStmt::Nop,
            IrStmt::Jump(6),
        ]);
        let cfg = Cfg::new(&prog);
        // pc=2 是内层 PushTrap：压栈前仅外层活跃 → 只有 →8。
        assert_eq!(cfg.succs(2), &[3, 8]);
        // pc=4 内层 PopTrap：两帧均活跃 → 结构后继 5 + 分派 7,8。
        assert_eq!(cfg.succs(4), &[5, 7, 8]);
        // pc=5 内层已弹、外层仍活跃 → 结构后继 6 + 分派 8。
        assert_eq!(cfg.succs(5), &[6, 8]);
        // PopTrap 指令本身仍处于被保护区间（多出边只更保守）。
        assert_eq!(cfg.succs(6), &[7, 8]);
        // 两帧均弹出后（pc=7 起）只剩结构边。
        assert_eq!(cfg.succs(7), &[8]);
        assert_eq!(cfg.succs(8), &[6]);
    }
}
