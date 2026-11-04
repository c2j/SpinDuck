//! IR：PlBlock → 扁平指令序列（标签解析为 PC 跳转），M2 范围：
//! 赋值 / IF / LOOP / WHILE / EXIT / CONTINUE / GOTO / RETURN / RAISE /
//! 事务标记 / 嵌套 Block。SQL 类语句自 M3 起逐里程碑纳入，未纳入的编译期
//! 显式拒绝（ENG 域），禁止静默错误翻译。

use ogsql_parser::ast::plpgsql::{PlBlock, PlStatement};
use ogsql_parser::ast::Expr;

use crate::error::EngineError;
use crate::value::SpinValue;

/// 变量名 Newtype（AGENTS §5 边界类型纪律）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VarName(pub String);

/// 标签 Newtype。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label(pub String);

/// 赋值目标：变量 / 记录字段（编译期按帧作用域消歧，设计 §3.2）。
#[derive(Debug, Clone)]
pub enum AssignTarget {
    Var(VarName),
    Field { rec: VarName, field: String },
}

/// 表达式 IR（AST 的可求值子集；SQL 侧表达式经改写后下沉，不出现在此）。
#[derive(Debug, Clone)]
pub enum IrExpr {
    Const(SpinValue),
    Var(VarName),
    Field {
        rec: VarName,
        field: String,
    },
    Bin {
        op: BinOp,
        l: Box<IrExpr>,
        r: Box<IrExpr>,
    },
    Not(Box<IrExpr>),
    IsNull(Box<IrExpr>),
    /// 函数调用：PL 上下文一律解释器直调（M6 接例程表；M2 求值时报 ENG）
    FuncCall {
        name: String,
        args: Vec<IrExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// 指令（PC 为编译后索引；标签在代码生成期解析）。
#[derive(Debug, Clone)]
pub enum IrStmt {
    Nop,
    Assign {
        target: AssignTarget,
        expr: IrExpr,
    },
    Jump(usize),
    JumpIfNot {
        cond: IrExpr,
        to: usize,
    },
    JumpIf {
        cond: IrExpr,
        to: usize,
    },
    Raise {
        message: IrExpr,
    },
    Return {
        value: Option<IrExpr>,
    },
    /// 事务边界标记（§3.5 延迟事务，顶层统一执行）
    TxCommit,
    TxRollback,
}

/// 编译产物：线性指令序列。
#[derive(Debug, Clone, Default)]
pub struct IrProg {
    pub code: Vec<IrStmt>,
}

/// 编译器：AST 语句列表 → IR。
pub fn compile_stmts(stmts: &[PlStatement]) -> Result<IrProg, EngineError> {
    let mut c = Compiler::default();
    c.stmts(stmts)?;
    c.resolve_fixups()?;
    Ok(IrProg { code: c.code })
}

/// 从完整 PL 块编译（声明段 M2 忽略——变量按需在帧中创建）。
pub fn compile_block(block: &PlBlock) -> Result<IrProg, EngineError> {
    compile_stmts(&block.body)
}

#[derive(Default)]
struct Compiler {
    code: Vec<IrStmt>,
    labels: std::collections::HashMap<String, usize>,
    fixups: Vec<(usize, String)>,
    loop_stack: Vec<(String, String)>, // (start_label, end_label)
    depth: usize,
    synth: usize,
}

const MAX_DEPTH: usize = 64;

impl Compiler {
    fn here(&self) -> usize {
        self.code.len()
    }

    fn mark(&mut self, label: &str) {
        self.labels.insert(label.to_string(), self.here());
    }

    fn jump(&mut self, label: &str, cond: Option<IrExpr>, if_cond: bool) {
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

    fn resolve_fixups(&mut self) -> Result<(), EngineError> {
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

    fn synth_label(&mut self, hint: &str) -> String {
        self.synth += 1;
        format!("__{hint}_{}", self.synth)
    }

    fn stmts(&mut self, stmts: &[PlStatement]) -> Result<(), EngineError> {
        for st in stmts {
            self.stmt(st)?;
        }
        Ok(())
    }

    fn stmt(&mut self, st: &PlStatement) -> Result<(), EngineError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(EngineError::invariant(21, "PL nesting too deep"));
        }
        let r = self.stmt_inner(st);
        self.depth -= 1;
        r
    }

    fn stmt_inner(&mut self, st: &PlStatement) -> Result<(), EngineError> {
        match st {
            PlStatement::Null | PlStatement::Commit { .. } => {
                self.code.push(IrStmt::TxCommit);
            }
            PlStatement::Rollback { .. } => self.code.push(IrStmt::TxRollback),
            PlStatement::Savepoint { .. }
            | PlStatement::ReleaseSavepoint { .. }
            | PlStatement::SetTransaction { .. } => self.code.push(IrStmt::Nop),
            PlStatement::Assignment { target, expression } => {
                let t = self.assign_target(target)?;
                let e = self.expr(expression)?;
                self.code.push(IrStmt::Assign { target: t, expr: e });
            }
            PlStatement::If(i) => {
                // 条件链：JumpIfNot(else_i) → then → Jump(end) → [elsif…] → else
                let end = self.synth_label("if_end");
                let mut next = self.synth_label("if_next");
                let cond = self.expr(&i.condition)?;
                self.jump(&next, Some(cond), false);
                self.stmts(&i.then_stmts)?;
                self.jump(&end, None, true);
                for ei in &i.elsifs {
                    self.mark(&next);
                    next = self.synth_label("if_next");
                    let cond = self.expr(&ei.condition)?;
                    self.jump(&next, Some(cond), false);
                    self.stmts(&ei.stmts)?;
                    self.jump(&end, None, true);
                }
                if !i.else_stmts.is_empty() {
                    self.mark(&next);
                    self.stmts(&i.else_stmts)?;
                }
                self.mark(&end);
            }
            PlStatement::Loop(l) => {
                let start = l
                    .label
                    .clone()
                    .unwrap_or_else(|| self.synth_label("loop_start"));
                let end = l
                    .end_label
                    .clone()
                    .unwrap_or_else(|| format!("{start}__end"));
                self.mark(&start);
                self.loop_stack.push((start.clone(), end.clone()));
                self.stmts(&l.body)?;
                self.loop_stack.pop();
                self.jump(&start, None, true); // 无条件回跳 = LOOP
                self.mark(&end);
            }
            PlStatement::While(w) => {
                let start = w
                    .label
                    .clone()
                    .unwrap_or_else(|| self.synth_label("while_start"));
                let end = w
                    .end_label
                    .clone()
                    .unwrap_or_else(|| format!("{start}__end"));
                self.mark(&start);
                let cond = self.expr(&w.condition)?;
                self.jump(&end, Some(cond), false);
                self.loop_stack.push((start.clone(), end.clone()));
                self.stmts(&w.body)?;
                self.loop_stack.pop();
                self.jump(&start, None, true);
                self.mark(&end);
            }
            PlStatement::Exit { label, condition } => {
                let target = self.loop_end_label(label.as_deref())?;
                let cond = condition.as_ref().map(|c| self.expr(c)).transpose()?;
                self.jump(&target, cond, true); // EXIT WHEN c → 条件真跳出
            }
            PlStatement::Continue { label, condition } => {
                let target = self.loop_start_label(label.as_deref())?;
                let cond = condition.as_ref().map(|c| self.expr(c)).transpose()?;
                self.jump(&target, cond, true); // CONTINUE WHEN c → 条件真回跳
            }
            PlStatement::Goto { label } => {
                // 向前跳到循环顶标签 = continue（§3.2）；一般标签跳转同机制
                self.jump(label, None, true);
            }
            PlStatement::Return { expression } => {
                let value = expression.as_ref().map(|e| self.expr(e)).transpose()?;
                self.code.push(IrStmt::Return { value });
            }
            PlStatement::Raise(ra) => {
                let msg = ra
                    .message
                    .clone()
                    .map(|m| IrExpr::Const(SpinValue::Str(m)))
                    .unwrap_or_else(|| IrExpr::Const(SpinValue::Str("EXCEPTION".into())));
                self.code.push(IrStmt::Raise { message: msg });
            }
            PlStatement::Block(b) => self.stmts(&b.body)?,
            // —— M2 之后的里程碑纳入（编译期显式拒绝，禁止静默错误翻译）——
            other => {
                return Err(EngineError::invariant(
                    22,
                    format!("statement not yet supported in M2 compiler: {other:?}"),
                ));
            }
        }
        Ok(())
    }

    fn loop_end_label(&mut self, explicit: Option<&str>) -> Result<String, EngineError> {
        if let Some(l) = explicit {
            // 具名退出标签：直接指向该标签名（LOOP 的 end_label 会 mark 到它）
            return Ok(l.to_string());
        }
        self.loop_stack
            .last()
            .map(|(_, e)| e.clone())
            .ok_or_else(|| EngineError::invariant(23, "EXIT outside loop"))
    }

    fn loop_start_label(&mut self, explicit: Option<&str>) -> Result<String, EngineError> {
        if let Some(l) = explicit {
            return Ok(l.to_string());
        }
        self.loop_stack
            .last()
            .map(|(s, _)| s.clone())
            .ok_or_else(|| EngineError::invariant(24, "CONTINUE outside loop"))
    }

    fn assign_target(&mut self, e: &Expr) -> Result<AssignTarget, EngineError> {
        match e {
            Expr::PlVariable(name) => Ok(AssignTarget::Var(VarName(ident_path(name)))),
            Expr::ColumnRef(name) => {
                // 首段为帧内记录变量 → RecordField（§3.2 消歧；M2 帧为编译期可见集合）
                if name.len() == 2 {
                    Ok(AssignTarget::Field {
                        rec: VarName(ident0(name)),
                        field: ident1(name),
                    })
                } else {
                    Ok(AssignTarget::Var(VarName(ident_path(name))))
                }
            }
            other => Err(EngineError::invariant(
                25,
                format!("unsupported assignment target: {other:?}"),
            )),
        }
    }

    fn expr(&mut self, e: &Expr) -> Result<IrExpr, EngineError> {
        match e {
            Expr::Literal(l) => Ok(IrExpr::Const(lit(l))),
            Expr::PlVariable(name) => Ok(IrExpr::Var(VarName(ident_path(name)))),
            Expr::ColumnRef(name) => {
                if name.len() == 2 {
                    Ok(IrExpr::Field {
                        rec: VarName(ident0(name)),
                        field: ident1(name),
                    })
                } else {
                    // 单段标识符在 PL 表达式中仍按变量解释（§3.2 消歧同源）
                    Ok(IrExpr::Var(VarName(ident_path(name))))
                }
            }
            Expr::BinaryOp { left, op, right } => Ok(IrExpr::Bin {
                op: binop(op)?,
                l: Box::new(self.expr(left)?),
                r: Box::new(self.expr(right)?),
            }),
            Expr::UnaryOp { op, expr } if op.eq_ignore_ascii_case("NOT") => {
                Ok(IrExpr::Not(Box::new(self.expr(expr)?)))
            }
            Expr::IsNull { expr, negated } => {
                let inner = self.expr(expr)?;
                Ok(if *negated {
                    IrExpr::Not(Box::new(IrExpr::IsNull(Box::new(inner))))
                } else {
                    IrExpr::IsNull(Box::new(inner))
                })
            }
            Expr::FunctionCall { name, args, .. } => Ok(IrExpr::FuncCall {
                name: ident_path(name),
                args: args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            // 其余表达式（LIKE/CASE/Between/子查询…）M2 不支持；SQL 侧表达式的
            // 大多数场景发生在 SQL 文本内（M3 改写管线），PL 侧按需逐里程碑扩展
            other => Err(EngineError::invariant(
                26,
                format!("PL expression not yet supported in M2 evaluator: {other:?}"),
            )),
        }
    }
}

fn lit(l: &ogsql_parser::ast::Literal) -> SpinValue {
    use ogsql_parser::ast::Literal as L;
    match l {
        L::Integer(i) => SpinValue::Int(*i),
        L::Float(f) | L::String(f) | L::EscapeString(f) | L::NationalString(f) => {
            SpinValue::Str(f.clone())
        }
        L::Boolean(b) => SpinValue::Bool(*b),
        L::Null => SpinValue::Null,
        L::BitString(s) | L::HexString(s) | L::DollarString { body: s, .. } => {
            SpinValue::Str(s.clone())
        }
    }
}

fn binop(op: &str) -> Result<BinOp, EngineError> {
    Ok(match op.to_ascii_uppercase().as_str() {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "||" => BinOp::Concat,
        "=" => BinOp::Eq,
        "<>" | "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        "AND" => BinOp::And,
        "OR" => BinOp::Or,
        _ => {
            return Err(EngineError::invariant(
                27,
                format!("unsupported operator: {op}"),
            ))
        }
    })
}

fn ident_path(name: &ogsql_parser::ast::ObjectName) -> String {
    name.iter()
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}

fn ident0(name: &ogsql_parser::ast::ObjectName) -> String {
    name.first().map(|i| i.value.clone()).unwrap_or_default()
}

fn ident1(name: &ogsql_parser::ast::ObjectName) -> String {
    name.get(1).map(|i| i.value.clone()).unwrap_or_default()
}
