//! 编译器状态与标签/跳转机制（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use crate::rules::RuleRegistry;
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;
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
    /// 例程体内局部 DDL 标记（`code` 下标 → CTAS 与否），移交 IrProg
    pub(crate) local_ddl: Vec<LocalDdlStmt>,
    pub(crate) labels: std::collections::HashMap<String, usize>,
    pub(crate) fixups: Vec<(usize, String)>,
    pub(crate) loop_stack: Vec<(String, String)>, // (start_label, end_label)
    pub(crate) depth: usize,
    pub(crate) synth: usize,
    /// 变量名 → 推断 SpinType（`%TYPE`/显式类型声明；移交 IrProg）
    pub(crate) var_types: BTreeMap<String, crate::types::SpinType>,
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

    /// DECLARE 段编译（例程顶层块与嵌套 `BEGIN..END` 块共用）：
    /// 变量绑定入变量集并生成初始化指令（GaussDB 语义：声明即变量，未给
    /// 初值时为 NULL）；游标编译为静态 SQL 待 OPEN 使用。
    /// 嵌套块声明与外层同名时按词法作用域遮蔽：内层变量获得独立规范名，
    /// 块内引用解析到内层槽位，返回的 [`VarRestore`] 列表须在块编译结束
    /// 后逆序 [`VarScope::restore`]（例程顶层调用方忽略返回值）。
    pub(crate) fn declare_decls(
        &mut self,
        decls: &[ogsql_parser::ast::plpgsql::PlDeclaration],
    ) -> Result<Vec<crate::sql::VarRestore>, EngineError> {
        use ogsql_parser::ast::plpgsql::PlDeclaration;
        let mut restores = Vec::new();
        for decl in decls {
            match decl {
                PlDeclaration::Cursor(cd) => {
                    let name = cd.name.to_ascii_lowercase();
                    let query_text = match &cd.parsed_query {
                        Some(q) => ogsql_parser::formatter::SqlFormatter::new().format_statement(q),
                        None => cd.query.clone(),
                    };
                    let unit =
                        rewrite_sql(&query_text, &self.vars, &self.rules, &self.routine_names)?;
                    self.pending_cursor_sql
                        .insert(name, (unit.text, unit.binds, unit.fn_sites));
                }
                PlDeclaration::Variable(v) => {
                    let (canon, restore) = self.vars.declare(&v.name.to_ascii_lowercase());
                    restores.push(restore);
                    let name = VarName(canon);
                    // `%ROWTYPE`：记录锚定游标 → 单目标 FETCH INTO 按整行记录赋值
                    if let ogsql_parser::ast::plpgsql::PlDataType::PercentRowType(anchor) =
                        &v.data_type
                    {
                        self.rowtype_of
                            .insert(name.0.clone(), anchor.to_ascii_lowercase());
                    }
                    if let ogsql_parser::ast::plpgsql::PlDataType::TypeName(raw) = &v.data_type {
                        if let Some(ty) = self.rules.spin_type(raw) {
                            self.var_types.insert(name.0.clone(), ty);
                        }
                    }
                    let init = match &v.default {
                        Some(e) => self.expr(e)?,
                        None => IrExpr::Const(SpinValue::Null),
                    };
                    self.code.push(IrStmt::Assign {
                        target: AssignTarget::Var(name),
                        expr: init,
                    });
                }
                // TYPE .. IS TABLE OF / VARRAY：集合类型解释器未实现，编译期显式
                // 拒绝（ENG-0071，设计 V4 §4.4 Gate 2A）；RECORD/REF CURSOR 暂不涉及。
                PlDeclaration::Type(
                    ogsql_parser::ast::plpgsql::PlTypeDecl::TableOf { .. }
                    | ogsql_parser::ast::plpgsql::PlTypeDecl::VarrayOf { .. },
                ) => {
                    return Err(EngineError::invariant(
                        71,
                        "TYPE .. IS TABLE OF / VARRAY is not supported",
                    ));
                }
                // PRAGMA AUTONOMOUS_TRANSACTION / EXCEPTION_INIT：解释器均未实现
                // （后者静默丢弃会让 `WHEN <name>` 永不命中）——编译期显式拒绝
                // （ENG-0071，设计 V4 §4.4 Gate 2A）。其余 PRAGMA 维持忽略。
                PlDeclaration::Pragma { name, .. }
                    if name.eq_ignore_ascii_case("autonomous_transaction")
                        || name.eq_ignore_ascii_case("exception_init") =>
                {
                    return Err(EngineError::invariant(
                        71,
                        "PRAGMA AUTONOMOUS_TRANSACTION / EXCEPTION_INIT is not supported",
                    ));
                }
                // RECORD/TYPE/PRAGMA/嵌套例程：v1 不影响帧布局（嵌套例程 M9）
                _ => {}
            }
        }
        Ok(restores)
    }

    /// PL 变量面名 → 帧规范名（嵌套遮蔽后的唯一槽位；未登记原样返回）。
    pub(crate) fn var_name(&self, path: &str) -> VarName {
        let s = path.to_ascii_lowercase();
        if let Some(c) = self.vars.resolve_opt(&s) {
            return VarName(c.to_string());
        }
        // 限定路径（`rec.field`）：首段命中变量映射时仅替换首段。
        if let Some((head, tail)) = s.split_once('.') {
            if let Some(c) = self.vars.resolve_opt(head) {
                return VarName(format!("{c}{tail}"));
            }
        }
        VarName(s)
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

    /// `body … EXCEPTION WHEN …` 段的编译（唯一实现点）：
    /// PushTrap → body → PopTrap → Jump(resume) → handlers（每臂末 Jump(resume)）→ resume。
    /// 块级（`BEGIN..EXCEPTION`）与例程级（过程体尾部 EXCEPTION 段）共用。
    pub(crate) fn trap_section(
        &mut self,
        body: &[PlStatement],
        exc: &ogsql_parser::ast::plpgsql::PlExceptionBlock,
    ) -> Result<(), EngineError> {
        let resume = self.synth_label("trap_resume");
        self.code.push(IrStmt::PushTrap {
            entries: Vec::new(),
        }); // 占位，回填
        let trap_at = self.here() - 1;
        self.stmts(body)?;
        self.code.push(IrStmt::PopTrap);
        self.jump(&resume, None, true);
        let mut entries = Vec::new();
        for h in &exc.handlers {
            let body_start = self.here();
            self.stmts(&h.statements)?;
            let body_end = self.here();
            self.jump(&resume, None, true);
            entries.push(TrapEntry {
                conds: h
                    .conditions
                    .iter()
                    .map(|c| c.to_ascii_lowercase())
                    .collect(),
                body_start,
                body_end,
            });
        }
        self.mark(&resume);
        self.code[trap_at] = IrStmt::PushTrap { entries };
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
