//! 动态 SQL 统一管线与 SQL 上下文例程下沉（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor};
use crate::runtime::frame::Frame;
use crate::runtime::interp::binds::resolve_binds;
use crate::value::SpinValue;

/// 动态 SQL 统一管线（决策 2025-11-05 收口）：文本求值 → ogsql 解析校验 →
/// **静态语句同一条改写管线**（`ir::rewrite_sql`：`[patterns]` → 调用点下沉 →
/// 规则改名 → 方言变换）→ 物化缓存。EXECUTE IMMEDIATE 与 OPEN FOR 共用；
/// 解析失败显式报错（BIZ-061），不执行未解析文本。
pub(super) fn prepare_dynamic_sql(
    text: &str,
    frame: &Frame,
    ex: &dyn SqlExecutor,
    cat: &crate::catalog::RoutineCatalog,
    depth: usize,
) -> Result<(String, Vec<crate::sql::FnSite>), EngineError> {
    // 带错误通道解析（与静态语句同口径；不再静默吞解析失败）
    let (infos, errors) = ogsql_parser::parser::Parser::parse_sql(text);
    if !errors.is_empty() || infos.is_empty() {
        let head: String = text.chars().take(80).collect();
        return Err(EngineError::business(
            61,
            format!("动态 SQL 解析失败（ogsql-parser 不支持的形态请向其上游提 issue）：{head}"),
        ));
    }
    let fmt = ogsql_parser::formatter::SqlFormatter::new();
    let sql = infos
        .iter()
        .map(|si| fmt.format_statement(&si.statement))
        .collect::<Vec<String>>()
        .join(";\n");
    // 动态文本不含 PL 变量（USING v1 不支持，变量须先拼接为字面量）
    // → 空 VarScope 是**口径**而非缺省
    let unit = crate::ir::rewrite_sql(
        &sql,
        &Default::default(),
        cat.sql_rules(),
        &cat.routine_names(),
    )?;
    let sites = unit.fn_sites;
    materialize_fn_sites(&sites, frame, ex, cat, depth)?;
    Ok((unit.text, sites))
}

/// SQL 上下文例程调用下沉物化（§8.3 路径②）。
///
/// 语句文本中的调用点已在编译期替换为参数缓存子查询；此处在执行前逐元组
/// 调用例程、填充缓存表（拓扑序保证依赖表先建）。
pub(super) fn materialize_fn_sites(
    sites: &[crate::sql::FnSite],
    frame: &Frame,
    ex: &dyn SqlExecutor,
    cat: &crate::catalog::RoutineCatalog,
    depth: usize,
) -> Result<(), EngineError> {
    let max = crate::sql::sqlfn::MAX_TUPLES;
    for site in sites {
        let binds = resolve_binds(&site.binds, frame)?;
        let mut st = ex.query(&site.distinct_sql, &binds)?;
        let rows = st.fetch_many(max + 1);
        if rows.len() > max {
            return Err(EngineError::business(
                157,
                format!("例程 {} 的 SQL 上下文参数元组超过上界 {max}", site.name),
            ));
        }
        let arity = site.args.len();
        let mut cols = vec!["v VARCHAR".to_string()];
        for j in 0..arity {
            cols.push(format!("a{j} VARCHAR"));
        }
        ex.execute(
            &format!(
                "CREATE OR REPLACE TEMP TABLE {} ({})",
                site.table,
                cols.join(", ")
            ),
            &BindMap::new(),
        )?;
        for chunk in rows.chunks(500) {
            let mut tuples = Vec::with_capacity(chunk.len());
            for row in chunk {
                let v = crate::runtime::interp::routine::call_routine_value(
                    cat, &site.name, row, ex, depth,
                )?;
                let mut items = vec![sql_literal(&v)];
                for cell in row {
                    items.push(sql_literal(cell));
                }
                tuples.push(format!("({})", items.join(", ")));
            }
            ex.execute(
                &format!("INSERT INTO {} VALUES {}", site.table, tuples.join(", ")),
                &BindMap::new(),
            )?;
        }
    }
    Ok(())
}

/// SpinValue → SQL 字面量（文本型转义；数值/布尔按文本直出）。
pub(super) fn sql_literal(v: &SpinValue) -> String {
    match v {
        SpinValue::Null => "NULL".to_string(),
        SpinValue::Str(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.as_str_loose(),
    }
}
