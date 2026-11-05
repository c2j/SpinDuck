//! 编译器状态与标签/跳转机制（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use crate::rules::RuleRegistry;
use crate::runtime::cursor::CursorAttrKind;
use ogsql_parser::ast::plpgsql::PlStatement;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct Compiler {
    pub(crate) vars: crate::sql::VarScope,
    /// 规则注册表（`[functions]` 改名表 + `[patterns]` 开关）：SQL 改写唯一来源
    pub(crate) rules: RuleRegistry,
    /// 静态游标声明 → 编译后的 (sql, binds)（compile_block_with 填充，§7.2）
    pub(crate) pending_cursor_sql: BTreeMap<String, CompiledCursorSql>,
    /// `%ROWTYPE` 变量 → 锚定游标名（§7.3.2：FETCH INTO 整行记录判定）
    pub(crate) rowtype_of: BTreeMap<String, String>,
    /// 已注册例程名（SQL 上下文调用下沉判定；空集 = 不下沉）
    pub(crate) routine_names: BTreeSet<String>,
    pub(crate) code: Vec<IrStmt>,
    pub(crate) labels: std::collections::HashMap<String, usize>,
    pub(crate) fixups: Vec<(usize, String)>,
    pub(crate) loop_stack: Vec<(String, String)>, // (start_label, end_label)
    pub(crate) depth: usize,
    pub(crate) synth: usize,
}

/// 嵌套深度上限。取 32 而非 64：解析器与编译器递归帧较大，64 层在默认测试栈
/// （2 MiB）下会栈溢出（abort），护栏必须先于溢出生效（tests/compiler_contracts.rs
/// 锁定 31 通过 / 32 拒绝）。
const MAX_DEPTH: usize = 32;

impl Compiler {
    pub(crate) fn here(&self) -> usize {
        self.code.len()
    }

    pub(crate) fn mark(&mut self, label: &str) {
        self.labels.insert(label.to_string(), self.here());
    }

    pub(crate) fn jump(&mut self, label: &str, cond: Option<IrExpr>, if_cond: bool) {
        let at = self.here();
        let known = self.labels.get(label).copied();
        match (known, cond) {
            (Some(to), None) => self.code.push(IrStmt::Jump(to)),
            (Some(to), Some(c)) if if_cond => self.code.push(IrStmt::JumpIf { cond: c, to }),
            (Some(to), Some(c)) => self.code.push(IrStmt::JumpIfNot { cond: c, to }),
            (None, None) => {
                self.code.push(IrStmt::Jump(usize::MAX));
                self.fixups.push((at, label.to_string()));
            }
            (None, Some(c)) => {
                self.code.push(if if_cond {
                    IrStmt::JumpIf {
                        cond: c,
                        to: usize::MAX,
                    }
                } else {
                    IrStmt::JumpIfNot {
                        cond: c,
                        to: usize::MAX,
                    }
                });
                self.fixups.push((at, label.to_string()));
            }
        }
    }

    pub(crate) fn resolve_fixups(&mut self) -> Result<(), EngineError> {
        let fixups = std::mem::take(&mut self.fixups);
        for (at, label) in fixups {
            let resolved = *self.labels.get(&label).ok_or_else(|| {
                EngineError::invariant(20, format!("unknown goto/exit label: {label}"))
            })?;
            match &mut self.code[at] {
                IrStmt::Jump(target) => *target = resolved,
                IrStmt::JumpIf { to: target, .. } | IrStmt::JumpIfNot { to: target, .. } => {
                    *target = resolved
                }
                _ => return Err(EngineError::invariant(28, "fixup target must be a jump")),
            }
        }
        Ok(())
    }

    /// FOR 隐式游标循环体：FETCH → %NOTFOUND 退出 → 循环体 → 回跳。
    pub(crate) fn for_cursor_loop(
        &mut self,
        cursor: &VarName,
        rec: &VarName,
        body: &[PlStatement],
        start: &str,
        end: &str,
    ) -> Result<(), EngineError> {
        self.mark(start);
        self.code.push(IrStmt::Fetch {
            cursor: cursor.clone(),
            targets: vec![AssignTarget::Var(rec.clone())],
            record: true,
        });
        let not_found = IrExpr::CursorAttr {
            cursor: cursor.clone(),
            kind: CursorAttrKind::NotFound,
        };
        self.jump(end, Some(not_found), true);
        self.stmts(body)?;
        self.jump(start, None, true);
        Ok(())
    }

    pub(crate) fn synth_label(&mut self, hint: &str) -> String {
        self.synth += 1;
        format!("__{hint}_{}", self.synth)
    }

    pub(crate) fn stmts(&mut self, stmts: &[PlStatement]) -> Result<(), EngineError> {
        for st in stmts {
            self.stmt(st)?;
        }
        Ok(())
    }

    pub(crate) fn stmt(&mut self, st: &PlStatement) -> Result<(), EngineError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(EngineError::invariant(21, "PL nesting too deep"));
        }
        let r = self.stmt_inner(st);
        self.depth -= 1;
        r
    }
}
