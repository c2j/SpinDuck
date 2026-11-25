//! 回退区域 `SELECT INTO` 站点的计划期证据分析（issue #65）。
//!
//! 复用守卫 `key_checks` 同口径的证据：等值合取项的列必须与游标
//! 字段类型严格一致（`schema::load` 双端 kind 比对），残余合取项只
//! 允许不含副作用的静态/不变量谓词。任何一步不可证即放弃该站点，
//! 解释器走原路径——缓存是纯加速层，不改变可证面。

use super::super::schema::{self, Table};
use super::super::shape::{self, Name, Select};
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

/// 单站点证据链：形态 → 合取项分类 → 类型对齐 → 聚合 SQL 生成。
fn analyze(sql: &str, binds: &[(String, BindSrc)], ctx: &mut SiteCtx<'_>) -> Option<SiteSpec> {
    let sel = shape::select(sql).ok()?;
    if sel.filter.as_deref().is_some_and(shape::filter_has_tail) {
        return None;
    }
    let bind_map: HashMap<&str, &BindSrc> = binds.iter().map(|(p, s)| (p.as_str(), s)).collect();
    // 结构证据先行：残余谓词的列 kind 相容证明与键类型对齐共用。
    let dim_table = load_table(ctx.ex, &mut ctx.tables, &sel.table)?;
    let cursor = ctx.cursor?;
    let cursor_table = load_table(ctx.ex, &mut ctx.tables, &cursor.table)?;
    let rctx = ResidualCtx {
        bind_map: &bind_map,
        written_vars: ctx.written_vars,
        dim: &dim_table,
        sel: &sel,
    };
    let mut keys: Vec<(Name, String)> = Vec::new();
    let mut key_fields: Vec<(VarName, String)> = Vec::new();
    let mut residual: Vec<String> = Vec::new();
    let mut typed = ResidualOk::default();
    for conj in split_conjuncts(sel.filter.as_deref().unwrap_or("")) {
        match key_conjunct(&conj, &bind_map, ctx.region, &sel) {
            Some((col, ph, rec, field)) => {
                keys.push((col, ph.clone()));
                key_fields.push((rec, field));
            }
            None => {
                // 残余合取项必须过全定义谓词证明（B1：不可证 total
                // 的形态——算术/函数/子查询/类型不安全比较——整站禁用）。
                residual::check(&conj, &rctx, &mut typed)?;
                residual.push(conj.join(" "));
            }
        }
    }
    // 键字段在区域内被显式改写 → 逐键结果不可证。
    if key_fields
        .iter()
        .any(|(rec, field)| ctx.written_fields.contains(&(rec.clone(), field.clone())))
    {
        return None;
    }
    // 类型对齐证据：键列 kind == 游标字段源列 kind（守卫 key_checks 同口径）；
    // 浮点键列有损舍入（B4）一律禁用；键绑定值构建期按 kind 复核。
    for (col, ph) in &keys {
        let field = match bind_map.get(ph.as_str()) {
            Some(BindSrc::Field { field, .. }) => field.to_ascii_lowercase(),
            _ => return None,
        };
        let dim_kind = column_kind(&dim_table, col)?;
        if matches!(
            dim_kind.as_str(),
            "double" | "float" | "real" | "float4" | "float8"
        ) {
            return None;
        }
        let src = cursor.source_of(&Name(field.clone()))?;
        let cur_kind = column_kind(&cursor_table, src)?;
        if dim_kind != cur_kind {
            return None;
        }
        typed.typed_binds.push((ph.clone(), dim_kind));
    }
    // 投影源列必须存在于维度表：形态误解析（如 DISTINCT）在此拦截，
    // 避免构建期绑定错误徒增一次失败查询。
    for src in &sel.sources {
        column_kind(&dim_table, src)?;
    }
    let mut spec = build_spec(sql, &sel, &keys, &residual, &typed.inv_phs);
    spec.binds = binds.to_vec();
    spec.typed_binds = typed.typed_binds;
    spec.watch = watch_tables(&sel, cursor);
    Some(spec)
}

/// WHERE 顶层合取项切分：`BETWEEN a AND b` 的 AND 不属于合取边界。
fn split_conjuncts(filter: &str) -> Vec<Vec<String>> {
    let Some(t) = shape::Tokens::new(filter) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut between_open = false;
    for w in t.words() {
        match w.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "between" if depth == 0 => between_open = true,
            "and" if depth == 0 => {
                if between_open {
                    between_open = false;
                } else {
                    out.push(std::mem::take(&mut cur));
                    continue;
                }
            }
            _ => {}
        }
        cur.push(w.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 等值合取项 → (维度列, 占位符, 记录变量, 字段名)；非键形态返回 None。
/// 限定符必须是表名或别名，否则原查询本身不可证，按非键处理（残余侧
/// 会因字段绑定拒绝整站点）。
fn key_conjunct(
    conj: &[String],
    bind_map: &HashMap<&str, &BindSrc>,
    region: &CursorRegion,
    sel: &Select,
) -> Option<(Name, String, VarName, String)> {
    let (col, ph, qual) = match conj {
        [c, eq, p] if eq == "=" => (c, p, None),
        [q, dot, c, eq, p] if dot == "." && eq == "=" => (c, p, Some(q)),
        [p, eq, q, dot, c] if eq == "=" && dot == "." => (c, p, Some(q)),
        _ => return None,
    };
    let (col, ph) = if bind_map.contains_key(ph.as_str()) && is_ident(col) {
        (col, ph)
    } else if bind_map.contains_key(col.as_str()) && is_ident(ph) {
        (ph, col)
    } else {
        return None;
    };
    if let Some(q) = qual {
        let ok = q == &sel.table.0 || sel.alias.as_ref().is_some_and(|a| &a.0 == q);
        if !ok {
            return None;
        }
    }
    match bind_map.get(ph.as_str()) {
        Some(BindSrc::Field { rec, field }) if rec == &region.record => Some((
            Name(col.clone()),
            ph.clone(),
            rec.clone(),
            field.to_ascii_lowercase(),
        )),
        _ => None,
    }
}

/// 裸标识符（与 `Tokens::name` 同规则：字母/下划线开头，字母数字下划线续）。
fn is_ident(w: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        split_conjuncts(s)
            .into_iter()
            .map(|c| c.join(" "))
            .collect()
    }

    #[test]
    fn should_split_top_level_and_but_keep_between_range() {
        assert_eq!(
            words("a = 1 and pri between 1 and 9 and b > 2"),
            vec!["a = 1", "pri between 1 and 9", "b > 2"]
        );
    }

    #[test]
    fn should_keep_or_and_parens_inside_single_conjunct() {
        assert_eq!(
            words("(a = 1 or a = 2) and b = 3"),
            vec!["( a = 1 or a = 2 )", "b = 3"]
        );
    }

    #[test]
    fn should_handle_not_between_and_multiple_ranges() {
        assert_eq!(
            words("x not between 1 and 2 and y between 3 and 4"),
            vec!["x not between 1 and 2", "y between 3 and 4"]
        );
    }

    #[test]
    fn should_classify_field_equality_as_key_and_var_equality_as_residual() {
        let record = VarName("r".into());
        let sel = Select {
            table: Name("t".into()),
            alias: None,
            columns: vec![Name("k".into())],
            sources: vec![Name("k".into())],
            filter: None,
            verbatim: None,
        };
        let binds = [
            (
                "$v_r__a".to_string(),
                BindSrc::Field {
                    rec: record.clone(),
                    field: "a".into(),
                },
            ),
            ("$v_x".to_string(), BindSrc::Var(VarName("x".into()))),
        ];
        let bind_map: HashMap<&str, &BindSrc> =
            binds.iter().map(|(p, s)| (p.as_str(), s)).collect();
        let region = CursorRegion {
            open_pc: 0,
            fetch_pc: 0,
            close_pc: 0,
            cursor: VarName("c".into()),
            record: record.clone(),
        };
        // 字段等值 → 键
        let hit = key_conjunct(
            &["a".into(), "=".into(), "$v_r__a".into()],
            &bind_map,
            &region,
            &sel,
        );
        assert!(hit.is_some(), "field equality should be a key");
        // 变量等值 → 非键（残余侧按不变量谓词处理）
        let miss = key_conjunct(
            &["a".into(), "=".into(), "$v_x".into()],
            &bind_map,
            &region,
            &sel,
        );
        assert!(miss.is_none(), "var equality is not a row-variant key");
        // 反向书写同样识别
        let rev = key_conjunct(
            &["$v_r__a".into(), "=".into(), "a".into()],
            &bind_map,
            &region,
            &sel,
        );
        assert!(rev.is_some(), "reversed equality should be a key");
    }
}
