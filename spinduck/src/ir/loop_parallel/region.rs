//! 门禁 1 + 门禁 6：SESE 游标骨架复用、`G_pure` DAG 切分、多路交权
//! 出口 `E_yield`、迭代边界 `E_iter_end` 与反可重入证明、`max_prefix_steps`。
//!
//! 语义依据设计 §3.1/§4.1：`body_start_pc` 为唯一入口；内部禁止任何
//! 向后回边（除精确指向 `fetch_pc` 的迭代边界边）；交权目标经反可重入
//! 证明后才允许进入 `V_pure`。

use super::callmat;
use super::cfg::Cfg;
use super::exprs::{self, Loc};
use super::{ParallelSkipReason, PrewarmQuery, PurePrefixPlan};
use crate::ir::set_oriented::region::{self, CursorRegion};
use crate::ir::{CursorSource, IrProg, IrStmt};
use crate::lookup_cache::shape;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// `body_start_pc` 之后的最小有效工作量（门禁 1 尾部门槛，设计默认 ≥4）。
const MIN_PURE_COMPUTE: usize = 4;

/// 入口：抽取游标区域并构建 `G_pure`。
pub(crate) fn build(
    prog: &IrProg,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<PurePrefixPlan, ParallelSkipReason> {
    let region = extract_region(prog)?;
    reject_cursor_fn_sites(prog, &region, catalog)?;
    let body_start_pc = region.fetch_pc + 2;
    let cond_pc = region.fetch_pc + 1;
    let exit_pc = region.close_pc + 1;
    let cfg = Cfg::new(prog);

    // 循环内全部写位置（range_key 循环不变性判定用；含处理器体，
    // 因为 trap 分派边可达——保守超集）。
    let loop_writes = loop_write_locs(prog, &region, catalog);

    // 门禁 1：自 body_start_pc 前向构建纯子图。
    let (g_pure, queries) =
        build_g_pure(prog, &cfg, &region, body_start_pc, &loop_writes, catalog)?;

    // 门禁 6：拓扑序 + 最长路径（同时复核无环——build 已拒绝回边，
    // trap 边引入的环在此兜底拒绝）。
    let topo = topo_order(&cfg, &g_pure)?;
    let max_prefix_steps = max_steps(&cfg, &g_pure, &topo, body_start_pc);

    // 反可重入证明（§3.1）：移除 E_iter_end 后，任一交权目标不得
    // 可达 V_pure。
    prove_no_reentry(prog, &cfg, &region, &g_pure)?;

    // 最小工作量门禁：≥1 条可预热 SqlQuery 或 ≥4 条有效纯计算指令。
    let compute = g_pure
        .iter()
        .filter(|pc| is_compute_instr(&prog.code[**pc]))
        .count();
    if queries.is_empty() && compute < MIN_PURE_COMPUTE {
        return Err(ParallelSkipReason::InsufficientParallelWork);
    }

    Ok(PurePrefixPlan {
        region,
        body_start_pc,
        cond_pc,
        exit_pc,
        g_pure,
        max_prefix_steps,
        queries,
    })
}

/// 复用 `set_oriented::region::extract` 识别单层游标循环；错误映射到
/// `ParallelSkipReason`（非记录 FETCH / 多目标等形态保守归入
/// `UnsupportedControlFlow`——v1 仅支持 `%ROWTYPE` 单记录目标）。
fn extract_region(prog: &IrProg) -> Result<CursorRegion, ParallelSkipReason> {
    if !prog
        .code
        .iter()
        .any(|s| matches!(s, IrStmt::OpenCursor { .. }))
    {
        return Err(ParallelSkipReason::NoLoopRegionFound);
    }
    region::extract(prog).map_err(|e| match e {
        // extract 只认 Static 源；失败时若存在动态游标，按设计报
        // DynamicCursorSource（门禁 7 先于门禁 1 的语义在此保留）。
        _ if has_dynamic_cursor(prog) => ParallelSkipReason::DynamicCursorSource,
        _ => ParallelSkipReason::UnsupportedControlFlow,
    })
}

fn has_dynamic_cursor(prog: &IrProg) -> bool {
    prog.code.iter().any(|s| {
        matches!(
            s,
            IrStmt::OpenCursor {
                source: CursorSource::Dynamic { .. },
                ..
            }
        )
    })
}

/// 游标源 `fn_sites` 准入判定（V8-E5）：物化在 `OPEN` 一次完成，
/// 两种模式同点同结果，故仅需保证快照口径不分叉——
/// ① `distinct_sql` 绑定不含行相关源（`Field`/`Expr` 在 `OPEN` 时
/// 记录未绑定，属不可物化形态）；② 物化表不被循环区域写
/// （`W_loop` 完备枚举失败时保守拒绝）。
fn reject_cursor_fn_sites(
    prog: &IrProg,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<(), ParallelSkipReason> {
    let Some(IrStmt::OpenCursor { source, .. }) = prog.code.get(region.open_pc) else {
        return Err(ParallelSkipReason::UnsupportedControlFlow);
    };
    let CursorSource::Static { fn_sites, .. } = source else {
        return Err(ParallelSkipReason::DynamicCursorSource);
    };
    if fn_sites.is_empty() {
        return Ok(());
    }
    let row_dependent = fn_sites.iter().any(|site| {
        site.binds
            .iter()
            .any(|(_, src)| !matches!(src, crate::ir::BindSrc::Var(_)))
    });
    if row_dependent {
        return Err(ParallelSkipReason::MaterializedFnSitePresent);
    }
    let w_loop = region::write_set(prog, region, catalog)
        .map_err(|_| ParallelSkipReason::MaterializedFnSitePresent)?;
    if fn_sites
        .iter()
        .any(|site| w_loop.contains(&site.table.to_lowercase()))
    {
        return Err(ParallelSkipReason::MaterializedFnSitePresent);
    }
    Ok(())
}

/// 循环区域（含 trap 处理器体）全部写位置集合——供 range_key
/// 「循环入口静态可定值」判定。语法级保守超集。
fn loop_write_locs(
    prog: &IrProg,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> BTreeSet<Loc> {
    let mut out = BTreeSet::new();
    for stmt in &prog.code[region.open_pc..=region.close_pc] {
        for loc in exprs::stmt_defs(stmt, catalog) {
            out.insert(loc);
        }
    }
    out
}

/// `V_pure` 内可计入「有效赋值/条件指令」的指令种类（最小工作量口径）。
fn is_compute_instr(stmt: &IrStmt) -> bool {
    matches!(
        stmt,
        IrStmt::Assign { .. }
            | IrStmt::Perform { .. }
            | IrStmt::JumpIf { .. }
            | IrStmt::JumpIfNot { .. }
    )
}

/// 单条语句的纯度分类：`Ok(true)` = 进入 `V_pure`；`Ok(false)` = 交权
/// 边界；`Err` = 整域拒绝（`CursorAttr`）。
fn classify(
    stmt: &IrStmt,
    loop_writes: &BTreeSet<Loc>,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<bool, ParallelSkipReason> {
    if exprs::stmt_has_cursor_attr(stmt) {
        return Err(ParallelSkipReason::UnsupportedControlFlow);
    }
    Ok(match stmt {
        IrStmt::Nop
        | IrStmt::Assign { .. }
        | IrStmt::Perform { .. }
        | IrStmt::Jump(_)
        | IrStmt::JumpIf { .. }
        | IrStmt::JumpIfNot { .. }
        | IrStmt::PushTrap { .. }
        | IrStmt::PopTrap => callmat::stmt_exprs_materializable(stmt, catalog, 0),
        // 策略 A（保守交权）：FoldedQuery 一律交权（设计 §4.1 门禁 4）。
        IrStmt::FoldedQuery { .. } => false,
        IrStmt::SqlQuery {
            sql,
            binds,
            into: _,
            fn_sites,
        } => {
            // fn_sites 物化 / 不可静态预热 / range_key 非循环不变 → 交权。
            fn_sites.is_empty() && query_cacheable(sql, binds, loop_writes)
        }
        // 副作用与流控类一律交权；嵌套静态游标已由 extract 拒绝。
        IrStmt::SqlExec { .. }
        | IrStmt::SqlExecDynamic { .. }
        | IrStmt::TxCommit
        | IrStmt::TxCommitIfWritten
        | IrStmt::TxRollback
        | IrStmt::CallRoutine { .. }
        | IrStmt::OpenCursor { .. }
        | IrStmt::Fetch { .. }
        | IrStmt::CloseCursor { .. }
        | IrStmt::Raise { .. }
        | IrStmt::Return { .. } => false,
    })
}

/// `SqlQuery` 能否进入 `V_pure`：静态通过 `shape::recognize` 且全部
/// range_key 绑定在循环入口可定值（对应变量不被循环内任何语句写）。
fn query_cacheable(
    sql: &str,
    binds: &[(String, crate::ir::BindSrc)],
    loop_writes: &BTreeSet<Loc>,
) -> bool {
    // Top-1（`LIMIT 1`/`ROWNUM=1`）歧义查询交权而非拒绝：门禁 5 只对
    // `G_pure` 内 SqlQuery 做 Top-1 审计，交权后主线程原生执行，
    // 语义零变化且不再阻断更早的纯前缀（V8-E2 深化后该形态常见）。
    if super::proof::has_top1_pattern(sql) {
        return false;
    }
    let Some(plan) = shape::recognize(sql) else {
        return false;
    };
    plan.range_binds().iter().all(|name| {
        binds
            .iter()
            .find(|(n, _)| n == name)
            .is_some_and(|(_, src)| match src {
                crate::ir::BindSrc::Var(v) => !loop_writes.contains(&Loc::var(v)),
                crate::ir::BindSrc::Field { rec, field } => {
                    !loop_writes.contains(&Loc::field(rec, field))
                        && !loop_writes.contains(&Loc::var(rec))
                }
                // Expr 绑定无法映射帧位置——保守判不可定值。
                crate::ir::BindSrc::Expr { .. } => false,
            })
    })
}

/// 构建 `G_pure`：从 `body_start_pc` 前向遍历，分类语句并记录交权边。
/// 返回 `(V_pure, 预热查询计划)`。
fn build_g_pure(
    prog: &IrProg,
    cfg: &Cfg<'_>,
    region: &CursorRegion,
    body_start_pc: usize,
    loop_writes: &BTreeSet<Loc>,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<(BTreeSet<usize>, Vec<PrewarmQuery>), ParallelSkipReason> {
    let mut g_pure: BTreeSet<usize> = BTreeSet::new();
    let mut queries = Vec::new();
    let mut queue: VecDeque<usize> = VecDeque::from([body_start_pc]);
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    while let Some(pc) = queue.pop_front() {
        if !seen.insert(pc) {
            continue;
        }
        let Some(stmt) = prog.code.get(pc) else {
            continue;
        };
        // 越出区域（前缀/后缀/程序外）的语句一律不是纯节点——
        // 它们作为交权目标存在，不进入遍历。
        if pc < region.open_pc || pc > region.close_pc {
            continue;
        }
        if classify(stmt, loop_writes, catalog)? {
            g_pure.insert(pc);
            if let IrStmt::SqlQuery { sql, .. } = stmt {
                queries.push(PrewarmQuery {
                    pc,
                    sql: sql.clone(),
                });
            }
            for &next in cfg.struct_succs(pc) {
                // 迭代边界边：精确指向 fetch_pc 的回跳为合法续环。
                if next == region.fetch_pc {
                    continue;
                }
                // 跳出区域的出边 = 交权（EXIT/RETURN/外跳），不遍历。
                if next < region.open_pc || next > region.close_pc {
                    continue;
                }
                // 区域内任何 u <= pc 的回边（含跳 cond_pc/open_pc、
                // 局部循环、自环）都是不可纯化控制流，拒绝。
                if next <= pc {
                    return Err(ParallelSkipReason::UnsupportedControlFlow);
                }
                if !seen.contains(&next) {
                    queue.push_back(next);
                }
            }
        }
        // 交权语句：不加入 g_pure，其后继不再属于纯子图（由调用方
        // 通过「g_pure 节点的后继 ∉ g_pure」隐式表达 E_yield）。
    }
    Ok((g_pure, queries))
}

/// Kahn 拓扑排序 `g_pure` 内部边；存在环（理论上仅可能由 trap 边
/// 引入）时报 `UnsupportedControlFlow`。
pub(crate) fn topo_order(
    cfg: &Cfg<'_>,
    g_pure: &BTreeSet<usize>,
) -> Result<Vec<usize>, ParallelSkipReason> {
    let mut indeg: BTreeMap<usize, usize> = g_pure.iter().map(|pc| (*pc, 0)).collect();
    for &u in g_pure {
        for &v in cfg.struct_succs(u) {
            if g_pure.contains(&v) {
                if let Some(d) = indeg.get_mut(&v) {
                    *d += 1;
                }
            }
        }
    }
    let mut queue: VecDeque<usize> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(pc, _)| *pc)
        .collect();
    let mut order = Vec::with_capacity(g_pure.len());
    while let Some(u) = queue.pop_front() {
        order.push(u);
        for &v in cfg.struct_succs(u) {
            if !g_pure.contains(&v) {
                continue;
            }
            if let Some(d) = indeg.get_mut(&v) {
                *d -= 1;
                if *d == 0 {
                    queue.push_back(v);
                }
            }
        }
    }
    if order.len() != g_pure.len() {
        return Err(ParallelSkipReason::UnsupportedControlFlow);
    }
    Ok(order)
}

/// 门禁 6：DAG 最长路径 = 每行从 `body_start_pc` 到任一交权出口的
/// 最大指令步数。交权后继不计入（Worker 在出口前停步）。
fn max_steps(cfg: &Cfg<'_>, g_pure: &BTreeSet<usize>, topo: &[usize], body_start_pc: usize) -> u64 {
    let mut cost: BTreeMap<usize, u64> = BTreeMap::new();
    for &u in topo.iter().rev() {
        let mut best = 0u64;
        for &v in cfg.struct_succs(u) {
            if g_pure.contains(&v) {
                best = best.max(cost.get(&v).copied().unwrap_or(0));
            }
        }
        cost.insert(u, best + 1);
    }
    cost.get(&body_start_pc).copied().unwrap_or(0)
}

/// 反可重入证明（§3.1）：
/// 1. `E_iter_end` = 环内指向 `fetch_pc` 的回跳边（已在建图时过滤）+
///    跳出 `[open_pc..=close_pc]` 的出边；
/// 2. 从任意交权目标 `v ∉ V_pure`（`v != fetch_pc`）出发，在**全程序**
///    CFG 上（已移除 →fetch_pc 环内边）不可达 `V_pure` 任何节点；
/// 3. trap 分派边精确化：`V_pure` 节点 `u` 的 trap 后继 `h` 可达的
///    `V_pure` 节点若全是 `u` 的**后代**（结构后继方向），则安全——
///    原生执行 `u` 蕴含 Worker 已在 `u` 处交权，`u` 的后代尚未被
///    Worker 执行；仅当 `h` 可达 `u` 的 `V_pure` **祖先**（Worker 必先
///    执行、`u` 处交权后原生重跑）才是真重入。
fn prove_no_reentry(
    prog: &IrProg,
    cfg: &Cfg<'_>,
    region: &CursorRegion,
    g_pure: &BTreeSet<usize>,
) -> Result<(), ParallelSkipReason> {
    // 交权边 (u→v)：u ∈ V_pure 的结构后继 v ∉ V_pure（v != fetch_pc）。
    // Worker 在 u 处交权、主线程自 v 原生续跑；trap 分派边不产生
    // 种子——Worker 无异常分派，其目标可达性由第 3 步按
    // 「受保护语句 → 处理器」逐边精确判定（`reentry_tests` 守护
    // purchase 的「处理器前向汇合到 G_pure 后继」形态）。
    let mut yield_edges: Vec<(usize, usize)> = Vec::new();
    let mut seeds: BTreeSet<usize> = BTreeSet::new();
    for &u in g_pure {
        for &v in cfg.struct_succs(u) {
            if !g_pure.contains(&v) && v != region.fetch_pc && v < prog.code.len() {
                yield_edges.push((u, v));
                seeds.insert(v);
            }
        }
    }
    // V_pure 内结构前驱表：祖先反向 BFS 与 trap 步共享。
    let mut preds: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &u in g_pure {
        for &v in cfg.struct_succs(u) {
            if g_pure.contains(&v) {
                preds.entry(v).or_default().push(u);
            }
        }
    }
    // 每个交权目标 v 的 V_pure 可达集（全 CFG 边——含 trap 分派
    // 保守边；fetch_pc/open_pc 视为截断点，其后属下一轮迭代/区域
    // 外，不算同轮重入）。
    let mut reach: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &v in &seeds {
        let mut hits = BTreeSet::new();
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        let mut stack = vec![v];
        while let Some(pc) = stack.pop() {
            if pc == region.fetch_pc || pc == region.open_pc || !seen.insert(pc) {
                continue;
            }
            if g_pure.contains(&pc) {
                hits.insert(pc);
            }
            for &next in cfg.succs(pc) {
                if next != region.fetch_pc && !seen.contains(&next) {
                    stack.push(next);
                }
            }
        }
        reach.insert(v, hits);
    }
    // 逐交权边判定：v 可达的 V_pure 节点若全是 u 的**后代或并行
    // 分支**（非祖先），则 Worker 本行未执行它们——主线程原生续跑
    // 恰执行一次，安全（IF 分支汇合形态：一臂交权、汇合尾同时被
    // 另一臂纳入 V_pure）。仅当 v 可达 u 的 V_pure **祖先**（含 u
    // 自身）时，Worker 本行已执行过该节点、主线程重放才是真重入。
    for &(u, v) in &yield_edges {
        let Some(hits) = reach.get(&v) else {
            continue;
        };
        if hits.is_empty() {
            continue;
        }
        let mut ancestors: BTreeSet<usize> = BTreeSet::new();
        let mut astack = vec![u];
        while let Some(x) = astack.pop() {
            if !ancestors.insert(x) {
                continue;
            }
            if let Some(ps) = preds.get(&x) {
                astack.extend(ps.iter().copied());
            }
        }
        if hits.iter().any(|x| ancestors.contains(x)) {
            return Err(ParallelSkipReason::UnsupportedControlFlow);
        }
    }
    prove_trap_reentry(prog, cfg, region, g_pure, &preds)
}

/// 第 3 步：trap 分派边的祖先级重入检查。对每个带 trap 分派边的
/// `V_pure` 节点 `u`，计算 `u` 在 `V_pure` 内的结构祖先集 `A_u`
/// （含 `u` 自身），再验证每个处理器入口 `h` 的全 CFG 可达集不含
/// `A_u` 成员。`A_u` 之外的 `V_pure` 可达目标均为 `u` 的后代或
/// 并行分支——原生执行 `u` 时 Worker 均未执行它们，重跑安全。
fn prove_trap_reentry(
    prog: &IrProg,
    cfg: &Cfg<'_>,
    region: &CursorRegion,
    g_pure: &BTreeSet<usize>,
    preds: &BTreeMap<usize, Vec<usize>>,
) -> Result<(), ParallelSkipReason> {
    for &u in g_pure {
        let struct_succs = cfg.struct_succs(u);
        let trap_targets: Vec<usize> = cfg
            .succs(u)
            .iter()
            .copied()
            .filter(|v| !struct_succs.contains(v) && *v < prog.code.len())
            .collect();
        if trap_targets.is_empty() {
            continue;
        }
        // A_u：u 的 V_pure 结构祖先（含自身）。
        let mut ancestors: BTreeSet<usize> = BTreeSet::new();
        let mut astack = vec![u];
        while let Some(x) = astack.pop() {
            if !ancestors.insert(x) {
                continue;
            }
            if let Some(ps) = preds.get(&x) {
                astack.extend(ps.iter().copied());
            }
        }
        for h in trap_targets {
            // R_h：处理器入口的全 CFG 可达集（跳过 fetch/open）。
            let mut reached: BTreeSet<usize> = BTreeSet::new();
            let mut rstack = vec![h];
            while let Some(x) = rstack.pop() {
                if x == region.fetch_pc || x == region.open_pc || !reached.insert(x) {
                    continue;
                }
                if ancestors.contains(&x) {
                    return Err(ParallelSkipReason::UnsupportedControlFlow);
                }
                for &next in cfg.succs(x) {
                    if next != region.fetch_pc && !reached.contains(&next) {
                        rstack.push(next);
                    }
                }
            }
        }
    }
    Ok(())
}
