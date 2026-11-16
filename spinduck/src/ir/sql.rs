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
        // Gate 2A：Oracle 系批量取数/RETURNING INTO 等构造无对应 DuckDB 语义，
        // 显式拒绝而非静默改写（`INTO <table>` 对 SELECT 由 SqlQuery.into 承载，
        // 仅 bulk_collect/into_table 拒绝）。
        fn reject_pl_fetch_flags(
            bulk_collect: bool,
            into_table: bool,
            returning: bool,
            into_targets: bool,
        ) -> Result<(), EngineError> {
            if bulk_collect || into_table || returning || into_targets {
                return Err(EngineError::invariant(
                    71,
                    "BULK COLLECT / RETURNING INTO / INTO TABLE is not supported",
                ));
            }
            Ok(())
        }
        match statement {
            S::Select(sel) => {
                // 仅拒绝显式 BULK COLLECT；`INTO` 在非 PL 上下文会被解析器
                // 歧义归为 into_table（PG SELECT INTO 语义），属存量行为不拦截
                reject_pl_fetch_flags(sel.node.bulk_collect, false, false, false)?;
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
                let into = self.compile_into_targets(into_targets.unwrap_or_default())?;
                if unit.fn_sites.is_empty()
                    && unit.binds.is_empty()
                    && super::dummy::emit(self, &sel.node, &into)
                {
                    return Ok(());
                }
                let (sql, binds, fn_sites) = (unit.text, unit.binds, unit.fn_sites);
                self.code.push(IrStmt::SqlQuery {
                    sql,
                    binds,
                    into,
                    fn_sites,
                });
            }
            S::Insert(_) | S::Update(_) | S::Delete(_) | S::Truncate(_) => {
                // RETURNING / RETURNING INTO / BULK COLLECT：DuckDB 无对应语义，
                // 由 Gate 2B 的 prepare 兜底前在此显式拒绝
                let (bulk, returning) = match statement {
                    S::Insert(i) => (
                        i.node.bulk_collect,
                        !i.node.returning.is_empty() || i.node.into_targets.is_some(),
                    ),
                    S::Update(u) => (
                        u.node.bulk_collect,
                        !u.node.returning.is_empty() || u.node.into_targets.is_some(),
                    ),
                    S::Delete(d) => (
                        d.node.bulk_collect,
                        !d.node.returning.is_empty() || d.node.into_targets.is_some(),
                    ),
                    _ => (false, false),
                };
                reject_pl_fetch_flags(bulk, false, returning, false)?;
                let unit = rewrite_sql(sql_text, &self.vars, &self.rules, &self.routine_names)?;
                let (sql, binds, fn_sites) = (unit.text, unit.binds, unit.fn_sites);
                self.code.push(IrStmt::SqlExec {
                    sql,
                    binds,
                    fn_sites,
                });
            }
            // Gate 2A：MERGE INTO 无 DuckDB 1.5.5 对应语义，显式 ENG-0071
            // （顶层 Statement::Merge 已由 gate/mod.rs 拦截，此处覆盖例程体内嵌入形态）
            S::Merge(_) => {
                return Err(EngineError::invariant(71, "MERGE INTO is not supported"));
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

    fn compile_into_targets(
        &mut self,
        targets: Vec<ogsql_parser::ast::SelectTarget>,
    ) -> Result<Vec<AssignTarget>, EngineError> {
        targets
            .into_iter()
            .map(|target| match target {
                ogsql_parser::ast::SelectTarget::Expr(expr, _) => self.assign_target(&expr),
                ogsql_parser::ast::SelectTarget::Star(_) => {
                    Err(EngineError::invariant(42, "SELECT * INTO is not supported"))
                }
            })
            .collect()
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
