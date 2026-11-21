//! SQL 类语句与赋值目标/循环标签（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use ogsql_parser::ast::Expr;

/// 限定对象名 → DuckDB 引号形态（`s.t` → `"s"."t"`）；
/// unquoted 段归一小写，与 `gate::schema::object_key` 同口径。
fn quote_object_name(name: &ogsql_parser::ast::ObjectName) -> String {
    name.iter()
        .map(|i| {
            let v = if i.quote_style.is_some() {
                i.value.clone()
            } else {
                i.value.to_ascii_lowercase()
            };
            format!("\"{}\"", v.replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(".")
}

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
            // MERGE INTO：DuckDB ≥1.4 支持（内核 1.5.5 覆盖），与 DML 同路径
            // 改写为 SqlExec 交 Gate 2B prepare 校验，不再 Gate 2A 拒绝。
            S::Insert(_) | S::Update(_) | S::Delete(_) | S::Truncate(_) | S::Merge(_) => {
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
                    buffer_policy: BufferPolicy::Immediate,
                });
            }
            // 例程体内局部 DDL：CTAS 原文改写为 SqlExec（DuckDB 原生支持
            // `CREATE [TEMP] TABLE .. AS ..`）；普通 CREATE TABLE 由 AST
            // 重建列清单（类型经 `[types]` 映射）。标记 local_ddl 供 Gate 2B
            // 在 stub 库执行——后续引用获得真实列检查（KERNEL-0011）。
            S::CreateTableAs(t) => {
                let unit = rewrite_sql(sql_text, &self.vars, &self.rules, &self.routine_names)?;
                let idx = self.code.len();
                self.code.push(IrStmt::SqlExec {
                    sql: unit.text,
                    binds: unit.binds,
                    fn_sites: unit.fn_sites,
                    buffer_policy: BufferPolicy::Immediate,
                });
                self.local_ddl.push(LocalDdlStmt {
                    index: idx,
                    ctas: true,
                    name: quote_object_name(&t.name),
                    temporary: t.temporary,
                    if_not_exists: t.if_not_exists,
                });
            }
            S::CreateTable(t) => {
                let sql = self.local_create_table_ddl(t)?;
                let idx = self.code.len();
                self.code.push(IrStmt::SqlExec {
                    sql,
                    binds: Vec::new(),
                    fn_sites: Vec::new(),
                    buffer_policy: BufferPolicy::Immediate,
                });
                self.local_ddl.push(LocalDdlStmt {
                    index: idx,
                    ctas: false,
                    name: quote_object_name(&t.name),
                    temporary: t.temporary,
                    if_not_exists: t.if_not_exists,
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

    /// 例程体内 `CREATE [TEMP] TABLE name (col type, ..)` → DuckDB DDL。
    /// 仅支持纯列清单形态：列约束（NOT NULL/DEFAULT 等）与 LIKE/继承/
    /// 分区/表空间等扩展项维持 `ENG-0043` 显式拒绝，不静默丢语义。
    /// `UNLOGGED` 忽略（DuckDB 无对应概念，临时表本就不落盘）。
    fn local_create_table_ddl(
        &self,
        t: &ogsql_parser::ast::CreateTableStatement,
    ) -> Result<String, EngineError> {
        let unsupported = |what: &str| {
            EngineError::invariant(
                43,
                format!("body CREATE TABLE with {what} is not supported (M8/M9 scope)"),
            )
        };
        if t.columns.is_empty()
            || !t.constraints.is_empty()
            || !t.like_clauses.is_empty()
            || !t.inherits.is_empty()
            || t.partition_by.is_some()
            || t.subpartition_by.is_some()
            || t.subpartitions_count.is_some()
            || t.distribute_by.is_some()
            || t.to_group.is_some()
            || t.tablespace.is_some()
            || t.on_commit.is_some()
            || !t.options.is_empty()
            || !t.table_options.is_empty()
            || t.compress.is_some()
            || t.ilm.is_some()
            || t.row_movement.is_some()
        {
            return Err(unsupported("constraints/partition/like/inherits/options"));
        }
        if t.columns.iter().any(|c| !c.constraints.is_empty()) {
            return Err(unsupported("column constraints"));
        }
        let mut sql = String::from("CREATE ");
        if t.temporary {
            sql.push_str("TEMP ");
        }
        sql.push_str("TABLE ");
        if t.if_not_exists {
            sql.push_str("IF NOT EXISTS ");
        }
        sql.push_str(&quote_object_name(&t.name));
        sql.push('(');
        for (i, c) in t.columns.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push('"');
            sql.push_str(&c.name.to_ascii_lowercase().replace('"', "\"\""));
            sql.push_str("\" ");
            sql.push_str(&crate::types::duckdb_type_name(&c.data_type, &self.rules));
        }
        sql.push(')');
        Ok(sql)
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
