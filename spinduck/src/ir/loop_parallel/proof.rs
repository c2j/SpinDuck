//! 门禁 2（字段粒度跨轮状态独立性）与门禁 5（传递性确定性 /
//! 会话无关 / 事务锚定读 / Top-1 歧义）。
//!
//! 门禁 2 语义（设计 §4.1-2）：对整轮循环 CFG 做到达定值——`V_pure`
//! 内任何读取，若其值可能来自上一轮迭代（残余段写回、异常路径漏写
//! 残留、隐式 `__sqlerrm`），一律 `CycleCarriedDependence`。粒度为
//! `VarName` 整变量 + `(rec, field)` 字段路径；整记录读保守视作读
//! 全部字段。

use super::cfg::Cfg;
use super::exprs::{self, Loc};
use super::region;
use super::{ParallelSkipReason, PurePrefixPlan};
use crate::catalog::RoutineCatalog;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{IrExpr, IrProg, IrStmt, VarName};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// trap 进入时解释器以 `frame.set` 隐式写入的帧变量（`interp.rs`），
/// 纯 IR 定值分析不可见，须显式建模（R4-P1-2）。
const IMPLICIT_SQLERRM: &str = "__sqlerrm";

/// 门禁 2 入口。
pub(crate) fn check_loop_carried_independence(
    prog: &IrProg,
    plan: &PurePrefixPlan,
    catalog: &RoutineCatalog,
) -> Result<(), ParallelSkipReason> {
    let region = &plan.region;
    let cfg = Cfg::new(prog);
    let defs = defs_at(prog, region, catalog);

    // 可达 fetch_pc 的区域内 PC 集合（反向 BFS；区域外节点无法回到
    // fetch_pc——反重入证明已保证）。
    let reaching = pcs_reaching_fetch(prog, &cfg, region);

    // 跨轮定值集 C：区域内可达 fetch_pc 的写位置 ∪ 隐式 __sqlerrm
    // （trap 处理器体入口）。fetch_pc 自身的 FETCH 目标是每行刷新
    // 的输入，不算"残留"。
    let carried = carried_locations(&defs, region, &reaching);

    // G_pure 内 must-def 前向分析（DAG 拓扑序单遍）。
    let topo = region::topo_order(&cfg, &plan.g_pure)?;
    let recordish = recordish_vars(prog, region);
    let preds = pure_preds(&cfg, &plan.g_pure);
    let mut must_in: BTreeMap<usize, BTreeSet<Loc>> = BTreeMap::new();
    // 入口不变量：FETCH 目标每轮必写（record=true 单 Var 目标）。
    must_in.insert(
        plan.body_start_pc,
        fetch_target_locs(prog, region).into_iter().collect(),
    );
    for &u in &topo {
        if u == plan.body_start_pc {
            continue;
        }
        let mut acc: Option<BTreeSet<Loc>> = None;
        for &p in preds.get(&u).into_iter().flatten() {
            let out = must_out(&must_in, &defs, p);
            acc = Some(match acc {
                None => out,
                Some(prev) => prev.intersection(&out).cloned().collect(),
            });
        }
        must_in.insert(u, acc.unwrap_or_default());
    }

    // 逐用点新鲜度检查。
    for &pc in &plan.g_pure {
        let Some(stmt) = prog.code.get(pc) else {
            continue;
        };
        let must = must_in.get(&pc).cloned().unwrap_or_default();
        for loc in exprs::stmt_uses(stmt) {
            if is_fresh(&loc, &must) {
                continue;
            }
            if is_carried(&loc, &carried, &recordish) {
                return Err(ParallelSkipReason::CycleCarriedDependence);
            }
        }
    }
    Ok(())
}

/// `pc` 的写位置（含 trap 入口隐式 `__sqlerrm`）。
fn defs_at(
    prog: &IrProg,
    region: &CursorRegion,
    catalog: &RoutineCatalog,
) -> BTreeMap<usize, Vec<Loc>> {
    let mut map: BTreeMap<usize, Vec<Loc>> = BTreeMap::new();
    for (i, stmt) in prog.code[region.open_pc..=region.close_pc]
        .iter()
        .enumerate()
    {
        let pc = region.open_pc + i;
        map.insert(pc, exprs::stmt_defs(stmt, catalog));
    }
    // trap 处理器体入口：解释器以 frame.set 写 __sqlerrm（无 coerce）。
    for stmt in &prog.code {
        if let IrStmt::PushTrap { entries } = stmt {
            for e in entries {
                if e.body_start >= region.open_pc && e.body_start <= region.close_pc {
                    map.entry(e.body_start)
                        .or_default()
                        .push(Loc::var(&VarName(IMPLICIT_SQLERRM.into())));
                }
            }
        }
    }
    map
}

/// `g_pure` 内节点的纯前驱表。
fn pure_preds(cfg: &Cfg<'_>, g_pure: &BTreeSet<usize>) -> BTreeMap<usize, Vec<usize>> {
    let mut preds: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &u in g_pure {
        for &v in cfg.succs(u) {
            if g_pure.contains(&v) {
                preds.entry(v).or_default().push(u);
            }
        }
    }
    preds
}

/// `must_out(pc)` = `(must_in - killed) ∪ defs`；`Var(v)` 写杀死 `v` 的
/// 全部字段位（整记录重写），`Field` 写只杀自身。
fn must_out(
    must_in: &BTreeMap<usize, BTreeSet<Loc>>,
    defs: &BTreeMap<usize, Vec<Loc>>,
    pc: usize,
) -> BTreeSet<Loc> {
    let mut out = must_in.get(&pc).cloned().unwrap_or_default();
    for d in defs.get(&pc).into_iter().flatten() {
        match d {
            Loc::Var(v) => {
                out.retain(|l| !matches!(l, Loc::Field { rec, .. } if rec == v));
                out.insert(d.clone());
            }
            Loc::Field { .. } => {
                out.insert(d.clone());
            }
        }
    }
    out
}

/// 用点新鲜度：`Var(v)` 需 `Var(v)` 在 must 集；`Field{r,f}` 接受
/// `Field(r,f)` 或整记录 `Var(r)`（整记录写覆盖全字段）。
fn is_fresh(loc: &Loc, must: &BTreeSet<Loc>) -> bool {
    match loc {
        Loc::Var(v) => must.contains(&Loc::Var(v.clone())),
        Loc::Field { rec, field } => {
            must.contains(&Loc::Field {
                rec: rec.clone(),
                field: field.clone(),
            }) || must.contains(&Loc::Var(rec.clone()))
        }
    }
}

/// 跨轮携带判定：`Var(v)` 读在 v 为记录变量时，任一 `Field(v,*)`
/// 被跨轮写亦视为携带（整记录读覆盖全字段）。
fn is_carried(loc: &Loc, carried: &BTreeSet<Loc>, recordish: &BTreeSet<VarName>) -> bool {
    match loc {
        Loc::Var(v) => {
            carried.contains(&Loc::Var(v.clone()))
                || (recordish.contains(v)
                    && carried
                        .iter()
                        .any(|l| matches!(l, Loc::Field { rec, .. } if rec == v)))
        }
        Loc::Field { rec, field } => {
            carried.contains(&Loc::Field {
                rec: rec.clone(),
                field: field.clone(),
            }) || carried.contains(&Loc::Var(rec.clone()))
        }
    }
}

/// 记录变量集合：全程序 `Field{rec}` 出现处 + FETCH 记录目标。
fn recordish_vars(prog: &IrProg, region: &CursorRegion) -> BTreeSet<VarName> {
    let mut out = exprs::record_vars(prog);
    out.insert(region.record.clone());
    out
}

/// FETCH 目标位置（每轮刷新输入，作为 `G_pure` 入口 must-def）。
fn fetch_target_locs(prog: &IrProg, region: &CursorRegion) -> Vec<Loc> {
    match prog.code.get(region.fetch_pc) {
        Some(IrStmt::Fetch { targets, .. }) => targets.iter().map(exprs::target_loc).collect(),
        // fetch_pc 非 Fetch 指令属计划/区域错位：无目标可防，
        // 由下游守卫按空集处理（不吞错——G_pure 另有结构校验）。
        _ => Vec::new(),
    }
}

/// 可达 `fetch_pc` 的区域内 PC（反向 BFS，边取自 `Cfg::succs` 过滤
/// 在 `[open_pc..=close_pc]` 内）。
fn pcs_reaching_fetch(prog: &IrProg, cfg: &Cfg<'_>, region: &CursorRegion) -> BTreeSet<usize> {
    let last = region.close_pc.min(prog.code.len().saturating_sub(1));
    let mut rev: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for u in region.open_pc..=last {
        for &v in cfg.succs(u) {
            if v >= region.open_pc && v <= region.close_pc {
                rev.entry(v).or_default().push(u);
            }
        }
    }
    let mut seen: BTreeSet<usize> = BTreeSet::from([region.fetch_pc]);
    let mut queue: VecDeque<usize> = VecDeque::from([region.fetch_pc]);
    while let Some(v) = queue.pop_front() {
        for &u in rev.get(&v).into_iter().flatten() {
            if seen.insert(u) {
                queue.push_back(u);
            }
        }
    }
    seen
}

/// 跨轮定值集：区域内可达 fetch_pc 的写位置（隐式 `__sqlerrm` 已含
/// 于 `defs_at`），排除 fetch_pc 自身（每行刷新输入）。
fn carried_locations(
    defs: &BTreeMap<usize, Vec<Loc>>,
    region: &CursorRegion,
    reaching: &BTreeSet<usize>,
) -> BTreeSet<Loc> {
    let mut out = BTreeSet::new();
    for &pc in reaching {
        if pc == region.fetch_pc {
            continue;
        }
        for loc in defs.get(&pc).into_iter().flatten() {
            out.insert(loc.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 门禁 5：传递性确定性、会话无关性、事务锚定观察禁令、Top-1 歧义
// ---------------------------------------------------------------------------

/// 事务锚定读（随 `ROLLBACK; BEGIN` 变化；预热复位会切换事务）。
const TX_ANCHORED_FNS: &[&str] = &[
    "current_timestamp",
    "transaction_timestamp",
    "statement_timestamp",
    "now",
    "sysdate",
    "localtimestamp",
    "localtime",
    "current_date",
];

/// 非确定性 / 会话耦合函数。
const NONDET_FNS: &[&str] = &[
    "random",
    "setseed",
    "uuid",
    "sys_guid",
    "nextval",
    "currval",
    "clock_timestamp",
];

/// 门禁 5 入口：审计范围 = 前缀 + 区域 + 传递调用闭包（V8-E4 修订，
/// 取代 R5-P1-1 全例程口径）。
///
/// 范围收窄依据：事务锚定禁令防的是「并行路径引入串行不存在的额外
/// 事务切换」。当前实现不存在该切换——
/// ① 预热建索引仅 `inner.query` 只读 SELECT（`lookup_cache::build`），
///    DuckDB 1.5.5 实证只读查询失败不污染事务（后续
///    SELECT/INSERT/COMMIT 全部正常）；
/// ② 预热失败路径直接退回串行、不调用 `reset_session`；
/// ③ `CallRoutine`/`SqlFn` 物化经 `callmat` 证明无事务操作；
/// ④ 区域内 `TxCommit`/`TxRollback`/trap 触发的 `reset_session` 均为
///    IR 原生语句，两种模式逐语句一致。
/// 后缀（`close_pc` 之后）在并行窗口关闭后原生执行，事务演化与串行
/// 完全一致，故不审计；byfund 段 2 `OPEN c_fxj FOR` 动态游标即因此
/// 不再污染段 1 判定。
pub(crate) fn check_determinism_and_tx_anchor(
    prog: &IrProg,
    plan: &PurePrefixPlan,
    catalog: &RoutineCatalog,
) -> Result<(), ParallelSkipReason> {
    let mut visiting = BTreeSet::new();
    let last = plan.region.close_pc.min(prog.code.len().saturating_sub(1));
    for stmt in &prog.code[..=last] {
        scan_stmt_tx(stmt, catalog, &mut visiting)?;
    }
    // V_pure 内 SqlQuery 的 Top-1 歧义检查（仅纯前缀口径）。
    for &pc in &plan.g_pure {
        if let IrStmt::SqlQuery { sql, .. } = &prog.code[pc] {
            if has_top1_pattern(sql) {
                return Err(ParallelSkipReason::NonDeterministicOrAmbiguousTop1);
            }
        }
    }
    Ok(())
}

fn scan_stmt_tx(
    stmt: &IrStmt,
    catalog: &RoutineCatalog,
    visiting: &mut BTreeSet<String>,
) -> Result<(), ParallelSkipReason> {
    match stmt {
        IrStmt::Assign { expr, .. }
        | IrStmt::Perform { expr }
        | IrStmt::FoldedQuery { expr, .. } => scan_expr_tx(expr, catalog, visiting),
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => {
            scan_expr_tx(cond, catalog, visiting)
        }
        IrStmt::Raise {
            message: Some(expr),
        }
        | IrStmt::Return { value: Some(expr) } => scan_expr_tx(expr, catalog, visiting),
        IrStmt::SqlExecDynamic { .. } => {
            // 动态 SQL 文本运行期构造，无法静态证明不读事务锚定值。
            Err(ParallelSkipReason::TransactionAnchoredRead)
        }
        IrStmt::SqlExec { sql, fn_sites, .. } | IrStmt::SqlQuery { sql, fn_sites, .. } => {
            scan_sql_text_tx(sql)?;
            scan_sites_tx(fn_sites, catalog, visiting)
        }
        IrStmt::OpenCursor { source, .. } => match source {
            // V8-E5：游标 fn_site 物化在 `OPEN` 主线程同点执行，两种
            // 模式逐语句一致，不属于预热期/Worker 期的事务锚定风险面
            // ——跳过 callee 闭包审计（准入性由 `region::reject_cursor_fn_sites`
            // 保证）。
            crate::ir::CursorSource::Static { sql, .. } => scan_sql_text_tx(sql),
            crate::ir::CursorSource::Dynamic { query_expr } => {
                scan_expr_tx(query_expr, catalog, visiting)?;
                // OPEN FOR 文本运行期构造——保守拒判。
                Err(ParallelSkipReason::TransactionAnchoredRead)
            }
        },
        IrStmt::CallRoutine { name, args, .. } => {
            for a in args {
                scan_expr_tx(a, catalog, visiting)?;
            }
            scan_callee_tx(name, catalog, visiting)
        }
        // 无表达式/无 callee 的指令变体无事务读可传递。
        _ => Ok(()),
    }
}

fn scan_sites_tx(
    sites: &[crate::sql::FnSite],
    catalog: &RoutineCatalog,
    visiting: &mut BTreeSet<String>,
) -> Result<(), ParallelSkipReason> {
    for s in sites {
        scan_callee_tx(&s.name, catalog, visiting)?;
    }
    Ok(())
}

/// 传递闭包：目录命中则递归扫描 callee 全文；未命中（外部/未注册）
/// 保守拒判（设计 §4.1 门禁 5：闭包不可达同样拒判）。
fn scan_callee_tx(
    name: &str,
    catalog: &RoutineCatalog,
    visiting: &mut BTreeSet<String>,
) -> Result<(), ParallelSkipReason> {
    let key = name.to_ascii_lowercase();
    if !visiting.insert(key.clone()) {
        return Ok(());
    }
    let Some(routine) = catalog.lookup(name) else {
        return Err(ParallelSkipReason::TransactionAnchoredRead);
    };
    // callee 全文扫描（Top-1 检查仅属本层 V_pure，不下传）。
    for stmt in &routine.prog.code {
        scan_stmt_tx(stmt, catalog, visiting)?;
    }
    visiting.remove(&key);
    Ok(())
}

fn scan_expr_tx(
    expr: &IrExpr,
    catalog: &RoutineCatalog,
    visiting: &mut BTreeSet<String>,
) -> Result<(), ParallelSkipReason> {
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } | IrExpr::CursorAttr { .. } => {
            Ok(())
        }
        IrExpr::Bin { l, r, .. } => {
            scan_expr_tx(l, catalog, visiting)?;
            scan_expr_tx(r, catalog, visiting)
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => scan_expr_tx(e, catalog, visiting),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(e) = operand {
                scan_expr_tx(e, catalog, visiting)?;
            }
            for (c, v) in whens {
                scan_expr_tx(c, catalog, visiting)?;
                scan_expr_tx(v, catalog, visiting)?;
            }
            match else_expr {
                Some(e) => scan_expr_tx(e, catalog, visiting),
                None => Ok(()),
            }
        }
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => {
            check_fn_name_tx(name)?;
            if let Ok(sql) = prepared {
                scan_sql_text_tx(sql)?;
            }
            for a in args {
                scan_expr_tx(a, catalog, visiting)?;
            }
            // 目录命中的 PL 例程同样纳入传递闭包。
            if catalog.lookup(name).is_some() {
                scan_callee_tx(name, catalog, visiting)?;
            }
            Ok(())
        }
        IrExpr::SqlFn {
            name,
            args,
            prepared,
        } => {
            check_fn_name_tx(name)?;
            if let Ok(p) = prepared {
                scan_sql_text_tx(&p.sql)?;
            }
            for a in args {
                scan_expr_tx(a, catalog, visiting)?;
            }
            Ok(())
        }
        IrExpr::Lookup { args, default, .. } => {
            for (_, a) in args {
                scan_expr_tx(a, catalog, visiting)?;
            }
            scan_expr_tx(default, catalog, visiting)
        }
    }
}

/// SQL 文本词法扫描：按非标识符切分后比较禁用函数名（保守——
/// 列名撞名亦拒，不影响正确性只影响准入率）。
fn scan_sql_text_tx(sql: &str) -> Result<(), ParallelSkipReason> {
    for word in sql_words(sql) {
        let w = word.to_ascii_lowercase();
        if TX_ANCHORED_FNS.contains(&w.as_str()) {
            return Err(ParallelSkipReason::TransactionAnchoredRead);
        }
        if NONDET_FNS.contains(&w.as_str()) {
            return Err(ParallelSkipReason::NonDeterministicOrAmbiguousTop1);
        }
    }
    Ok(())
}

fn check_fn_name_tx(name: &str) -> Result<(), ParallelSkipReason> {
    let w = name.to_ascii_lowercase();
    if TX_ANCHORED_FNS.contains(&w.as_str()) {
        return Err(ParallelSkipReason::TransactionAnchoredRead);
    }
    if NONDET_FNS.contains(&w.as_str()) {
        return Err(ParallelSkipReason::NonDeterministicOrAmbiguousTop1);
    }
    Ok(())
}

/// `LIMIT 1` / `ROWNUM = 1` 形态检测（Top-1 歧义；无 PK/UNIQUE 证明
/// 时首期一律拒判，设计 §4.1 门禁 5）。
pub(crate) fn has_top1_pattern(sql: &str) -> bool {
    let words: Vec<String> = sql_words(sql)
        .into_iter()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    for w in words.windows(2) {
        if w[0] == "limit" && w[1] == "1" {
            return true;
        }
    }
    words.iter().any(|w| w == "rownum") && words.iter().any(|w| w == "1")
}

/// SQL 文本 → 标识符词序列（非 `[A-Za-z0-9_]` 切分）。
fn sql_words(sql: &str) -> Vec<&str> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}
