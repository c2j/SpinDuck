//! 编译入口与 SQL 改写管线（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::ir::*;
use crate::rules::RuleRegistry;
use crate::value::SpinValue;
use ogsql_parser::ast::plpgsql::{PlBlock, PlStatement};
use ogsql_parser::ast::Expr;
use std::collections::BTreeSet;

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
        rules: rules.clone(),
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
    compile_routine_with_names(block, extra_vars, rules, &BTreeSet::new())
}

/// 例程块编译（带已注册例程名集）：SQL 上下文例程调用下沉判定依赖它。
pub fn compile_routine_with_names(
    block: &PlBlock,
    extra_vars: &[String],
    rules: &RuleRegistry,
    routine_names: &BTreeSet<String>,
) -> Result<IrProg, EngineError> {
    let mut vars = collect_var_names(&block.body);
    for v in extra_vars {
        vars.insert(v.to_ascii_lowercase());
    }
    let mut c = Compiler {
        vars,
        rules: rules.clone(),
        routine_names: routine_names.clone(),
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
                let unit = rewrite_sql(&query_text, &c.vars, &c.rules, &c.routine_names)?;
                c.pending_cursor_sql
                    .insert(name, (unit.text, unit.binds, unit.fn_sites));
            }
            PlDeclaration::Variable(v) => {
                let name = VarName(v.name.to_ascii_lowercase());
                c.vars.insert(name.0.clone());
                // `%ROWTYPE`：记录锚定游标 → 单目标 FETCH INTO 按整行记录赋值
                if let ogsql_parser::ast::plpgsql::PlDataType::PercentRowType(anchor) = &v.data_type
                {
                    c.rowtype_of
                        .insert(name.0.clone(), anchor.to_ascii_lowercase());
                }
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
    // 例程级 EXCEPTION 段（语料主流形态：`IS BEGIN … EXCEPTION WHEN OTHERS … END;`）
    // 与块级走同一 trap 编译；此前 PlBlock.exception_block 被静默丢弃。
    match &block.exception_block {
        Some(exc) => c.trap_section(&block.body, exc)?,
        None => c.stmts(&block.body)?,
    }
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
        PlStatement::For(f) => {
            // FOR 循环变量（Range 计数 / Query·Cursor 记录）本身是帧变量，
            // 其记录字段（rec.field）会出现在体内 SQL 文本中，必须入集
            use ogsql_parser::ast::plpgsql::PlForKind;
            set.insert(f.variable.to_ascii_lowercase());
            if let PlForKind::Query { parsed_query, .. } = &f.kind {
                // 记录形态：无独立表达式可走，体内语句递归即可
                let _ = parsed_query;
            }
            collect_slice(&f.body, set);
        }
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

/// 统一 SQL 改写管线（静态语句编译与动态 SQL 准备共用，§6 单一实现点）：
/// `[patterns]`（rownum 首行 / UPDATE 去别名）→ 调用点下沉规划 → token 规则改名
/// → AST 级变换。动态路径以空 VarScope 调用（USING v1 不支持，PL 变量须先
/// 拼接为字面量），其余口径与静态一致。
pub fn rewrite_sql(
    sql_text: &str,
    vars: &crate::sql::VarScope,
    rules: &RuleRegistry,
    routine_names: &BTreeSet<String>,
) -> Result<crate::sql::SqlUnit, EngineError> {
    // UPDATE 目标别名去限定 / rownum 首行 → LIMIT 1（唯一实现见 sql::patterns）
    let text = crate::sql::apply_text_patterns(sql_text, rules)?;
    let sql_text = text.as_str();
    // SQL 上下文例程调用下沉：先规划调用点（原始名文本），再统一改写。
    let (marked, fn_sites) =
        crate::sql::plan_sites(sql_text, vars, &rules.functions, routine_names)?;
    let unit = crate::sql::rewrite_tokens_with_rules(&marked, vars, &rules.functions)?;
    let text = crate::sql::apply_ast_transforms(&unit.text)?;
    let mut binds = unit.binds;
    for s in &fn_sites {
        for (ph, b) in &s.binds {
            if !binds.iter().any(|(p, _)| p == ph) {
                binds.push((ph.clone(), b.clone()));
            }
        }
    }
    Ok(crate::sql::SqlUnit {
        text,
        binds,
        fn_sites,
    })
}
