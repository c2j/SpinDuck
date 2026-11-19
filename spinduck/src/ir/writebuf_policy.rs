//! V3-6 写缓冲编译期策略（设计 §3.6）：IR 编译完成后对**全部**例程统一运行，
//! 与是否走集合化无关。默认 `BufferPolicy::Immediate`（安全失败兜底）；
//! 仅「纯 `INSERT ... VALUES`」且通过 CFG 安全扫描的 `SqlExec` 升级 `Buffered`。
//!
//! 红线（任一命中即保持 `Immediate`）：
//! 1. 从该 INSERT 出发、到达确定刷盘屏障之前的全部可达路径上，存在跨表
//!    写耦合：非 `VALUES` 的修改性 `SqlExec` 触及其他表，且从**例程入口**
//!    到该写存在一条路径，其间没有任何「最终 `Buffered`、目标表被该写触及」
//!    的 `INSERT ... VALUES` 保持有效（该 INSERT 之后、该写之前没有同表
//!    非 `VALUES` 写 / `CallRoutine` / `SqlExecDynamic` 把该表刷掉）——
//!    运行期 `flush_if_touches` 只在被写表已有缓冲行时才先刷盘保序；
//!    全局屏障：`CallRoutine` / `SqlExecDynamic` / `Return` / `TxRollback`；
//!    同表屏障：触及同表的 `SqlQuery` / `FoldedQuery` / 非 `VALUES`
//!    `SqlExec`——`OpenCursor`/`Fetch`/`CloseCursor`/`TxCommit`/`Jump*`
//!    不是屏障，扫描必须穿过）；
//! 2. 包裹该 INSERT 的任意活跃 `PushTrap` 处理器体不是「空跳过」——即含
//!    副作用或控制流跳转（`CLOSE`、`GOTO`、写 OUT/INOUT 形参如
//!    `p_o_succeed`、写日志/调例程、SQL 读写、可能抛错的表达式等）。
//!    纯帧内观察赋值（`v_caught := 'yes'`）不算副作用，处理器仍视为空跳过。
//!
//! `out_params` 为例程 OUT/INOUT 形参名集（小写）；无例程形参上下文的
//! 编译路径（`compile_stmts`/`compile_block`）传空集。

use super::types::{AssignTarget, BufferPolicy, IrExpr, IrProg, IrStmt};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// 编译入口统一调用：就地写回每条 `SqlExec` 的 `buffer_policy`。
/// `out_params` 为例程 OUT/INOUT 形参名集（小写）；非例程路径传空集。
pub(crate) fn apply(prog: &mut IrProg, out_params: &BTreeSet<String>) {
    let succ = successors(&prog.code);
    let candidates: Vec<(usize, String)> = prog
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, s)| insert_target(s).map(|t| (pc, t)))
        .collect();
    if candidates.is_empty() {
        return;
    }
    let clean = classify(&prog.code, &succ, &candidates, out_params);
    for ((pc, _), ok) in candidates.iter().zip(clean) {
        if ok {
            if let IrStmt::SqlExec { buffer_policy, .. } = &mut prog.code[*pc] {
                *buffer_policy = BufferPolicy::Buffered;
            }
        }
    }
}

/// 两阶段判定：先乐观扫描（其他表 `INSERT ... VALUES` 一律视为同伴候选、
/// 穿透），再单调淘汰——路径上穿过「最终保持 Immediate 的其他表 INSERT」
/// 的候选降级，至不动点。
fn classify(
    code: &[IrStmt],
    succ: &[Vec<usize>],
    candidates: &[(usize, String)],
    out_params: &BTreeSet<String>,
) -> Vec<bool> {
    let mut clean = vec![true; candidates.len()];
    let mut immediate = HashSet::new();
    loop {
        let unguarded = unguarded_writes(code, succ, candidates, &immediate);
        let scan = Scan {
            code,
            succ,
            out_params,
            immediate: &immediate,
            unguarded: &unguarded,
        };
        let mut changed = false;
        for (i, (pc, table)) in candidates.iter().enumerate() {
            if clean[i] && !scan.clean(*pc, table) {
                clean[i] = false;
                changed = true;
            }
        }
        if !changed {
            return clean;
        }
        immediate = candidates
            .iter()
            .zip(&clean)
            .filter(|(_, ok)| !**ok)
            .map(|((pc, _), _)| *pc)
            .collect();
    }
}

/// 单候选的 CFG 扫描上下文；`immediate` 为已判 Immediate 的 `INSERT ...
/// VALUES` pc 集，`unguarded` 为存在无守卫入口路径的跨表修改写 pc 集
/// （淘汰轮使用，乐观轮 `immediate` 为空集）。
struct Scan<'a> {
    code: &'a [IrStmt],
    succ: &'a [Vec<usize>],
    out_params: &'a BTreeSet<String>,
    immediate: &'a HashSet<usize>,
    unguarded: &'a HashSet<usize>,
}

/// 候选指令：`SqlExec` 且为严格「INSERT INTO 表 [(列)] VALUES ...」形态，
/// 返回归一化表名（小写、去引号）。
fn insert_target(stmt: &IrStmt) -> Option<String> {
    let IrStmt::SqlExec {
        sql, buffer_policy, ..
    } = stmt
    else {
        return None;
    };
    if *buffer_policy != BufferPolicy::Immediate {
        return None;
    }
    plain_insert_values(sql)
}

/// 严格解析 `insert into <表> [(列清单)] values ...`；非纯 VALUES 形态返回
/// `None`（`INSERT ... SELECT`、`INSERT` 无 `VALUES` 等一律不算）。
fn plain_insert_values(sql: &str) -> Option<String> {
    let lower = sql.trim_start().to_ascii_lowercase();
    let rest = lower.strip_prefix("insert into")?;
    let rest = rest.trim_start();
    let (table, mut i) = take_ident(rest, 0)?;
    let bytes = rest.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'(' {
        i = skip_parens(rest, i)?;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    rest[i..].strip_prefix("values").filter(|tail| {
        tail.chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })?;
    Some(normalize_ident(&table))
}

/// 取标识符 token（含 `.`、`"`、`$` 限定/引用形态），返回 (token, end)。
fn take_ident(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '"' | '$') {
            i += 1;
        } else {
            break;
        }
    }
    (i > start).then(|| (s[start..i].to_string(), i))
}

/// 跳过平衡括号段（列清单），返回 `)` 之后下标；不配对返回 `None`。
fn skip_parens(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 表名归一化：小写、去全部双引号（`"sch"."t"` → `sch.t`）。
fn normalize_ident(token: &str) -> String {
    token.replace('"', "").to_ascii_lowercase()
}

/// SQL 文本的标识符 token 集（与 `writebuf::touches_buffered_table` 同口径：
/// 标识符字符含 `.` 与 `"`，token 去首尾引号、小写）。
fn ident_tokens(sql: &str) -> HashSet<String> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '"' | '$')))
        .map(|t| normalize_ident(t.trim_matches('"')))
        .filter(|t| !t.is_empty())
        .collect()
}

fn touches_table(sql: &str, table: &str) -> bool {
    ident_tokens(sql).contains(table)
}

/// 首关键字（小写）。
fn first_keyword(sql: &str) -> String {
    sql.trim_start()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 是否「修改表」的 DML/DDL 首关键字。
fn modifies_table(sql: &str) -> bool {
    matches!(
        first_keyword(sql).as_str(),
        "insert" | "update" | "delete" | "merge" | "truncate" | "create" | "drop" | "alter"
    )
}

/// 静态 CFG 后继：条件跳转含顺序后继；`Return`/`TxRollback`/`Raise` 无静态
/// 后继（`Raise` 的处理器派发已由红线 2 的活跃 trap 检查覆盖）。
fn successors(code: &[IrStmt]) -> Vec<Vec<usize>> {
    code.iter()
        .enumerate()
        .map(|(pc, st)| match st {
            IrStmt::Jump(to) => vec![*to],
            IrStmt::JumpIf { to, .. } | IrStmt::JumpIfNot { to, .. } => vec![*to, pc + 1],
            IrStmt::Return { .. } | IrStmt::TxRollback | IrStmt::Raise { .. } => Vec::new(),
            _ if pc + 1 < code.len() => vec![pc + 1],
            _ => Vec::new(),
        })
        .collect()
}

impl Scan<'_> {
    /// 红线 1 + 全局/同表屏障：BFS 全部可达路径，遇屏障即该路径终止；
    /// 任一可达「跨表写耦合」→ false（保持 Immediate）。
    fn clean(&self, start: usize, table: &str) -> bool {
        if trap_handler_effects(self.code, start, self.out_params) {
            return false;
        }
        let mut seen = vec![false; self.code.len()];
        let mut queue: VecDeque<usize> = self.succ[start].iter().copied().collect();
        while let Some(pc) = queue.pop_front() {
            if pc >= self.code.len() || seen[pc] {
                continue;
            }
            seen[pc] = true;
            match &self.code[pc] {
                // 确定全局刷盘屏障：路径终止，不展开
                IrStmt::CallRoutine { .. }
                | IrStmt::SqlExecDynamic { .. }
                | IrStmt::Return { .. }
                | IrStmt::TxRollback => {}
                IrStmt::SqlQuery { sql, .. } | IrStmt::FoldedQuery { sql, .. } => {
                    if !touches_table(sql, table) {
                        queue.extend(self.succ[pc].iter().copied());
                    }
                }
                IrStmt::SqlExec { sql, .. } => {
                    if let Some(t) = plain_insert_values(sql) {
                        if t != table && self.immediate.contains(&pc) {
                            // 其他表 VALUES 且已判 Immediate：即时跨表写 → 违规；
                            // 同表 VALUES 与 Buffered 候选均穿透（淘汰轮复核）
                            return false;
                        }
                        queue.extend(self.succ[pc].iter().copied());
                    } else if touches_table(sql, table) {
                        // 同表非 VALUES 语句：确定刷盘点，路径终止
                    } else if modifies_table(sql) {
                        // 跨表写：入口到该写的每条路径都须有「最终 Buffered
                        // 且目标表被本写触及」的 INSERT ... VALUES 且其间无
                        // 刷盘点；存在无守卫路径 → 跨表写耦合 → 违规
                        if self.unguarded.contains(&pc) {
                            return false;
                        }
                        queue.extend(self.succ[pc].iter().copied());
                    } else {
                        // 只读/非修改性 SQL：不是跨表耦合 → 穿透
                        queue.extend(self.succ[pc].iter().copied());
                    }
                }
                // OpenCursor/Fetch/CloseCursor/TxCommit/Jump*/Assign/Nop/PushTrap/
                // PopTrap 等一律不是屏障，继续扫描
                _ => queue.extend(self.succ[pc].iter().copied()),
            }
        }
        true
    }
}

/// 计算「无守卫」的跨表修改写 pc 集：存在一条从例程入口（pc 0）到该写的
/// 路径，其间没有任何「最终 `Buffered`、目标表被该写触及」的 `INSERT ...
/// VALUES` 保持有效（该 INSERT 之后、该写之前没有把该表刷掉的点：
/// 同表非 `VALUES` 写 / `CallRoutine` / `SqlExecDynamic`）。
///
/// must-数据流：`input[n]` = 所有入口→n 路径上都仍处于守卫状态的表集
/// （到达 n 之前）。多表守卫取保守口径——到达写时守卫交集为空即视为
/// 存在无守卫路径。
fn unguarded_writes(
    code: &[IrStmt],
    succ: &[Vec<usize>],
    candidates: &[(usize, String)],
    immediate: &HashSet<usize>,
) -> HashSet<usize> {
    // 最终 Buffered 的候选：pc → 目标表
    let buffered: HashMap<usize, &str> = candidates
        .iter()
        .filter(|(pc, _)| !immediate.contains(pc))
        .map(|(pc, t)| (*pc, t.as_str()))
        .collect();
    let targets: HashSet<&str> = buffered.values().copied().collect();

    // 入口可达集
    let mut reachable = vec![false; code.len()];
    if code.is_empty() {
        return HashSet::new();
    }
    reachable[0] = true;
    let mut queue = VecDeque::from([0usize]);
    while let Some(pc) = queue.pop_front() {
        for &s in &succ[pc] {
            if s < code.len() && !reachable[s] {
                reachable[s] = true;
                queue.push_back(s);
            }
        }
    }

    // 可达节点的前驱表
    let mut preds = vec![Vec::new(); code.len()];
    for (pc, ss) in succ.iter().enumerate() {
        if !reachable[pc] {
            continue;
        }
        for &s in ss {
            if s < code.len() && reachable[s] {
                preds[s].push(pc);
            }
        }
    }

    // must 不动点：input[n] = ∩ out(p)（p 为 n 的可达前驱）；input[0] = ∅
    let mut input: Vec<HashSet<&str>> = vec![targets.clone(); code.len()];
    input[0] = HashSet::new();
    loop {
        let mut changed = false;
        for n in 1..code.len() {
            if !reachable[n] || preds[n].is_empty() {
                continue;
            }
            let mut acc: Option<HashSet<&str>> = None;
            for &p in &preds[n] {
                let out = guarded_transfer(code, p, &input[p], &buffered);
                acc = Some(match acc {
                    None => out,
                    Some(a) => a.intersection(&out).copied().collect(),
                });
            }
            let new_in = acc.unwrap_or_else(|| targets.clone());
            if new_in != input[n] {
                input[n] = new_in;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    code.iter()
        .enumerate()
        .filter_map(|(pc, st)| match st {
            IrStmt::SqlExec { sql, .. }
                if reachable[pc] && plain_insert_values(sql).is_none() && modifies_table(sql) =>
            {
                // 到达该写时，所有入口路径共有的守卫表中没有被本写触及的
                // → 存在无守卫路径 → 跨表写耦合
                let guarded = input[pc].iter().any(|t| touches_table(sql, t));
                (!guarded).then_some(pc)
            }
            _ => None,
        })
        .collect()
}

/// 节点 `pc` 的守卫转移：最终 `Buffered` 的候选 INSERT 把其目标表加入
/// 守卫集；`CallRoutine`/`SqlExecDynamic` 清空全部守卫；触及守卫表的非
/// `VALUES` 修改写清除对应表守卫；其余节点透传。
fn guarded_transfer<'a>(
    code: &'a [IrStmt],
    pc: usize,
    inn: &HashSet<&'a str>,
    buffered: &HashMap<usize, &'a str>,
) -> HashSet<&'a str> {
    if let Some(t) = buffered.get(&pc) {
        let mut s = inn.clone();
        s.insert(*t);
        return s;
    }
    match &code[pc] {
        IrStmt::CallRoutine { .. } | IrStmt::SqlExecDynamic { .. } => HashSet::new(),
        IrStmt::SqlExec { sql, .. }
            if plain_insert_values(sql).is_none() && modifies_table(sql) =>
        {
            inn.iter()
                .filter(|t| !touches_table(sql, t))
                .copied()
                .collect()
        }
        _ => inn.clone(),
    }
}

/// 红线 2：覆盖 `pc` 的任意 `PushTrap` 处理器体含非「空跳过」指令 → true。
/// 「空跳过」= 全部语句为 `Nop` 或纯帧内观察赋值（局部变量/记录字段，
/// 目标非 OUT/INOUT 形参，表达式无 `FuncCall`/`SqlFn`/`Lookup`）。
fn trap_handler_effects(code: &[IrStmt], pc: usize, out_params: &BTreeSet<String>) -> bool {
    for (i, stmt) in code.iter().enumerate() {
        let IrStmt::PushTrap { entries } = stmt else {
            continue;
        };
        if i >= pc {
            break;
        }
        let Some(pop) = trap_pop_pc(code, i) else {
            continue;
        };
        if pc >= pop {
            continue;
        }
        for e in entries {
            if code[e.body_start..e.body_end]
                .iter()
                .any(|s| !handler_stmt_skip_equivalent(s, out_params))
            {
                return true;
            }
        }
    }
    false
}

/// 处理器体单句是否「空跳过」等价：`Nop`，或写帧内槽位的纯赋值。
fn handler_stmt_skip_equivalent(stmt: &IrStmt, out_params: &BTreeSet<String>) -> bool {
    match stmt {
        IrStmt::Nop => true,
        IrStmt::Assign { target, expr } => {
            !assigns_out_param(target, out_params) && expr_is_frame_pure(expr)
        }
        _ => false,
    }
}

/// 赋值目标根变量是否 OUT/INOUT 形参（写形参会逃逸到调用方，非空跳过）。
fn assigns_out_param(target: &AssignTarget, out_params: &BTreeSet<String>) -> bool {
    if out_params.is_empty() {
        return false;
    }
    let name = match target {
        AssignTarget::Var(v) => &v.0,
        AssignTarget::Field { rec, .. } => &rec.0,
    };
    out_params.contains(name)
}

/// 表达式是否纯帧内可求值：`FuncCall`/`SqlFn`/`Lookup` 可能调例程、下发
/// SQL 或抛错，一律视为非纯；其余变体递归要求子表达式同样纯。
fn expr_is_frame_pure(expr: &IrExpr) -> bool {
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } | IrExpr::CursorAttr { .. } => {
            true
        }
        IrExpr::Bin { l, r, .. } => expr_is_frame_pure(l) && expr_is_frame_pure(r),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_deref().is_none_or(expr_is_frame_pure)
                && whens
                    .iter()
                    .all(|(w, e)| expr_is_frame_pure(w) && expr_is_frame_pure(e))
                && else_expr.as_deref().is_none_or(expr_is_frame_pure)
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_is_frame_pure(e),
        IrExpr::FuncCall { .. } | IrExpr::SqlFn { .. } | IrExpr::Lookup { .. } => false,
    }
}

/// `PushTrap` 在 `trap_pc` 的匹配 `PopTrap` 下标（按嵌套深度配对）。
fn trap_pop_pc(code: &[IrStmt], trap_pc: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (pc, stmt) in code.iter().enumerate().skip(trap_pc + 1) {
        match stmt {
            IrStmt::PushTrap { .. } => depth += 1,
            IrStmt::PopTrap => {
                if depth == 0 {
                    return Some(pc);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}
