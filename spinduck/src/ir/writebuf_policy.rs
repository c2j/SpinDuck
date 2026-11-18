//! V3-6 写缓冲编译期策略（设计 §3.6）：IR 编译完成后对**全部**例程统一运行，
//! 与是否走集合化无关。默认 `BufferPolicy::Immediate`（安全失败兜底）；
//! 仅「纯 `INSERT ... VALUES`」且通过 CFG 安全扫描的 `SqlExec` 升级 `Buffered`。
//!
//! 红线（任一命中即保持 `Immediate`）：
//! 1. 从该 INSERT 出发、到达确定刷盘屏障之前的全部可达路径上，存在修改
//!    其他表的 DML（全局屏障：`CallRoutine` / `SqlExecDynamic` / `Return` /
//!    `TxRollback`；同表屏障：触及同表的 `SqlQuery` / `FoldedQuery` / 非
//!    `VALUES` `SqlExec`——`OpenCursor`/`Fetch`/`CloseCursor`/`TxCommit`/
//!    `Jump*` 不是屏障，扫描必须穿过）；
//! 2. 包裹该 INSERT 的任意活跃 `PushTrap` 处理器体不是「空跳过」（含副作用
//!    或控制流跳转）。

use super::types::{BufferPolicy, IrProg, IrStmt};
use std::collections::{HashSet, VecDeque};

/// 编译入口统一调用：就地写回每条 `SqlExec` 的 `buffer_policy`。
pub(crate) fn apply(prog: &mut IrProg) {
    let succ = successors(&prog.code);
    for pc in 0..prog.code.len() {
        let Some(table) = insert_target(&prog.code[pc]) else {
            continue;
        };
        if scan_clean(&prog.code, &succ, pc, &table) {
            if let IrStmt::SqlExec { buffer_policy, .. } = &mut prog.code[pc] {
                *buffer_policy = BufferPolicy::Buffered;
            }
        }
    }
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

/// 红线 1 + 全局/同表屏障：BFS 全部可达路径，遇屏障即该路径终止；
/// 任一可达「修改其他表的 DML」→ false（保持 Immediate）。
fn scan_clean(code: &[IrStmt], succ: &[Vec<usize>], start: usize, table: &str) -> bool {
    if trap_handler_effects(code, start) {
        return false;
    }
    let mut seen = vec![false; code.len()];
    let mut queue: VecDeque<usize> = succ[start].iter().copied().collect();
    while let Some(pc) = queue.pop_front() {
        if pc >= code.len() || seen[pc] {
            continue;
        }
        seen[pc] = true;
        match &code[pc] {
            // 确定全局刷盘屏障：路径终止，不展开
            IrStmt::CallRoutine { .. }
            | IrStmt::SqlExecDynamic { .. }
            | IrStmt::Return { .. }
            | IrStmt::TxRollback => {}
            IrStmt::SqlQuery { sql, .. } | IrStmt::FoldedQuery { sql, .. } => {
                if !touches_table(sql, table) {
                    queue.extend(succ[pc].iter().copied());
                }
            }
            IrStmt::SqlExec { sql, .. } => {
                if let Some(t) = plain_insert_values(sql) {
                    if t != table {
                        return false; // 改其他表的 INSERT ... VALUES
                    }
                    queue.extend(succ[pc].iter().copied());
                } else if touches_table(sql, table) {
                    // 同表非 VALUES 语句：确定刷盘点，路径终止
                } else if modifies_table(sql) {
                    return false; // 跨表 DML
                } else {
                    queue.extend(succ[pc].iter().copied());
                }
            }
            // OpenCursor/Fetch/CloseCursor/TxCommit/Jump*/Assign/Nop/PushTrap/
            // PopTrap 等一律不是屏障，继续扫描
            _ => queue.extend(succ[pc].iter().copied()),
        }
    }
    true
}

/// 红线 2：覆盖 `pc` 的任意 `PushTrap` 处理器体含非 `Nop` 指令 → true。
fn trap_handler_effects(code: &[IrStmt], pc: usize) -> bool {
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
                .any(|s| !matches!(s, IrStmt::Nop))
            {
                return true;
            }
        }
    }
    false
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
