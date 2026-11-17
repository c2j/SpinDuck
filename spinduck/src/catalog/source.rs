//! 例程源的静态目录编译，不构造或调用 SQL 执行器。

use super::{CompiledRoutine, RoutineCatalog, RoutineKind};
use crate::error::EngineError;
use crate::ir::{compile_routine_with_names, IrProg};
use crate::rules::RuleRegistry;
use ogsql_parser::ast::{PackageItem, Statement};
use ogsql_parser::token::tokenizer::Tokenizer;
use std::collections::BTreeSet;

/// 可移交运行期的静态例程目录。
pub struct CompiledCatalog {
    /// 已编译的例程及规则。
    pub catalog: RoutineCatalog,
    pub(crate) macro_sql: Vec<String>,
    pub(crate) macro_names: BTreeSet<String>,
}

/// 按源顺序编译独立例程及包体，SQL 执行不属于此阶段。
pub fn compile_sources(
    sources: &[&str],
    rules: RuleRegistry,
) -> Result<CompiledCatalog, EngineError> {
    let mut catalog = RoutineCatalog::new();
    catalog.set_sql_rules(rules.clone());
    let mut macro_sql = Vec::new();
    let mut macro_names = BTreeSet::new();
    for text in sources {
        let tokens = Tokenizer::new(text)
            .tokenize()
            .map_err(|e| EngineError::invariant(63, format!("tokenizer: {e}")))?;
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        let mut count = 0;
        for stmt in &stmts {
            match stmt {
                Statement::CreatePackageBody(body) => {
                    let pkg = body
                        .name
                        .iter()
                        .map(|id| id.value.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    count += register_package_items(&pkg, &body.items, &rules, &mut catalog)?;
                }
                Statement::CreateProcedure(_) | Statement::CreateFunction(_) => {
                    let names = catalog
                        .routine_names()
                        .difference(&macro_names)
                        .cloned()
                        .collect();
                    for r in super::compile_statement_routine_with_names(stmt, &rules, &names)? {
                        if let Some(sql) =
                            super::sql_macro::render(&r, &rules, &catalog, &macro_names)
                        {
                            macro_sql.push(sql);
                            macro_names.insert(r.name.to_ascii_lowercase());
                        }
                        catalog.register(r);
                        count += 1;
                    }
                }
                // 包规格与非例程 DDL 不生成可调用目录项。
                _ => {}
            }
        }
        check_registration(text, &stmts, count)?;
    }
    Ok(CompiledCatalog {
        catalog,
        macro_sql,
        macro_names,
    })
}

fn register_package_items(
    pkg: &str,
    items: &[PackageItem],
    rules: &RuleRegistry,
    catalog: &mut RoutineCatalog,
) -> Result<usize, EngineError> {
    let mut count = 0;
    for item in items {
        if let Some(routine) = compile_package_item(pkg, item, rules, &catalog.routine_names())? {
            catalog.register(routine);
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn check_registration(
    text: &str,
    stmts: &[Statement],
    count: usize,
) -> Result<(), EngineError> {
    let up = text.to_ascii_uppercase();
    if count == 0
        && up.contains("CREATE")
        && ["PROCEDURE", "FUNCTION", "PACKAGE"]
            .iter()
            .any(|k| up.contains(k))
        && !stmts
            .iter()
            .any(|s| matches!(s, Statement::CreatePackage(_)))
    {
        let head: String = text.trim().chars().take(120).collect();
        return Err(EngineError::invariant(
            64,
            format!(
            "声明了例程却未登记任何例程（解析器降级/静默丢弃，常见原因：体内构造不支持）：{head}"
        ),
        ));
    }
    Ok(())
}

pub(crate) fn compile_package_item(
    pkg: &str,
    item: &PackageItem,
    rules: &RuleRegistry,
    names: &BTreeSet<String>,
) -> Result<Option<CompiledRoutine>, EngineError> {
    let (name, parameters, block, kind) = match item {
        PackageItem::Procedure(p) => (&p.name, &p.parameters, &p.block, RoutineKind::Procedure),
        PackageItem::Function(f) => (&f.name, &f.parameters, &f.block, RoutineKind::Function),
        // 包变量、游标和类型声明不构成可调用例程。
        _ => return Ok(None),
    };
    let extra: Vec<String> = parameters
        .iter()
        .map(|p| p.name.to_ascii_lowercase())
        .collect();
    let mut prog = match block {
        Some(block) => compile_routine_with_names(block, &extra, rules, names)?,
        None => IrProg::default(),
    };
    let params = parameters
        .iter()
        .map(|p| super::map_param(p, rules))
        .collect::<Result<Vec<_>, _>>()?;
    super::remember_param_types(&mut prog, &params);
    Ok(Some(CompiledRoutine {
        name: format!(
            "{}.{}",
            pkg.to_ascii_lowercase(),
            name.join(".").to_ascii_lowercase()
        ),
        kind,
        params,
        prog,
    }))
}
