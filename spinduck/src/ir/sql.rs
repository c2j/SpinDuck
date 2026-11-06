//! SQL 类语句与赋值目标/循环标签（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use ogsql_parser::ast::Expr;

impl Compiler {
    /// SqlStatement 编译：SELECT INTO → SqlQuery（AST 摘除 INTO 后经 formatter
    /// 重建文本）；DML/DDL → SqlExec（原始 sql_text token 改写）。
    pub(crate) fn sql_statement(
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
                // [patterns] rownum_first_row：唯一实现见 sql::patterns
                if self.rules.patterns_rownum_first_row {
                    crate::sql::patterns::apply_on_select(&mut cleaned);
                }
                let formatted = ogsql_parser::formatter::SqlFormatter::new().format_statement(
                    &ogsql_parser::ast::Statement::Select(ogsql_parser::ast::Spanned::new(
                        cleaned, None,
                    )),
                );
                let unit = rewrite_sql(&formatted, &self.vars, &self.rules, &self.routine_names)?;
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
                let (sql, binds, fn_sites) = (unit.text, unit.binds, unit.fn_sites);
                self.code.push(IrStmt::SqlQuery {
                    sql,
                    binds,
                    into,
                    fn_sites,
                });
            }
            S::Insert(_) | S::Update(_) | S::Delete(_) | S::Truncate(_) => {
                let unit = rewrite_sql(sql_text, &self.vars, &self.rules, &self.routine_names)?;
                let (sql, binds, fn_sites) = (unit.text, unit.binds, unit.fn_sites);
                self.code.push(IrStmt::SqlExec {
                    sql,
                    binds,
                    fn_sites,
                });
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

    pub(crate) fn loop_end_label(&mut self, explicit: Option<&str>) -> Result<String, EngineError> {
        if let Some(l) = explicit {
            // 具名退出标签：直接指向该标签名（LOOP 的 end_label 会 mark 到它）
            return Ok(l.to_string());
        }
        self.loop_stack
            .last()
            .map(|(_, e)| e.clone())
            .ok_or_else(|| EngineError::invariant(23, "EXIT outside loop"))
    }

    pub(crate) fn loop_start_label(
        &mut self,
        explicit: Option<&str>,
    ) -> Result<String, EngineError> {
        if let Some(l) = explicit {
            return Ok(l.to_string());
        }
        self.loop_stack
            .last()
            .map(|(s, _)| s.clone())
            .ok_or_else(|| EngineError::invariant(24, "CONTINUE outside loop"))
    }

    /// OUT/INOUT 实参的写回目标（宽松版）：非变量/记录字段实参 → None（值传递）。
    pub(crate) fn assign_target_lenient(&mut self, e: &Expr) -> Option<AssignTarget> {
        self.assign_target(e).ok()
    }

    pub(crate) fn assign_target(&mut self, e: &Expr) -> Result<AssignTarget, EngineError> {
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
}
