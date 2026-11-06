//! 语句编译（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use crate::value::SpinValue;
use ogsql_parser::ast::plpgsql::PlStatement;
use ogsql_parser::ast::Expr;

impl Compiler {
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

    pub(crate) fn stmt_inner(&mut self, st: &PlStatement) -> Result<(), EngineError> {
        match st {
            // NULL 是空语句（不得产生事务边界）；只有 COMMIT 才是提交标记
            PlStatement::Null => self.code.push(IrStmt::Nop),
            PlStatement::Commit { .. } => self.code.push(IrStmt::TxCommit),
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
                            Some((sql, binds, sites)) => CursorSource::Static {
                                sql: sql.clone(),
                                binds: binds.clone(),
                                fn_sites: sites.clone(),
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
                // %ROWTYPE 单目标：整行记录（GaussDB `FETCH c INTO rec` 语义）
                let record = targets.len() == 1
                    && matches!(&targets[0], AssignTarget::Var(v)
                        if self
                            .rowtype_of
                            .get(&v.0)
                            .map(|c| *c == cname.0.to_ascii_lowercase())
                            .unwrap_or(false));
                self.code.push(IrStmt::Fetch {
                    record,
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
                        let unit =
                            rewrite_sql(&text, &self.vars, &self.rules, &self.routine_names)?;
                        let cname = self.synth_label("for_cur");
                        self.code.push(IrStmt::OpenCursor {
                            cursor: VarName(cname.clone()),
                            source: CursorSource::Static {
                                sql: unit.text,
                                binds: unit.binds,
                                fn_sites: unit.fn_sites,
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
                        let (sql, binds, sites) = self
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
                            source: CursorSource::Static {
                                sql,
                                binds,
                                fn_sites: sites,
                            },
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
                    fn_sites: Vec::new(),
                });
            }
            PlStatement::Raise(ra) => {
                // 语料两种形态：`RAISE;`（重抛，10 处）与 `RAISE '<msg>'`；
                // `RAISE <条件名>` / `RAISE_APPLICATION_ERROR` 语料零命中，显式拒绝
                // 而不是编造常量消息（旧实现把裸 RAISE 变成 "EXCEPTION" 假错误）。
                let ra = &ra.node;
                match (&ra.message, &ra.condname, &ra.sqlstate) {
                    (Some(m), None, None) => self.code.push(IrStmt::Raise {
                        message: Some(IrExpr::Const(SpinValue::Str(m.clone()))),
                    }),
                    (None, None, None) => self.code.push(IrStmt::Raise { message: None }),
                    (_, Some(name), _) => {
                        return Err(EngineError::business(
                            202,
                            format!("RAISE {name} (named condition) is not supported in v1"),
                        ))
                    }
                    (_, _, Some(state)) => {
                        return Err(EngineError::business(
                            202,
                            format!("RAISE WITH SQLSTATE {state} is not supported in v1"),
                        ))
                    }
                }
            }
            PlStatement::Block(b) => {
                // `<<label>>` 语句标签（GOTO 目标）：先 mark 再编译块体
                if let Some(l) = &b.label {
                    self.mark(l);
                }
                match &b.exception_block {
                    None => self.stmts(&b.body)?,
                    Some(exc) => self.trap_section(&b.body, exc)?,
                }
            }
            PlStatement::Execute(sp) => {
                // EXECUTE IMMEDIATE：统一走动态管线（与 OPEN FOR 同口径，
                // 决策记录 2025-11-05）。USING/INTO 为 v1 未支持的接缝，显式拒绝
                if !sp.using_args.is_empty() {
                    return Err(EngineError::invariant(
                        58,
                        "EXECUTE IMMEDIATE .. USING is not supported in v1",
                    ));
                }
                let text_expr = self.expr(&sp.string_expr)?;
                let mut into_targets = Vec::with_capacity(sp.into_targets.len());
                for t in &sp.into_targets {
                    into_targets.push(self.assign_target(t)?);
                }
                self.code.push(IrStmt::SqlExecDynamic {
                    text_expr: Box::new(text_expr),
                    into_targets,
                });
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
}
