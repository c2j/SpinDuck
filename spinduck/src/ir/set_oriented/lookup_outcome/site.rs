//! 回退区域 `SELECT INTO` 站点的计划期证据分析（issue #65）。
//!
//! 复用守卫 `key_checks` 同口径的证据：等值合取项的列必须与游标
//! 字段类型严格一致（`schema::load` 双端 kind 比对），残余合取项只
//! 允许不含副作用的静态/不变量谓词。任何一步不可证即放弃该站点，
//! 解释器走原路径——缓存是纯加速层，不改变可证面。

use super::super::schema::{self, Table};
use super::super::shape::{self, Name, Select};
use super::admission;
use super::residual::{self, ResidualCtx, ResidualOk};
use super::SiteSpec;
use crate::exec::SqlExecutor;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{AssignTarget, BindSrc, CursorSource, IrProg, IrStmt, VarName};
use std::collections::{BTreeSet, HashMap, HashSet};

/// 单站点分析的共享上下文（区域写集/游标形态/表结构缓存）。
struct SiteCtx<'a> {
    region: &'a CursorRegion,
    cursor: Option<&'a Select>,
    written_vars: &'a BTreeSet<VarName>,
    written_fields: &'a BTreeSet<(VarName, String)>,
    ex: &'a dyn SqlExecutor,
    tables: HashMap<String, Option<Table>>,
}

/// 区域内全部候选站点的预计算规格；无可证站点返回空表。
/// `routine` 用于站点身份（例程名 + 出现 PC），仅作诊断标记。
pub(super) fn collect(
    prog: &IrProg,
    region: &CursorRegion,
    ex: &dyn SqlExecutor,
    routine: &str,
) -> Vec<SiteSpec> {
    // 同 SQL 不同绑定表的站点无法共享索引键空间，整句禁用缓存。
    type Seen = HashMap<String, (Vec<(String, BindSrc)>, Vec<usize>)>;
    let mut seen: Seen = HashMap::new();
    let mut conflict: HashSet<String> = HashSet::new();
    for (i, stmt) in prog.code[region.open_pc..=region.close_pc]
        .iter()
        .enumerate()
    {
        let IrStmt::SqlQuery {
            sql,
            binds,
            into,
            fn_sites,
        } = stmt
        else {
            continue;
        };
        if !fn_sites.is_empty() || into.is_empty() {
            continue;
        }
        match seen.get_mut(sql) {
            None => {
                seen.insert(sql.clone(), (binds.clone(), vec![region.open_pc + i]));
            }
            Some((prev, pcs)) => {
                if *prev != *binds {
                    conflict.insert(sql.clone());
                } else {
                    pcs.push(region.open_pc + i);
                }
            }
        }
    }
    let mut specs = Vec::new();
    for (sql, (binds, pcs)) in seen {
        if conflict.contains(&sql) {
            continue;
        }
        // `lookup_cache` 已接管的形态让位（A 面行为逐字不变）。
        if crate::lookup_cache::shape::recognize(&sql).is_some() {
            continue;
        }
        let pcs = pcs
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("+");
        let scope = format!("{routine}@{pcs}");
        if let Some(spec) = analyze_one(prog, region, &sql, &binds, ex, &scope) {
            specs.push(spec);
        }
    }
    specs
}

/// 单站点重证入口（写失效后按当前 schema 重新走完整证据链；
/// `collect` 与 `cache` 的懒重建共用，保证两条路径口径一致）。
pub(super) fn analyze_one(
    prog: &IrProg,
    region: &CursorRegion,
    sql: &str,
    binds: &[(String, BindSrc)],
    ex: &dyn SqlExecutor,
    scope: &str,
) -> Option<SiteSpec> {
    let (written_vars, written_fields) = region_writes(prog, region);
    let cursor = cursor_select(prog, region);
    let mut ctx = SiteCtx {
        region,
        cursor: cursor.as_ref(),
        written_vars: &written_vars,
        written_fields: &written_fields,
        ex,
        tables: HashMap::new(),
    };
    let mut spec = analyze(sql, binds, &mut ctx)?;
    spec.scope = scope.to_string();
    Some(spec)
}

/// 区域写集：变量与记录字段两路（循环不变证据的反面）。
fn region_writes(
    prog: &IrProg,
    region: &CursorRegion,
) -> (BTreeSet<VarName>, BTreeSet<(VarName, String)>) {
    let mut vars = BTreeSet::new();
    let mut fields = BTreeSet::new();
    fn put(
        t: &AssignTarget,
        vars: &mut BTreeSet<VarName>,
        fields: &mut BTreeSet<(VarName, String)>,
    ) {
        match t {
            AssignTarget::Var(v) => {
                vars.insert(v.clone());
            }
            AssignTarget::Field { rec, field } => {
                fields.insert((rec.clone(), field.to_ascii_lowercase()));
            }
        }
    }
    for stmt in &prog.code[region.open_pc..=region.close_pc] {
        match stmt {
            IrStmt::Assign { target, .. } | IrStmt::FoldedQuery { target, .. } => {
                put(target, &mut vars, &mut fields);
            }
            IrStmt::Fetch { targets, .. }
            | IrStmt::SqlQuery { into: targets, .. }
            | IrStmt::SqlExecDynamic {
                into_targets: targets,
                ..
            } => targets.iter().for_each(|t| put(t, &mut vars, &mut fields)),
            IrStmt::CallRoutine { out_targets, .. } => out_targets
                .iter()
                .flatten()
                .for_each(|t| put(t, &mut vars, &mut fields)),
            IrStmt::OpenCursor { cursor, .. } => {
                vars.insert(cursor.clone());
            }
            _ => {}
        }
    }
    (vars, fields)
}

/// 游标声明的静态 SELECT 形态（verbatim 允许 ORDER BY 等尾部子句）。
fn cursor_select(prog: &IrProg, region: &CursorRegion) -> Option<Select> {
    let IrStmt::OpenCursor {
        source: CursorSource::Static { sql, .. },
        ..
    } = prog.code.get(region.open_pc)?
    else {
        return None;
    };
    shape::select(sql)
        .or_else(|_| shape::select_verbatim(sql))
        .ok()
}

/// 合取项分类产物：等值键 / 键字段 / 残余文本 / 类型复核项。
#[derive(Default)]
struct Conjuncts {
    keys: Vec<(Name, String)>,
    key_fields: Vec<(VarName, String)>,
    residual: Vec<String>,
    typed: ResidualOk,
}

/// 单站点证据链：形态 → 合取项分类 → 类型对齐 → 聚合 SQL 生成。
fn analyze(sql: &str, binds: &[(String, BindSrc)], ctx: &mut SiteCtx<'_>) -> Option<SiteSpec> {
    // R4-2：shape::select 不识别 DISTINCT/ALL 修饰符（`SELECT DISTINCT k`
    // 会被误解析为列 `distinct`+别名 `k`），同名引号列可绕过投影
    // 证明——按 token 显式拒绝，整站禁用。
    if admission::select_has_modifier(sql) {
        return None;
    }
    let sel = shape::select(sql).ok()?;
    if sel.filter.as_deref().is_some_and(shape::filter_has_tail) {
        return None;
    }
    let bind_map: HashMap<&str, &BindSrc> = binds.iter().map(|(p, s)| (p.as_str(), s)).collect();
    // 结构证据先行：残余谓词的列 kind 相容证明与键类型对齐共用。
    let dim_table = load_table(ctx.ex, &mut ctx.tables, &sel.table)?;
    let cursor = ctx.cursor?;
    let cursor_table = load_table(ctx.ex, &mut ctx.tables, &cursor.table)?;
    // R4-1：非二元排序规则下 GROUP BY 键折叠与探测 map 精确匹配
    // 口径不一致（'a' vs 'A'）——会话 default_collation 与列级
    // COLLATE 任一不可证即禁用。
    if !admission::collation_safe(ctx.ex, &[&sel.table, &cursor.table]) {
        return None;
    }
    let rctx = ResidualCtx {
        bind_map: &bind_map,
        written_vars: ctx.written_vars,
        dim: &dim_table,
        sel: &sel,
    };
    let mut conj = Conjuncts::default();
    classify_conjuncts(&sel, ctx.region, &rctx, &mut conj)?;
    // 键字段在区域内被显式改写 → 逐键结果不可证。
    if conj
        .key_fields
        .iter()
        .any(|(rec, field)| ctx.written_fields.contains(&(rec.clone(), field.clone())))
    {
        return None;
    }
    let kctx = KeyCtx {
        cursor,
        cursor_table: &cursor_table,
        dim_table: &dim_table,
    };
    check_key_types(&conj.keys, &bind_map, &kctx, &mut conj.typed)?;
    // 投影源列必须存在于维度表：形态误解析（如 DISTINCT）在此拦截，
    // 避免构建期绑定错误徒增一次失败查询。
    for src in &sel.sources {
        column_kind(&dim_table, src)?;
    }
    let mut spec = build_spec(sql, &sel, &conj.keys, &conj.residual, &conj.typed.inv_phs);
    spec.binds = binds.to_vec();
    spec.typed_binds = conj.typed.typed_binds;
    spec.watch = watch_tables(&sel, cursor);
    Some(spec)
}

/// WHERE 合取项逐项分类：等值键归键，其余过残余全定义谓词证明。
fn classify_conjuncts(
    sel: &Select,
    region: &CursorRegion,
    rctx: &ResidualCtx<'_>,
    out: &mut Conjuncts,
) -> Option<()> {
    for conj in admission::split_conjuncts(sel.filter.as_deref().unwrap_or("")) {
        match admission::key_conjunct(&conj, rctx.bind_map, region, sel) {
            Some((col, ph, rec, field)) => {
                out.keys.push((col, ph.clone()));
                out.key_fields.push((rec, field));
            }
            // 残余合取项必须过全定义谓词证明（B1/R1：不可证 total
            // 的形态——算术/跨类型比较/函数/子查询——整站禁用）。
            None => {
                residual::check(&conj, rctx, &mut out.typed)?;
                out.residual.push(conj.join(" "));
            }
        }
    }
    Some(())
}

/// 键类型对齐证据（守卫 key_checks 同口径 + 审核加固）：键列 kind
/// == 游标字段**实际输出**源列 kind；浮点键列有损舍入（B4）禁用；
/// verbatim 表达式键输出类型不可证（R2）禁用；键绑定值构建/探测
/// 期按 kind 复核。
struct KeyCtx<'a> {
    cursor: &'a Select,
    cursor_table: &'a Table,
    dim_table: &'a Table,
}

fn check_key_types(
    keys: &[(Name, String)],
    bind_map: &HashMap<&str, &BindSrc>,
    kctx: &KeyCtx<'_>,
    typed: &mut ResidualOk,
) -> Option<()> {
    for (col, ph) in keys {
        let field = match bind_map.get(ph.as_str()) {
            Some(BindSrc::Field { field, .. }) => field.to_ascii_lowercase(),
            _ => return None,
        };
        let dim_kind = column_kind(kctx.dim_table, col)?;
        if matches!(
            dim_kind.as_str(),
            "double" | "float" | "real" | "float4" | "float8"
        ) {
            return None;
        }
        let src = super::verbatim::cursor_key_source(kctx.cursor, &Name(field))?;
        let cur_kind = column_kind(kctx.cursor_table, &src)?;
        if dim_kind != cur_kind {
            return None;
        }
        typed.typed_binds.push((ph.clone(), dim_kind));
    }
    Some(())
}

/// 裸标识符（与 `Tokens::name` 同规则：字母/下划线开头，字母数字下划线续）。
pub(super) fn is_ident(w: &str) -> bool {
    let mut chars = w.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 失效监视表集合：维度表 + 游标源表（键类型证明依赖两端 schema，
/// 任一被写/DDL 都必须重证）。
fn watch_tables(sel: &Select, cursor: &Select) -> Vec<String> {
    let mut out = vec![sel.table.0.to_ascii_lowercase()];
    let cur = cursor.table.0.to_ascii_lowercase();
    if !out.contains(&cur) {
        out.push(cur);
    }
    out
}

fn load_table(
    ex: &dyn SqlExecutor,
    tables: &mut HashMap<String, Option<Table>>,
    name: &Name,
) -> Option<Table> {
    let key = name.0.to_ascii_lowercase();
    if !tables.contains_key(&key) {
        let loaded = schema::load(ex, name).ok().flatten();
        tables.insert(key.clone(), loaded);
    }
    tables.get(&key).cloned().flatten()
}

fn column_kind(table: &Table, col: &Name) -> Option<String> {
    table
        .columns
        .iter()
        .find(|c| c.name == *col)
        .map(|c| c.kind.clone())
}

/// 组装预计算规格：`GROUP BY` 键列 + 计数 + 单行投影折叠。
fn build_spec(
    sql: &str,
    sel: &Select,
    keys: &[(Name, String)],
    residual: &[String],
    inv_phs: &[String],
) -> SiteSpec {
    let mut cols = Vec::new();
    for (col, _) in keys {
        cols.push(col.sql());
    }
    cols.push("COUNT(*) AS \"__sd_cnt\"".to_string());
    for (i, src) in sel.sources.iter().enumerate() {
        cols.push(format!(
            "CASE WHEN COUNT(*) = 1 THEN MAX({}) END AS \"__sd_v{}\"",
            src.sql(),
            i
        ));
    }
    // 别名必须保留：残余/键合取项的 `t.col` 限定符依赖它解析（P1）。
    let from = match &sel.alias {
        Some(a) => format!("{} AS {}", sel.table.sql(), a.sql()),
        None => sel.table.sql(),
    };
    let mut build = format!("SELECT {} FROM {}", cols.join(", "), from);
    if !residual.is_empty() {
        build.push_str(&format!(" WHERE {}", residual.join(" AND ")));
    }
    if !keys.is_empty() {
        let dims = keys.iter().map(|(c, _)| c.sql()).collect::<Vec<_>>();
        build.push_str(&format!(" GROUP BY {}", dims.join(", ")));
    }
    // R4-3：`inner.query` 全量物化结果——LIMIT MAX_KEYS+1 是超限
    // 哨兵，把物化上界下推到 SQL 层（distinct 键 >200k → Disabled）。
    build.push_str(&format!(" LIMIT {}", super::MAX_KEYS + 1));
    SiteSpec {
        sql: sql.to_string(),
        binds: Vec::new(),
        watch: Vec::new(),
        key_phs: keys.iter().map(|(_, ph)| ph.clone()).collect(),
        build_sql: build,
        inv_phs: inv_phs.to_vec(),
        typed_binds: Vec::new(),
        schema: sel.columns.iter().map(|c| c.0.clone()).collect(),
        scope: String::new(),
    }
}
