//! E2 确定性调用物化的静态证明（V8 详设 `loop-parallel-v8/e2`）。
//!
//! `FuncCall`/`SqlFn` 表达式节点在满足下列条件时可进入 `G_pure`：
//! Worker 不执行例程本体，而是按 `(name, 实参值)` / `(sql, 绑定值)`
//! 探测主线程物化的 `CallSnapshot`；未命中经 `NeedKeys` 交权主线程
//! 补执行后重派（任意键依赖按需展开，无需静态 key_dag）。
//!
//! 准入双条件：
//! 1. **例程体纯度**（`routine_materializable`）：callee 传递闭包内
//!    只含无写、无事务、无游标、无动态 SQL、无 `Raise` 的指令；
//!    `SqlQuery`/`FoldedQuery` 只读放行（物化期在主线程实时执行，
//!    无需可快照化），`fn_sites` 非空保守拒绝。
//! 2. **表达式全物化**（`expr_materializable`）：`FuncCall` 仅当
//!    catalog 命中 `Function` 且例程可物化，或未命中且 `prepared`
//!    SQL 就绪（内建下沉）；`SqlFn` 要求 `prepared` 就绪；`Lookup`
//!    一律不物化。嵌套调用递归同规则。
//!
//! 事务锚定/非确定性审计由门禁 5（`proof::scan_*_tx`）在 analyze
//! 后续阶段覆盖——本模块只证"可物化"，不重复其判定。

use crate::catalog::{RoutineCatalog, RoutineKind};
use crate::ir::{IrExpr, IrProg, IrStmt};
use std::collections::BTreeSet;

/// 例程物化递归深度上限（防 catalog 环）。
const MAX_MAT_DEPTH: usize = 8;

/// 表达式是否可在 Worker 经 `CallSnapshot` 物化求值。
/// `Pure` 叶子（Const/Var/Field/Bin/Case/Not/IsNull）恒真；
/// `CursorAttr` 由 stmt 级审计单独整域拒绝，不在此判定。
pub(crate) fn expr_materializable(expr: &IrExpr, catalog: &RoutineCatalog, depth: usize) -> bool {
    if depth > MAX_MAT_DEPTH {
        return false;
    }
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } => true,
        IrExpr::Bin { l, r, .. } => {
            expr_materializable(l, catalog, depth) && expr_materializable(r, catalog, depth)
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_materializable(e, catalog, depth),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand
                .as_deref()
                .is_none_or(|e| expr_materializable(e, catalog, depth))
                && whens.iter().all(|(c, v)| {
                    expr_materializable(c, catalog, depth) && expr_materializable(v, catalog, depth)
                })
                && else_expr
                    .as_deref()
                    .is_none_or(|e| expr_materializable(e, catalog, depth))
        }
        IrExpr::SqlFn { prepared, args, .. } => {
            // eval_sqlfn 只用 prepared.binds（args 不参与求值），
            // 但 args 内的调用同样不会被执行——保守仍要求可物化，
            // 防止未来语义变化引入未审计副作用。
            prepared.is_ok()
                && args
                    .iter()
                    .all(|a| expr_materializable(a, catalog, depth + 1))
        }
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => {
            let args_ok = args
                .iter()
                .all(|a| expr_materializable(a, catalog, depth + 1));
            if !args_ok {
                return false;
            }
            match catalog.lookup(name) {
                // 目录命中 Function：例程体须可物化；prepared 仅
                // 影响"目录未命中"分派，但例程路径不使用它——
                // 保留 prepared 检查无害且与 builtin 分支对齐。
                Some(r) if r.kind == RoutineKind::Function => {
                    prepared.is_ok() && routine_materializable(&r.prog, catalog, depth + 1)
                }
                // Procedure 或其他类别：表达式语境运行时报错，
                // 交权保留原生错误语义。
                Some(_) => false,
                // 内建下沉：prepared SQL 就绪即可（名/SQL 的
                // 事务锚定审计归门禁 5）。
                None => prepared.is_ok(),
            }
        }
        // Lookup（记录字段回退查询）未纳入物化模型。
        IrExpr::Lookup { .. } | IrExpr::CursorAttr { .. } => false,
    }
}

/// 语句的表达式位是否全部可物化（与原 `exprs::stmt_needs_yield_expr`
/// 同位覆盖：Assign/Perform/JumpIf*/Raise/Return/SqlExecDynamic/
/// FoldedQuery/CallRoutine 参数）。
pub(crate) fn stmt_exprs_materializable(
    stmt: &IrStmt,
    catalog: &RoutineCatalog,
    depth: usize,
) -> bool {
    let exprs: Vec<&IrExpr> = match stmt {
        IrStmt::Assign { expr, .. }
        | IrStmt::Perform { expr }
        | IrStmt::FoldedQuery { expr, .. } => vec![expr],
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => vec![cond],
        IrStmt::Raise {
            message: Some(expr),
        }
        | IrStmt::Return { value: Some(expr) } => vec![expr],
        IrStmt::SqlExecDynamic { text_expr, .. } => vec![text_expr],
        IrStmt::CallRoutine { args, .. } => args.iter().collect(),
        // 其余指令变体不含可物化表达式（赋值/SQL/控制流等）。
        _ => Vec::new(),
    };
    exprs.iter().all(|e| expr_materializable(e, catalog, depth))
}

/// 例程体是否可物化：指令白名单 + 表达式递归 + CallRoutine 传递闭包。
/// 物化在主线程预热期实时执行 callee 全文——`SqlQuery` 等只读指令
/// 放行（其确定性由门禁 5 覆盖），一切写/事务/游标/动态/异常外抛
/// 一律拒绝。
pub(crate) fn routine_materializable(
    prog: &IrProg,
    catalog: &RoutineCatalog,
    depth: usize,
) -> bool {
    if depth > MAX_MAT_DEPTH {
        return false;
    }
    prog.code
        .iter()
        .all(|stmt| stmt_materializable(stmt, catalog, depth))
}

fn stmt_materializable(stmt: &IrStmt, catalog: &RoutineCatalog, depth: usize) -> bool {
    match stmt {
        IrStmt::Nop
        | IrStmt::Assign { .. }
        | IrStmt::Perform { .. }
        | IrStmt::Jump(_)
        | IrStmt::JumpIf { .. }
        | IrStmt::JumpIfNot { .. }
        | IrStmt::PushTrap { .. }
        | IrStmt::PopTrap
        | IrStmt::Return { .. } => stmt_exprs_materializable(stmt, catalog, depth),
        // 只读 SQL：物化期主线程实时执行；fn_sites 物化写临时表，
        // 逐键重复执行改变 sd_* 生命周期——保守拒绝。
        IrStmt::SqlQuery { fn_sites, .. } => {
            fn_sites.is_empty() && stmt_exprs_materializable(stmt, catalog, depth)
        }
        IrStmt::FoldedQuery { .. } => stmt_exprs_materializable(stmt, catalog, depth),
        // 嵌套例程调用：callee 须同样可物化（OUT 写回发生在
        // callee 帧内，主线程物化时按参数序完整捕获）。
        IrStmt::CallRoutine { name, .. } => catalog
            .lookup(name)
            .is_some_and(|r| routine_materializable(&r.prog, catalog, depth + 1)),
        // 写/事务/游标/动态/显式异常：一律不可物化。
        IrStmt::SqlExec { .. }
        | IrStmt::SqlExecDynamic { .. }
        | IrStmt::TxCommit
        | IrStmt::TxCommitIfWritten
        | IrStmt::TxRollback
        | IrStmt::OpenCursor { .. }
        | IrStmt::Fetch { .. }
        | IrStmt::CloseCursor { .. }
        | IrStmt::Raise { .. } => false,
    }
}

/// 门禁 7（$R_{prefix} \cap W_{loop} = \emptyset$）的读集补全：
/// `G_pure` 内经 `CallSnapshot` 物化的调用闭包所读表名。
///
/// 物化调用的结果按 `(name, 实参)` 缓存整轮循环——若 callee 体内
/// `SqlQuery`/`FoldedQuery`/`SqlFn`/内建 `prepared` SQL 读到的表被
/// 循环写（`W_loop`），后续同键行会命中陈旧物化值。此处把闭包读集
/// 并入 `R_prefix` 由不相交证明兜底；任何无法枚举读集的形态返回
/// `Err(())`，调用方必须保守拒绝（串行兜底），不得静默放行。
pub(crate) fn materialized_read_tables(
    prog: &IrProg,
    g_pure: &BTreeSet<usize>,
    catalog: &RoutineCatalog,
) -> Result<BTreeSet<String>, ()> {
    let mut out = BTreeSet::new();
    for &pc in g_pure {
        let Some(stmt) = prog.code.get(pc) else {
            return Err(());
        };
        for expr in stmt_exprs(stmt) {
            expr_read_tables(expr, catalog, &mut out, 0)?;
        }
    }
    Ok(out)
}

/// 例程体全量读集（物化闭包成员：白名单已保证只读，仍逐语句收集）。
fn prog_read_tables(
    prog: &IrProg,
    catalog: &RoutineCatalog,
    out: &mut BTreeSet<String>,
    depth: usize,
) -> Result<(), ()> {
    if depth > MAX_MAT_DEPTH {
        return Err(());
    }
    for stmt in &prog.code {
        match stmt {
            IrStmt::SqlQuery { sql, fn_sites, .. } => {
                if !fn_sites.is_empty() {
                    return Err(());
                }
                sql_read_tables(sql, out)?;
            }
            IrStmt::FoldedQuery { sql, .. } => sql_read_tables(sql, out)?,
            IrStmt::CallRoutine { name, args, .. } => {
                for a in args {
                    expr_read_tables(a, catalog, out, depth + 1)?;
                }
                let Some(r) = catalog.lookup(name) else {
                    return Err(());
                };
                prog_read_tables(&r.prog, catalog, out, depth + 1)?;
            }
            // 其余指令变体无 callee 读表，仅需扫描其表达式位。
            _ => {
                for expr in stmt_exprs(stmt) {
                    expr_read_tables(expr, catalog, out, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

/// 语句内全部表达式位（与 `stmt_exprs_materializable` 同位覆盖）。
fn stmt_exprs(stmt: &IrStmt) -> Vec<&IrExpr> {
    match stmt {
        IrStmt::Assign { expr, .. }
        | IrStmt::Perform { expr }
        | IrStmt::FoldedQuery { expr, .. } => vec![expr],
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => vec![cond],
        IrStmt::Raise {
            message: Some(expr),
        }
        | IrStmt::Return { value: Some(expr) } => vec![expr],
        IrStmt::SqlExecDynamic { text_expr, .. } => vec![text_expr],
        IrStmt::CallRoutine { args, .. } => args.iter().collect(),
        // 其余指令变体不含表达式位。
        _ => Vec::new(),
    }
}

fn expr_read_tables(
    expr: &IrExpr,
    catalog: &RoutineCatalog,
    out: &mut BTreeSet<String>,
    depth: usize,
) -> Result<(), ()> {
    if depth > MAX_MAT_DEPTH {
        return Err(());
    }
    match expr {
        IrExpr::Const(_) | IrExpr::Var(_) | IrExpr::Field { .. } | IrExpr::CursorAttr { .. } => {
            Ok(())
        }
        IrExpr::Bin { l, r, .. } => {
            expr_read_tables(l, catalog, out, depth)?;
            expr_read_tables(r, catalog, out, depth)
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => expr_read_tables(e, catalog, out, depth),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(e) = operand {
                expr_read_tables(e, catalog, out, depth)?;
            }
            for (c, v) in whens {
                expr_read_tables(c, catalog, out, depth)?;
                expr_read_tables(v, catalog, out, depth)?;
            }
            match else_expr {
                Some(e) => expr_read_tables(e, catalog, out, depth),
                None => Ok(()),
            }
        }
        IrExpr::SqlFn { prepared, args, .. } => {
            for a in args {
                expr_read_tables(a, catalog, out, depth + 1)?;
            }
            match prepared {
                Ok(s) => sql_read_tables(&s.sql, out),
                Err(_) => Err(()),
            }
        }
        IrExpr::FuncCall {
            name,
            args,
            prepared,
        } => {
            for a in args {
                expr_read_tables(a, catalog, out, depth + 1)?;
            }
            match catalog.lookup(name) {
                Some(r) if r.kind == RoutineKind::Function => {
                    prog_read_tables(&r.prog, catalog, out, depth + 1)
                }
                Some(_) => Err(()),
                None => match prepared {
                    Ok(sql) => sql_read_tables(sql, out),
                    Err(_) => Err(()),
                },
            }
        }
        // Lookup 未纳入物化模型——出现在纯子图属口径漂移，保守拒绝。
        IrExpr::Lookup { .. } => Err(()),
    }
}

/// 单条 SQL 文本的读表集合。优先 `shape::recognize`（权威点查形态）；
/// 其余 SELECT 退化为 token 扫描——收集 `FROM`/`JOIN`/`,` 之后的
/// 标识符（含子查询内层、逗号连接表），列名/别名误收只会造成
/// 过度拒绝（安全方向），绝不漏收真表。无法分词 → `Err(())`。
fn sql_read_tables(sql: &str, out: &mut BTreeSet<String>) -> Result<(), ()> {
    if let Some(plan) = crate::lookup_cache::shape::recognize(sql) {
        out.extend(plan.tables.iter().cloned());
        return Ok(());
    }
    use ogsql_parser::token::tokenizer::Tokenizer;
    let tokens = Tokenizer::new(sql).tokenize().map_err(|_| ())?;
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|t| sql.get(t.span.start..t.span.end))
        .map(|s| s.to_ascii_lowercase())
        .collect();
    // 引号标识符（`"lut"`）首字符非字母/下划线会被下方收集器静默
    // 跳过 → 漏收真表（假阴性，危险方向）。引擎产 SQL 可能输出
    // 引号形式，无法安全归一化时整体保守拒绝（过度拒绝安全）。
    if words.iter().any(|w| w.starts_with('"')) {
        return Err(());
    }
    const STOP: &[&str] = &[
        "select",
        "from",
        "where",
        "group",
        "order",
        "having",
        "limit",
        "offset",
        "on",
        "using",
        "as",
        "join",
        "inner",
        "left",
        "right",
        "full",
        "cross",
        "natural",
        "union",
        "except",
        "intersect",
        "fetch",
        "for",
        "and",
        "or",
        "not",
        "in",
        "is",
        "between",
        "like",
    ];
    let mut i = 0usize;
    while i < words.len() {
        let triggered = matches!(words[i].as_str(), "from" | "join" | ",");
        if !triggered {
            i += 1;
            continue;
        }
        // 逗号只在 FROM 子句内才是表分隔——`SELECT a, b` 的逗号会
        // 误触发；此时下一个词是标识符/`(`，收进来只造成过度拒绝，
        // 安全方向可接受。
        i += 1;
        while i < words.len() {
            let w = words[i].as_str();
            if w == "(" {
                i += 1;
                continue;
            }
            if w == ")" || w == "." {
                i += 1;
                continue;
            }
            if STOP.contains(&w) {
                break;
            }
            // 数字/运算符等非标识符词跳过（不 break，防 WHERE 前列名截断）。
            if w.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                out.insert(w.to_string());
            }
            i += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非 shape 点查形态的 SELECT 走 token 扫描兜底：裸表名正常收集。
    #[test]
    fn should_collect_unquoted_table_names() {
        let mut out = BTreeSet::new();
        assert!(sql_read_tables("SELECT a, b FROM lut WHERE k = 1", &mut out).is_ok());
        assert!(out.contains("lut"), "读表集合: {out:?}");
    }

    /// 引号标识符（`"lut"`）无法安全归一化 → 保守 `Err(())`：
    /// 漏收真表是危险方向（假阴性放行 LP → CallSnapshot 陈旧读），
    /// 过度拒绝安全（PR#62 复审 P2-2）。
    #[test]
    fn should_reject_quoted_identifiers_conservatively() {
        // 非点查形态（聚合）强制走 token 扫描兜底。
        let mut out = BTreeSet::new();
        assert!(
            sql_read_tables("SELECT COUNT(*) FROM \"lut\" GROUP BY k", &mut out).is_err(),
            "引号标识符必须保守拒绝: {out:?}"
        );
    }
}
