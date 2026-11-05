//! IR：PlBlock → 扁平指令序列（标签解析为 PC 跳转），M2 范围：
//! 赋值 / IF / LOOP / WHILE / EXIT / CONTINUE / GOTO / RETURN / RAISE /
//! 事务标记 / 嵌套 Block。SQL 类语句自 M3 起逐里程碑纳入，未纳入的编译期
//! 显式拒绝（ENG 域），禁止静默错误翻译。

use ogsql_parser::ast::plpgsql::{PlBlock, PlStatement};
use ogsql_parser::ast::Expr;

use crate::error::EngineError;
use crate::rules::RuleRegistry;
use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;
use std::collections::BTreeMap;

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
    /// 游标属性：cursor%NOTFOUND 等（§7.3.3）
    CursorAttr {
        cursor: VarName,
        kind: CursorAttrKind,
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
    /// SELECT [INTO ...]（行数语义：0→NO_DATA_FOUND，≥2→TOO_MANY_ROWS）
    SqlQuery {
        sql: String,
        binds: Vec<(String, VarName)>,
        into: Vec<AssignTarget>,
    },
    /// DML/DDL（Truncate/Insert/Update/Delete/...）
    SqlExec {
        sql: String,
        binds: Vec<(String, VarName)>,
    },
    /// PERFORM <expr>：求值并丢弃
    Perform {
        expr: IrExpr,
    },
    /// OPEN 游标（§7.1：静态已改写 / 动态运行期文本）
    OpenCursor {
        cursor: VarName,
        source: CursorSource,
    },
    /// FETCH INTO（位置对位写槽；流尾不覆盖目标）
    Fetch {
        cursor: VarName,
        targets: Vec<AssignTarget>,
        /// true = 隐式游标（FOR rec IN ...）：单目标整体按 schema 建 RECORD
        record: bool,
    },
    CloseCursor {
        cursor: VarName,
    },
    /// 安装异常处理器表（§3.5；handlers 体在 handlers 区，start/end 已标记）
    PushTrap {
        entries: Vec<TrapEntry>,
    },
    /// 撤销当前处理器（正常路径离开 try 体）
    PopTrap,
    /// CALL 例程（§3.4.1）：OUT/INOUT 按 out_targets 位置写回调用方
    CallRoutine {
        name: String,
        args: Vec<IrExpr>,
        /// 与 params 序对应：Some = 写回目标，None = 丢弃
        out_targets: Vec<Option<AssignTarget>>,
    },
}

/// 单个处理器：条件列表 + 主体指令区间 [body_start, body_end)。
#[derive(Debug, Clone)]
pub struct TrapEntry {
    pub conds: Vec<String>,
    pub body_start: usize,
    pub body_end: usize,
}

#[derive(Debug, Clone)]
pub enum CursorSource {
    /// 静态游标：声明段 parsed_query 经改写管线后的文本与绑定
    Static {
        sql: String,
        binds: Vec<(String, VarName)>,
    },
    /// 动态 OPEN FOR <变量>（运行期求值文本，M8 前不做人内变量替换）
    Dynamic { query_expr: IrExpr },
}

/// 编译产物：线性指令序列。
#[derive(Debug, Clone, Default)]
pub struct IrProg {
    pub code: Vec<IrStmt>,
}

impl IrProg {
    /// 纯表达式函数体识别（§8.2 路径③）：仅允许 `Nop` + 唯一 `RETURN <expr>`。
    ///
    /// 该形态不含局部变量/分支/副作用，表达式可直接下沉为内核宏；
    /// 任何其它指令都返回 `None`（调用方据此拒绝宏登记，不做近似翻译）。
    pub fn single_return_expr(&self) -> Option<&IrExpr> {
        let mut ret = None;
        for st in &self.code {
            match st {
                IrStmt::Nop => {}
                IrStmt::Return { value: Some(e) } if ret.is_none() => ret = Some(e),
                _ => return None,
            }
        }
        ret
    }
}

/// 编译器：AST 语句列表 → IR。预扫描收集 PL 变量名集合（SQL 改写用）。
pub fn compile_stmts(stmts: &[PlStatement]) -> Result<IrProg, EngineError> {
    compile_stmts_with(stmts, &RuleRegistry::default())
}

/// 带规则注册表的编译（SQL 改写应用函数映射与解析级变换）。
pub fn compile_stmts_with(
    stmts: &[PlStatement],
    rules: &RuleRegistry,
) -> Result<IrProg, EngineError> {
    let vars = collect_var_names(stmts);
    let mut c = Compiler {
        vars,
        funcs: rules.functions.clone(),
        ..Default::default()
    };
    c.stmts(stmts)?;
    c.resolve_fixups()?;
    Ok(IrProg { code: c.code })
}

/// 从完整 PL 块编译：声明段静态游标在此编译（§7.2）。
pub fn compile_block(block: &PlBlock) -> Result<IrProg, EngineError> {
    compile_block_with(block, &RuleRegistry::default())
}

/// 带规则注册表的块编译。
pub fn compile_block_with(block: &PlBlock, rules: &RuleRegistry) -> Result<IrProg, EngineError> {
    compile_routine_with(block, &[], rules)
}

/// 例程块编译：额外把参数名加入变量集（M6：CALL 参数即帧变量）。
pub fn compile_routine_with(
    block: &PlBlock,
    extra_vars: &[String],
    rules: &RuleRegistry,
) -> Result<IrProg, EngineError> {
    let mut vars = collect_var_names(&block.body);
    for v in extra_vars {
        vars.insert(v.to_ascii_lowercase());
    }
    let mut c = Compiler {
        vars,
        funcs: rules.functions.clone(),
        ..Default::default()
    };
    use ogsql_parser::ast::plpgsql::PlDeclaration;
    // 声明段：变量入变量集并生成初始化指令（GaussDB 语义：声明即变量，
    // 未给初值时为 NULL）；游标编译为静态 SQL 待 OPEN 使用
    for decl in &block.declarations {
        match decl {
            PlDeclaration::Cursor(cd) => {
                let name = cd.name.to_ascii_lowercase();
                let query_text = match &cd.parsed_query {
                    Some(q) => ogsql_parser::formatter::SqlFormatter::new().format_statement(q),
                    None => cd.query.clone(),
                };
                let unit = crate::sql::rewrite_tokens_with_rules(&query_text, &c.vars, &c.funcs)?;
                let text = crate::sql::apply_ast_transforms(&unit.text)?;
                c.pending_cursor_sql.insert(name, (text, unit.binds));
            }
            PlDeclaration::Variable(v) => {
                let name = VarName(v.name.to_ascii_lowercase());
                c.vars.insert(name.0.clone());
                let init = match &v.default {
                    Some(e) => c.expr(e)?,
                    None => IrExpr::Const(SpinValue::Null),
                };
                c.code.push(IrStmt::Assign {
                    target: AssignTarget::Var(name),
                    expr: init,
                });
            }
            // RECORD/TYPE/PRAGMA/嵌套例程：v1 不影响帧布局（嵌套例程 M9）
            _ => {}
        }
    }
    c.stmts(&block.body)?;
    c.resolve_fixups()?;
    Ok(IrProg { code: c.code })
}
/// 预扫描：赋值目标 + PL 表达式中的 PlVariable 引用（不进入 SQL 语句内部）。
/// 扩展点（M6）：例程参数与 DECLARE 声明同样入集（GaussDB 语义：声明即变量）。
fn collect_var_names(stmts: &[PlStatement]) -> crate::sql::VarScope {
    let mut set = crate::sql::VarScope::new();
    for st in stmts {
        collect_stmt(st, &mut set);
    }
    set
}

fn collect_slice(stmts: &[PlStatement], set: &mut crate::sql::VarScope) {
    for st in stmts {
        collect_stmt(st, set);
    }
}

fn collect_stmt(st: &PlStatement, set: &mut crate::sql::VarScope) {
    match st {
        PlStatement::Assignment { target, expression } => {
            collect_expr(target, set);
            collect_expr(expression, set);
        }
        PlStatement::If(i) => {
            collect_expr(&i.condition, set);
            collect_slice(&i.then_stmts, set);
            for ei in &i.elsifs {
                collect_expr(&ei.condition, set);
                collect_slice(&ei.stmts, set);
            }
            collect_slice(&i.else_stmts, set);
        }
        PlStatement::Loop(l) => collect_slice(&l.body, set),
        PlStatement::While(w) => {
            collect_expr(&w.condition, set);
            collect_slice(&w.body, set);
        }
        PlStatement::Exit { condition, .. } | PlStatement::Continue { condition, .. } => {
            if let Some(c) = condition {
                collect_expr(c, set);
            }
        }
        PlStatement::Return {
            expression: Some(e),
        } => collect_expr(e, set),
        PlStatement::Block(b) => collect_slice(&b.body, set),
        _ => {}
    }
}

fn collect_expr(e: &Expr, set: &mut crate::sql::VarScope) {
    match e {
        Expr::PlVariable(name) => {
            set.insert(ident_path(name).to_ascii_lowercase());
        }
        Expr::ColumnRef(name) if name.len() == 2 => {
            set.insert(ident0(name).to_ascii_lowercase());
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr(left, set);
            collect_expr(right, set);
        }
        Expr::UnaryOp { expr, .. } => collect_expr(expr, set),
        Expr::IsNull { expr, .. } => collect_expr(expr, set),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_expr(a, set);
            }
        }
        _ => {}
    }
}

fn rewrite_sql(
    sql_text: &str,
    vars: &crate::sql::VarScope,
    funcs: &BTreeMap<String, String>,
) -> Result<crate::sql::SqlUnit, EngineError> {
    let unit = crate::sql::rewrite_tokens_with_rules(sql_text, vars, funcs)?;
    let text = crate::sql::apply_ast_transforms(&unit.text)?;
    Ok(crate::sql::SqlUnit {
        text,
        binds: unit.binds,
    })
}

#[derive(Default)]
pub(crate) struct Compiler {
    vars: crate::sql::VarScope,
    /// 静态游标声明 → 编译后的 (sql, binds)（compile_block_with 填充，§7.2）
    pending_cursor_sql: BTreeMap<String, (String, Vec<(String, VarName)>)>,
    funcs: BTreeMap<String, String>,
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

    /// FOR 隐式游标循环体：FETCH → %NOTFOUND 退出 → 循环体 → 回跳。
    fn for_cursor_loop(
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
                // 空 ELSE 时 `next` 仍必须是跳转目标（= end），否则 fixup 悬空
                self.mark(&next);
                if !i.else_stmts.is_empty() {
                    self.stmts(&i.else_stmts)?;
                }
                self.mark(&end);
            }
            PlStatement::Case(cs) => {
                // PL CASE → IF 链脱糖：简单 CASE 先求选择子一次（临时变量），
                // 搜索 CASE 直接取 when 条件；语义与 SQL CASE 一致（首个命中）
                let end = self.synth_label("case_end");
                let sel = match &cs.expression {
                    Some(e) => {
                        let tmp = VarName(self.synth_label("case_sel"));
                        let init = self.expr(e)?;
                        self.code.push(IrStmt::Assign {
                            target: AssignTarget::Var(tmp.clone()),
                            expr: init,
                        });
                        Some(tmp)
                    }
                    None => None,
                };
                for w in &cs.whens {
                    let next = self.synth_label("case_next");
                    let cond = match &sel {
                        Some(tmp) => IrExpr::Bin {
                            op: BinOp::Eq,
                            l: Box::new(IrExpr::Var(tmp.clone())),
                            r: Box::new(self.expr(&w.condition)?),
                        },
                        None => self.expr(&w.condition)?,
                    };
                    self.jump(&next, Some(cond), false);
                    self.stmts(&w.stmts)?;
                    self.jump(&end, None, true);
                    self.mark(&next);
                }
                if !cs.else_stmts.is_empty() {
                    self.stmts(&cs.else_stmts)?;
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
            PlStatement::Open(open_stmt) => {
                let (cursor, kind) = (&open_stmt.cursor, &open_stmt.kind);
                let cname = match cursor {
                    Expr::PlVariable(n) | Expr::ColumnRef(n) => VarName(ident_path(n)),
                    other => {
                        return Err(EngineError::invariant(
                            48,
                            format!("unsupported cursor reference: {other:?}"),
                        ));
                    }
                };
                let source = match kind {
                    ogsql_parser::ast::plpgsql::PlOpenKind::Simple { .. } => {
                        // 静态游标：声明段 parsed_query 已在 compile_block_with 编译
                        match self.pending_cursor_sql.get(&cname.0.to_ascii_lowercase()) {
                            Some((sql, binds)) => CursorSource::Static {
                                sql: sql.clone(),
                                binds: binds.clone(),
                            },
                            None => {
                                return Err(EngineError::invariant(
                                    49,
                                    format!("cursor {} has no compiled query", cname.0),
                                ));
                            }
                        }
                    }
                    ogsql_parser::ast::plpgsql::PlOpenKind::ForQuery { query, .. } => {
                        let vexpr =
                            ogsql_parser::ast::Expr::PlVariable(vec![ogsql_parser::ast::Ident {
                                value: query.clone(),
                                quote_style: None,
                            }]);
                        CursorSource::Dynamic {
                            query_expr: self.expr(&vexpr)?,
                        }
                    }
                    ogsql_parser::ast::plpgsql::PlOpenKind::ForUsing { .. }
                    | ogsql_parser::ast::plpgsql::PlOpenKind::ForExecute { .. } => {
                        return Err(EngineError::invariant(
                            51,
                            "OPEN FOR EXECUTE/USING (dynamic with USING) is M8 scope",
                        ));
                    }
                };
                self.code.push(IrStmt::OpenCursor {
                    cursor: cname,
                    source,
                });
            }
            PlStatement::Fetch(fetch_stmt) => {
                let (cursor, direction, bulk_collect, into) = (
                    &fetch_stmt.cursor,
                    &fetch_stmt.direction,
                    &fetch_stmt.bulk_collect,
                    &fetch_stmt.into,
                );
                if direction.is_some() || *bulk_collect {
                    return Err(EngineError::invariant(
                        52,
                        "scroll fetch / BULK COLLECT is not supported in v1",
                    ));
                }
                let cname = match &cursor {
                    Expr::PlVariable(n) | Expr::ColumnRef(n) => VarName(ident_path(n)),
                    other => {
                        return Err(EngineError::invariant(
                            53,
                            format!("unsupported cursor reference: {other:?}"),
                        ));
                    }
                };
                let targets = into
                    .iter()
                    .map(|e| self.assign_target(e))
                    .collect::<Result<Vec<_>, _>>()?;
                self.code.push(IrStmt::Fetch {
                    record: false,
                    cursor: cname,
                    targets,
                });
            }
            PlStatement::Close { cursor, .. } => {
                let cname = match &cursor {
                    Expr::PlVariable(n) | Expr::ColumnRef(n) => VarName(ident_path(n)),
                    other => {
                        return Err(EngineError::invariant(
                            54,
                            format!("unsupported cursor reference: {other:?}"),
                        ));
                    }
                };
                self.code.push(IrStmt::CloseCursor { cursor: cname });
            }
            PlStatement::For(f) => {
                // Range → 计数循环；Query → 隐式游标循环（M4c 打开）
                use ogsql_parser::ast::plpgsql::PlForKind;
                let var = VarName(f.variable.to_ascii_lowercase());
                let start = f
                    .label
                    .clone()
                    .unwrap_or_else(|| self.synth_label("for_start"));
                let end = f
                    .end_label
                    .clone()
                    .unwrap_or_else(|| format!("{start}__end"));
                self.loop_stack.push((start.clone(), end.clone()));
                match &f.kind {
                    PlForKind::Range {
                        low,
                        high,
                        step,
                        reverse,
                    } => {
                        if *reverse {
                            return Err(EngineError::invariant(
                                55,
                                "FOR REVERSE is not supported in v1",
                            ));
                        }
                        let e_low = self.expr(low)?;
                        self.code.push(IrStmt::Assign {
                            target: AssignTarget::Var(var.clone()),
                            expr: e_low,
                        });
                        self.mark(&start);
                        let cond = IrExpr::Bin {
                            op: BinOp::Le,
                            l: Box::new(IrExpr::Var(var.clone())),
                            r: Box::new(self.expr(high)?),
                        };
                        self.jump(&end, Some(cond), false);
                        self.stmts(&f.body)?;
                        let step_inc = match step {
                            Some(s) => self.expr(s)?,
                            None => IrExpr::Const(SpinValue::Int(1)),
                        };
                        self.code.push(IrStmt::Assign {
                            target: AssignTarget::Var(var.clone()),
                            expr: IrExpr::Bin {
                                op: BinOp::Add,
                                l: Box::new(IrExpr::Var(var.clone())),
                                r: Box::new(step_inc),
                            },
                        });
                        self.jump(&start, None, true);
                    }
                    PlForKind::Query {
                        query,
                        parsed_query,
                        using_args,
                    } => {
                        // FOR rec IN <query>：脱糖为隐式游标（§7.3.4），
                        // 与显式游标共用 OPEN/FETCH/CLOSE 语义与 %NOTFOUND 判定
                        if !using_args.is_empty() {
                            return Err(EngineError::invariant(
                                58,
                                "FOR rec IN query USING is not supported in v1",
                            ));
                        }
                        let text = match parsed_query {
                            Some(st) => {
                                ogsql_parser::formatter::SqlFormatter::new().format_statement(st)
                            }
                            None => query.clone(),
                        };
                        let unit = rewrite_sql(&text, &self.vars, &self.funcs)?;
                        let cname = self.synth_label("for_cur");
                        self.code.push(IrStmt::OpenCursor {
                            cursor: VarName(cname.clone()),
                            source: CursorSource::Static {
                                sql: unit.text,
                                binds: unit.binds,
                            },
                        });
                        self.for_cursor_loop(&VarName(cname.clone()), &var, &f.body, &start, &end)?;
                        self.code.push(IrStmt::CloseCursor {
                            cursor: VarName(cname),
                        });
                    }
                    PlForKind::Cursor {
                        cursor_name,
                        arguments,
                    } => {
                        if !arguments.is_empty() {
                            return Err(EngineError::invariant(
                                59,
                                "FOR rec IN cursor(args) is not supported in v1",
                            ));
                        }
                        let name = match cursor_name {
                            Expr::PlVariable(n) | Expr::ColumnRef(n) => ident_path(n),
                            other => {
                                return Err(EngineError::invariant(
                                    60,
                                    format!("unsupported cursor reference: {other:?}"),
                                ));
                            }
                        };
                        // 复用声明段已编译的静态 SQL（游标声明 = 唯一真源）
                        let (sql, binds) = self
                            .pending_cursor_sql
                            .get(&name.to_ascii_lowercase())
                            .cloned()
                            .ok_or_else(|| {
                                EngineError::invariant(
                                    61,
                                    format!("FOR rec IN {name}: cursor query not compiled"),
                                )
                            })?;
                        let cname = VarName(name.clone());
                        self.code.push(IrStmt::OpenCursor {
                            cursor: cname.clone(),
                            source: CursorSource::Static { sql, binds },
                        });
                        self.for_cursor_loop(&cname, &var, &f.body, &start, &end)?;
                        self.code.push(IrStmt::CloseCursor { cursor: cname });
                    }
                }
                self.loop_stack.pop();
                self.mark(&end);
            }
            PlStatement::ProcedureCall(pc_stmt) => {
                let name = ident_path(&pc_stmt.name);
                let mut args = Vec::new();
                let mut out_targets = Vec::new();
                for a in &pc_stmt.arguments {
                    args.push(self.expr(a)?);
                    // OUT/INOUT 实参语法位：变量/记录字段可写回；字面量等 None
                    out_targets.push(self.assign_target_lenient(a));
                }
                self.code.push(IrStmt::CallRoutine {
                    name,
                    args,
                    out_targets,
                });
            }
            PlStatement::SqlStatement {
                sql_text,
                statement,
                ..
            } => {
                self.sql_statement(sql_text, statement)?;
            }
            PlStatement::Perform { query, .. } => {
                // PERFORM expr → SELECT expr 求值丢弃（M3 简化实现）
                let unit =
                    crate::sql::rewrite_tokens(&format!("SELECT {}", query.trim()), &self.vars)?;
                self.code.push(IrStmt::SqlExec {
                    sql: unit.text,
                    binds: unit.binds,
                });
            }
            PlStatement::Raise(ra) => {
                let msg = ra
                    .message
                    .clone()
                    .map(|m| IrExpr::Const(SpinValue::Str(m)))
                    .unwrap_or_else(|| IrExpr::Const(SpinValue::Str("EXCEPTION".into())));
                self.code.push(IrStmt::Raise { message: msg });
            }
            PlStatement::Block(b) => {
                // `<<label>>` 语句标签（GOTO 目标）：先 mark 再编译块体
                if let Some(l) = &b.label {
                    self.mark(l);
                }
                match &b.exception_block {
                    None => self.stmts(&b.body)?,
                    // BEGIN..EXCEPTION：PushTrap → try 体 → PopTrap → Jump(resume)
                    // → handlers 区（每臂 Jump(resume)）→ resume:
                    Some(exc) => {
                        let resume = self.synth_label("trap_resume");
                        let mut entries = Vec::new();
                        self.code.push(IrStmt::PushTrap {
                            entries: Vec::new(),
                        }); // 占位，回填
                        let trap_at = self.here() - 1;
                        self.stmts(&b.body)?;
                        self.code.push(IrStmt::PopTrap);
                        self.jump(&resume, None, true);
                        // handlers 区
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
                        // 回填 PushTrap.entries
                        self.code[trap_at] = IrStmt::PushTrap { entries };
                    }
                }
            }
            PlStatement::Execute(sp) => {
                // EXECUTE IMMEDIATE：只有"静态文本 + 已解析语句"可编译期下沉；
                // 运行期拼接（动态）保留为 M8 范围并显式拒绝（§10.4 注入面）
                let st = sp.parsed_query.as_ref().ok_or_else(|| {
                    EngineError::invariant(
                        62,
                        "EXECUTE with runtime-built SQL text is deferred to M8",
                    )
                })?;
                if !sp.using_args.is_empty() {
                    return Err(EngineError::invariant(
                        58,
                        "EXECUTE IMMEDIATE .. USING is not supported in v1",
                    ));
                }
                let text = ogsql_parser::formatter::SqlFormatter::new().format_statement(st);
                if sp.into_targets.is_empty() {
                    self.sql_statement(&text, st)?;
                } else {
                    let mut into = Vec::with_capacity(sp.into_targets.len());
                    for t in &sp.into_targets {
                        into.push(self.assign_target(t)?);
                    }
                    let unit = rewrite_sql(&text, &self.vars, &self.funcs)?;
                    self.code.push(IrStmt::SqlQuery {
                        sql: unit.text,
                        binds: unit.binds,
                        into,
                    });
                }
            }
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

    /// SqlStatement 编译：SELECT INTO → SqlQuery（AST 摘除 INTO 后经 formatter
    /// 重建文本）；DML/DDL → SqlExec（原始 sql_text token 改写）。
    fn sql_statement(
        &mut self,
        sql_text: &str,
        statement: &ogsql_parser::ast::Statement,
    ) -> Result<(), EngineError> {
        use ogsql_parser::ast::Statement as S;
        match statement {
            S::Select(sel) => {
                let into_targets = sel.into_targets.clone();
                // 摘除 INTO 后经 formatter 重建 SQL 文本（DuckDB 不支持 INTO 子句）
                let mut cleaned = sel.node.clone();
                cleaned.into_targets = None;
                let formatted = ogsql_parser::formatter::SqlFormatter::new().format_statement(
                    &ogsql_parser::ast::Statement::Select(ogsql_parser::ast::Spanned::new(
                        cleaned, None,
                    )),
                );
                let unit = rewrite_sql(&formatted, &self.vars, &self.funcs)?;
                let into = match into_targets {
                    None => Vec::new(),
                    Some(targets) => {
                        let mut out = Vec::new();
                        for t in targets {
                            match t {
                                ogsql_parser::ast::SelectTarget::Expr(e, _) => {
                                    out.push(self.assign_target(&e)?);
                                }
                                ogsql_parser::ast::SelectTarget::Star(_) => {
                                    return Err(EngineError::invariant(
                                        42,
                                        "SELECT * INTO is not supported",
                                    ));
                                }
                            }
                        }
                        out
                    }
                };
                let (sql, binds) = (unit.text, unit.binds);
                self.code.push(IrStmt::SqlQuery { sql, binds, into });
            }
            S::Insert(_) | S::Update(_) | S::Delete(_) | S::Truncate(_) => {
                let unit = rewrite_sql(sql_text, &self.vars, &self.funcs)?;
                let (sql, binds) = (unit.text, unit.binds);
                self.code.push(IrStmt::SqlExec { sql, binds });
            }
            other => {
                return Err(EngineError::invariant(
                    43,
                    format!("SQL statement kind not yet supported (M8/M9 scope): {other:?}"),
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

    /// OUT/INOUT 实参的写回目标（宽松版）：非变量/记录字段实参 → None（值传递）。
    fn assign_target_lenient(&mut self, e: &Expr) -> Option<AssignTarget> {
        self.assign_target(e).ok()
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
            Expr::CursorAttribute { cursor, attribute } => {
                let cname = match cursor.as_ref() {
                    Expr::PlVariable(n) | Expr::ColumnRef(n) => VarName(ident_path(n)),
                    other => {
                        return Err(EngineError::invariant(
                            46,
                            format!("unsupported cursor reference: {other:?}"),
                        ));
                    }
                };
                let kind = match attribute {
                    ogsql_parser::ast::CursorAttributeKind::NotFound => CursorAttrKind::NotFound,
                    ogsql_parser::ast::CursorAttributeKind::Found => CursorAttrKind::Found,
                    ogsql_parser::ast::CursorAttributeKind::IsOpen => CursorAttrKind::IsOpen,
                    ogsql_parser::ast::CursorAttributeKind::RowCount => CursorAttrKind::RowCount,
                    ogsql_parser::ast::CursorAttributeKind::BulkExceptions => {
                        return Err(EngineError::invariant(
                            47,
                            "BULK_EXCEPTIONS cursor attribute is not supported",
                        ));
                    }
                };
                Ok(IrExpr::CursorAttr {
                    cursor: cname,
                    kind,
                })
            }
            Expr::FunctionCall { name, args, .. } => Ok(IrExpr::FuncCall {
                name: ident_path(name),
                args: args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            Expr::Parenthesized(inner) => self.expr(inner),
            Expr::UnaryOp { op, expr } if op == "-" => Ok(IrExpr::Bin {
                op: BinOp::Sub,
                l: Box::new(IrExpr::Const(SpinValue::Int(0))),
                r: Box::new(self.expr(expr)?),
            }),
            Expr::UnaryOp { op, expr } if op == "+" => self.expr(expr),
            // BETWEEN 闭区间（§3.2 求值口径与 SQL 一致）：NOT BETWEEN 为区间外
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let x = self.expr(expr)?;
                let lo = self.expr(low)?;
                let hi = self.expr(high)?;
                let ge = IrExpr::Bin {
                    op: BinOp::Ge,
                    l: Box::new(x.clone()),
                    r: Box::new(lo),
                };
                let le = IrExpr::Bin {
                    op: BinOp::Le,
                    l: Box::new(x.clone()),
                    r: Box::new(hi),
                };
                if *negated {
                    let lt = IrExpr::Bin {
                        op: BinOp::Lt,
                        l: Box::new(x.clone()),
                        r: Box::new(self.expr(low)?),
                    };
                    let gt = IrExpr::Bin {
                        op: BinOp::Gt,
                        l: Box::new(x),
                        r: Box::new(self.expr(high)?),
                    };
                    Ok(or_chain(vec![lt, gt]))
                } else {
                    Ok(and_chain(vec![ge, le]))
                }
            }
            // IN 列表：等式析取（NOT IN 为全部不等）
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let x = self.expr(expr)?;
                let mut parts = Vec::with_capacity(list.len());
                for item in list {
                    let eq = IrExpr::Bin {
                        op: if *negated { BinOp::Ne } else { BinOp::Eq },
                        l: Box::new(x.clone()),
                        r: Box::new(self.expr(item)?),
                    };
                    parts.push(eq);
                }
                Ok(if *negated {
                    and_chain(parts)
                } else {
                    or_chain(parts)
                })
            }
            // 其余表达式（LIKE/CASE/子查询…）M2 不支持；SQL 侧表达式的
            // 大多数场景发生在 SQL 文本内（M3 改写管线），PL 侧按需逐里程碑扩展
            other => Err(EngineError::invariant(
                26,
                format!("PL expression not yet supported in M2 evaluator: {other:?}"),
            )),
        }
    }
}

/// 条件链左结合组合（OR）：`a OR b OR c`。
fn or_chain(parts: Vec<IrExpr>) -> IrExpr {
    fold_bin(BinOp::Or, parts)
}

/// 条件链左结合组合（AND）：`a AND b AND c`。
fn and_chain(parts: Vec<IrExpr>) -> IrExpr {
    fold_bin(BinOp::And, parts)
}

fn fold_bin(op: BinOp, parts: Vec<IrExpr>) -> IrExpr {
    let mut it = parts.into_iter();
    let first = it.next().expect("chain requires at least one item");
    it.fold(first, |acc, next| IrExpr::Bin {
        op,
        l: Box::new(acc),
        r: Box::new(next),
    })
}

fn lit(l: &ogsql_parser::ast::Literal) -> SpinValue {
    use ogsql_parser::ast::Literal as L;
    match l {
        L::Integer(i) => SpinValue::Int(*i),
        // 小数按十进制字面量承载（禁止降级为字符串：`x * 0.0008` 曾因字符串
        // 常量在 DuckDB 侧类型推断失败）
        L::Float(f) => match parse_decimal_literal(f) {
            Some(v) => v,
            None => SpinValue::Str(f.clone()),
        },
        L::String(f) | L::EscapeString(f) | L::NationalString(f) => SpinValue::Str(f.clone()),
        L::Boolean(b) => SpinValue::Bool(*b),
        L::Null => SpinValue::Null,
        L::BitString(s) | L::HexString(s) | L::DollarString { body: s, .. } => {
            SpinValue::Str(s.clone())
        }
    }
}

/// 十进制字面量 → `SpinValue::Decimal`（非标度值 + scale）；指数形式不支持时返回
/// `None`（调用方降级为字符串，行为显式可测）。
fn parse_decimal_literal(text: &str) -> Option<SpinValue> {
    let t = text.trim();
    if t.contains('e') || t.contains('E') {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (ip, fp) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if ip.is_empty() && fp.is_empty() {
        return None;
    }
    if !ip.chars().all(|c| c.is_ascii_digit()) || !fp.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scale = fp.len().min(18) as u8;
    let digits = format!("{ip}{fp}");
    let unscaled: i128 = digits.parse().ok()?;
    let signed = if neg { -unscaled } else { unscaled };
    Some(SpinValue::Decimal(signed, 38, scale))
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
