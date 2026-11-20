//! 动态 SQL 统一管线与 SQL 上下文例程下沉（自 `interp.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::error::EngineError;
use crate::exec::BindMap;
use crate::runtime::interp::binds::resolve_binds;
use crate::runtime::interp::Env;
use crate::value::SpinValue;

/// 动态 SQL 统一管线（决策 2025-11-05 收口）：文本求值 → ogsql 解析校验 →
/// **静态语句同一条改写管线**（`ir::rewrite_sql`：`[patterns]` → 调用点下沉 →
/// 规则改名 → 方言变换）→ 物化缓存。EXECUTE IMMEDIATE 与 OPEN FOR 共用；
/// 解析失败显式报错（BIZ-061），不执行未解析文本。
pub(super) fn prepare_dynamic_sql(
    text: &str,
    env: &mut Env<'_>,
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
        env.cat.sql_rules(),
        &env.cat.routine_names(),
    )?;
    let sites = unit.fn_sites;
    materialize_fn_sites(&sites, env)?;
    Ok((unit.text, sites))
}

/// 单站物化产物（阶段一）：distinct 元组查询 + 逐元组例程调用的字面量行。
struct SiteRows {
    table: String,
    arity: usize,
    items: Vec<Vec<String>>,
}

/// 阶段一：解析绑定 → distinct 元组查询 → 逐元组 `call_routine_value`，
/// 结果只收集为字面量行（不建表）。例程求值可触发异常处理器
/// reset_session→ROLLBACK，事务性回退会撤销已发出的 CREATE OR REPLACE
/// TEMP（目录 DDL 参与事务），故全部站点求值完成后才统一建表（阶段二）。
fn eval_site(
    table: &str,
    name: &str,
    distinct_sql: &str,
    binds_src: &[(String, crate::ir::BindSrc)],
    arity: usize,
    env: &mut Env<'_>,
) -> Result<SiteRows, EngineError> {
    let max = crate::sql::sqlfn::MAX_TUPLES;
    let binds = resolve_binds(binds_src, env.frame)?;
    let mut st = env.ex.query(distinct_sql, &binds)?;
    let rows = st.fetch_many(max + 1);
    if rows.len() > max {
        return Err(EngineError::business(
            157,
            format!("例程 {name} 的 SQL 上下文参数元组超过上界 {max}"),
        ));
    }
    let mut items_all = Vec::with_capacity(rows.len());
    for row in &rows {
        let v = crate::runtime::interp::routine::call_routine_value(env, name, row)?;
        let mut items = vec![sql_literal(&v)];
        items.extend(row.iter().map(sql_literal));
        items_all.push(items);
    }
    Ok(SiteRows {
        table: table.to_string(),
        arity,
        items: items_all,
    })
}

/// 阶段二：统一 CREATE OR REPLACE TEMP + 分块 INSERT（求值全成功后执行，
/// 回退无可撤销对象）。列名用 sd_ 保留前缀（与 plan::cache_subquery 同口径）。
fn write_site(site: &SiteRows, ex: &dyn crate::exec::SqlExecutor) -> Result<(), EngineError> {
    let mut cols = vec!["sd_v VARCHAR".to_string()];
    for j in 0..site.arity {
        cols.push(format!("sd_a{j} VARCHAR"));
    }
    ex.execute(
        &format!(
            "CREATE OR REPLACE TEMP TABLE {} ({})",
            site.table,
            cols.join(", ")
        ),
        &BindMap::new(),
    )?;
    for chunk in site.items.chunks(500) {
        let tuples: Vec<String> = chunk
            .iter()
            .map(|items| format!("({})", items.join(", ")))
            .collect();
        ex.execute(
            &format!("INSERT INTO {} VALUES {}", site.table, tuples.join(", ")),
            &BindMap::new(),
        )?;
    }
    Ok(())
}

/// SQL 上下文例程调用下沉物化（§8.3 路径②）。
///
/// 语句文本中的调用点已在编译期替换为参数缓存子查询；此处在执行前逐元组
/// 调用例程、填充缓存表（拓扑序保证依赖表先建）。
pub(super) fn materialize_fn_sites(
    sites: &[crate::sql::FnSite],
    env: &mut Env<'_>,
) -> Result<(), EngineError> {
    let evaled = sites
        .iter()
        .map(|site| {
            eval_site(
                &site.table,
                &site.name,
                &site.distinct_sql,
                &site.binds,
                site.args.len(),
                env,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    for site in &evaled {
        write_site(site, env.ex)?;
    }
    Ok(())
}

/// 集合化区域的 fn_site 物化前导（`ConversionPlan::fn_preludes`）：
/// 与解释器 OpenCursor 时点同口径的两阶段物化——全部 distinct 查询与
/// 例程求值先行，全部成功后才统一建缓存表；任一失败直接上抛
/// EngineError（与解释路径同点失败，绝不部分物化后续跑批写）。
pub(crate) fn materialize_fn_preludes(
    preludes: &[crate::ir::set_oriented::FnPrelude],
    frame: &mut crate::runtime::Frame,
    ex: &dyn crate::exec::SqlExecutor,
    sink: &mut dyn crate::exec::TraceSink,
    cat: &crate::catalog::RoutineCatalog,
    var_types: &std::collections::BTreeMap<String, crate::types::SpinType>,
) -> Result<(), EngineError> {
    let mut env = Env {
        frame,
        ex,
        sink,
        cat,
        depth: 0,
        budget: super::DEFAULT_STEP_BUDGET,
        batch: false,
        wbuf: None,
        current_error: None,
        last_write_rows: 0,
        var_types,
    };
    let evaled = preludes
        .iter()
        .map(|p| {
            eval_site(
                &p.table,
                &p.name,
                &p.distinct_sql,
                &p.binds,
                p.arity,
                &mut env,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    for site in &evaled {
        write_site(site, ex)?;
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
