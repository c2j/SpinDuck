//! 区域证明义务：前缀干净度、写集/读集、读写不交、出口活跃（效果准入与
//! trap 体结构识别见 [`super::traps`]）。

use super::{CursorRegion, SkipReason};
use crate::ir::{AssignTarget, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use std::collections::BTreeSet;

/// BatchAtomic 入口事务干净度（§4.5 第 1 项）：前缀至区域入口无未提交 DML。
/// 仅沿正常路径扫描；异常处理器体（体内跳转不落入区域内部）整体跳过；
/// 跳过处理器的前向跳转放行，其余分支保守拒绝。
pub(crate) fn prefix_is_clean(prog: &IrProg, region: &CursorRegion) -> Result<(), SkipReason> {
    let handlers = prefix_handler_ranges(prog, region)?;
    let in_handler = |pc: usize| handlers.iter().any(|(s, e)| pc >= *s && pc < *e);
    let mut dirty = false;
    for (pc, stmt) in prog.code[..region.open_pc].iter().enumerate() {
        if in_handler(pc) {
            continue;
        }
        match stmt {
            IrStmt::TxCommit | IrStmt::TxRollback => dirty = false,
            IrStmt::SqlExec { .. } | IrStmt::SqlExecDynamic { .. } | IrStmt::CallRoutine { .. } => {
                dirty = true;
            }
            IrStmt::Jump(to) => {
                if *to <= pc || *to > region.open_pc {
                    return Err(SkipReason::UnsupportedControlFlow);
                }
                for (i, stmt) in prog.code[pc + 1..*to].iter().enumerate() {
                    let p = pc + 1 + i;
                    if in_handler(p) {
                        continue;
                    }
                    // 退出型处理器后的收尾跳转是不可达死代码：仅放行
                    // 与本跳转同目标的前向 Jump（无效果、同汇合点）。
                    if matches!(stmt, IrStmt::Jump(t) if t == to) {
                        continue;
                    }
                    return Err(SkipReason::UnsupportedControlFlow);
                }
            }
            IrStmt::JumpIf { .. } | IrStmt::JumpIfNot { .. } => {
                return Err(SkipReason::UnsupportedControlFlow);
            }
            _ => {}
        }
    }
    if dirty {
        Err(SkipReason::TransactionBoundaryInsideLoop)
    } else {
        Ok(())
    }
}

/// 前缀效果准入：允许异常块（处理器体完全位于前缀且体内跳转不落入
/// 区域内部，见 prefix_handler_ranges），其余调用/动态 SQL/正常路径
/// RAISE 仍拒绝。
pub(crate) fn reject_prefix_effects(
    prog: &IrProg,
    region: &CursorRegion,
) -> Result<(), SkipReason> {
    let handlers = prefix_handler_ranges(prog, region)?;
    let in_handler = |pc: usize| handlers.iter().any(|(s, e)| pc >= *s && pc < *e);
    for (pc, stmt) in prog.code[..region.open_pc].iter().enumerate() {
        if in_handler(pc) {
            continue;
        }
        match stmt {
            IrStmt::PushTrap { .. } | IrStmt::PopTrap => {}
            IrStmt::CallRoutine { .. } => {
                return Err(SkipReason::UnsupportedCall);
            }
            IrStmt::Raise { .. } => return Err(SkipReason::ExceptionScopePresent),
            IrStmt::SqlExecDynamic { .. }
            | IrStmt::OpenCursor {
                source: CursorSource::Dynamic { .. },
                ..
            } => return Err(SkipReason::DynamicSql),
            _ => {}
        }
    }
    Ok(())
}

/// 收集前缀内全部 PushTrap 的处理器体区间。处理器体须完全位于前缀、
/// 末指令为 Return/Raise/无条件 Jump（顺序落入区域入口的 fallthrough
/// 形态不可证，保守拒绝），且体内跳转不得落入区域内部（open_pc <
/// target <= close_pc）：落点为 open_pc（正常进区域）或区域外（留在
/// 前缀/跳入后缀）均由解释器按原 PC 语义处理；落入区域内部会绕过
/// 批写入口证明，保守拒绝（ExceptionScopePresent）。
fn prefix_handler_ranges(
    prog: &IrProg,
    region: &CursorRegion,
) -> Result<Vec<(usize, usize)>, SkipReason> {
    let mut ranges = Vec::new();
    for stmt in &prog.code[..region.open_pc] {
        let IrStmt::PushTrap { entries } = stmt else {
            continue;
        };
        for e in entries {
            if e.body_start >= e.body_end || e.body_end > region.open_pc {
                return Err(SkipReason::ExceptionScopePresent);
            }
            if !matches!(
                prog.code.get(e.body_end - 1),
                Some(IrStmt::Return { .. }) | Some(IrStmt::Raise { .. }) | Some(IrStmt::Jump(_))
            ) {
                return Err(SkipReason::ExceptionScopePresent);
            }
            for st in &prog.code[e.body_start..e.body_end] {
                let target = match st {
                    IrStmt::Jump(t) => Some(*t),
                    IrStmt::JumpIf { to, .. } | IrStmt::JumpIfNot { to, .. } => Some(*to),
                    _ => None,
                };
                if target.is_some_and(|t| t > region.open_pc && t <= region.close_pc) {
                    return Err(SkipReason::ExceptionScopePresent);
                }
            }
            ranges.push((e.body_start, e.body_end));
        }
    }
    Ok(ranges)
}

/// 区域写集：提取区域内全部 DML 目标表名（小写归一）。
/// 动态 SQL / 非 NoOp 例程调用无法静态确定写集，保守拒绝。
pub(crate) fn write_set(
    prog: &IrProg,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<BTreeSet<String>, SkipReason> {
    let mut writes = BTreeSet::new();
    // §3.5-4：INSERT 跳起型处理器在 bad_rows==0 准入下不可达，其 callee
    // 写集不进入区域写集（退回路径由解释器自行维护写集）。
    let log_bodies = super::insert_log_bodies(prog, region);
    for (offset, stmt) in prog.code[region.open_pc..=region.close_pc]
        .iter()
        .enumerate()
    {
        match stmt {
            IrStmt::SqlExec { sql, .. } => {
                if let Some(table) = dml_target(sql) {
                    writes.insert(table);
                }
            }
            IrStmt::SqlExecDynamic { .. } => return Err(SkipReason::DynamicSql),
            IrStmt::CallRoutine {
                name,
                args,
                out_targets,
            } => {
                let pc = region.open_pc + offset;
                let noop = super::traps::in_handler_body(prog, region, pc)
                    && super::super::catalog_summary::call_is_noop(
                        catalog,
                        prog,
                        name,
                        args,
                        out_targets,
                    );
                let logged = log_bodies.iter().any(|(lo, hi)| pc >= *lo && pc < *hi);
                if !noop && !logged {
                    return Err(SkipReason::UnsupportedCall);
                }
            }
            _ => {}
        }
    }
    Ok(writes)
}

/// 从 DML 文本提取目标表名（INSERT INTO / UPDATE / DELETE FROM / MERGE INTO）。
fn dml_target(sql: &str) -> Option<String> {
    use ogsql_parser::token::tokenizer::Tokenizer;
    let tokens = Tokenizer::new(sql).tokenize().ok()?;
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|t| sql.get(t.span.start..t.span.end))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            "insert" | "merge" => {
                if words.get(i + 1).is_some_and(|w| w == "into") {
                    return words.get(i + 2).cloned();
                }
            }
            "update" => {
                return words.get(i + 1).cloned();
            }
            "delete" if words.get(i + 1).is_some_and(|w| w == "from") => {
                return words.get(i + 2).cloned();
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 游标查询读集：提取 OPEN 语句中 SELECT 的 FROM 表名。
pub(crate) fn read_set(
    prog: &IrProg,
    region: &CursorRegion,
) -> Result<BTreeSet<String>, SkipReason> {
    let Some(IrStmt::OpenCursor { source, .. }) = prog.code.get(region.open_pc) else {
        return Err(SkipReason::UnsupportedControlFlow);
    };
    let CursorSource::Static { sql, .. } = source else {
        return Err(SkipReason::DynamicSql);
    };
    let mut reads = BTreeSet::new();
    if let Some(table) = select_from_target(sql) {
        reads.insert(table);
    }
    Ok(reads)
}

/// 读写不交校验：Reads(cursor) ∩ Writes(region) = ∅。
pub(crate) fn reads_writes_disjoint(
    prog: &IrProg,
    region: &CursorRegion,
    catalog: &crate::catalog::RoutineCatalog,
) -> Result<(), SkipReason> {
    let reads = read_set(prog, region)?;
    let writes = write_set(prog, region, catalog)?;
    if reads.intersection(&writes).next().is_some() {
        return Err(SkipReason::TargetReadDependency);
    }
    Ok(())
}

/// 从 SELECT 文本提取 FROM 后的首个表名。
pub(crate) fn select_from_target(sql: &str) -> Option<String> {
    use ogsql_parser::token::tokenizer::Tokenizer;
    let tokens = Tokenizer::new(sql).tokenize().ok()?;
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|t| sql.get(t.span.start..t.span.end))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let from_pos = words.iter().position(|w| w == "from")?;
    words.get(from_pos + 1).cloned()
}

/// 循环不变变量证明（§4.4 B1）：变量在区域体（含处理器）内无任何赋值
/// （Assign/Fetch/SqlQuery INTO）即为循环不变，可作静态过滤绑定单值携带。
pub(crate) fn var_invariant_in_region(prog: &IrProg, region: &CursorRegion, var: &VarName) -> bool {
    for stmt in &prog.code[region.open_pc..=region.close_pc] {
        let assigned = match stmt {
            IrStmt::Assign {
                target: AssignTarget::Var(v),
                ..
            } => v == var,
            IrStmt::Fetch { targets, .. } | IrStmt::SqlQuery { into: targets, .. } => targets
                .iter()
                .any(|t| matches!(t, AssignTarget::Var(v) if v == var)),
            _ => false,
        };
        if assigned {
            return false;
        }
    }
    true
}

pub(crate) fn collect_stmt_reads(stmt: &IrStmt, out: &mut BTreeSet<VarName>) {
    match stmt {
        IrStmt::Assign { expr, .. } => collect_expr_reads(expr, out),
        IrStmt::SqlExec { binds, .. } | IrStmt::SqlQuery { binds, .. } => {
            for (_, src) in binds {
                match src {
                    crate::ir::BindSrc::Var(v) => {
                        out.insert(v.clone());
                    }
                    crate::ir::BindSrc::Field { rec, .. } => {
                        out.insert(rec.clone());
                    }
                    // Expr 绑定不含帧变量读（其基字段属游标记录，非 IR 变量）。
                    crate::ir::BindSrc::Expr { .. } => {}
                }
            }
        }
        IrStmt::JumpIf { cond, .. } | IrStmt::JumpIfNot { cond, .. } => {
            collect_expr_reads(cond, out)
        }
        _ => {}
    }
}

fn collect_expr_reads(expr: &IrExpr, out: &mut BTreeSet<VarName>) {
    match expr {
        IrExpr::Var(v) => {
            out.insert(v.clone());
        }
        IrExpr::Field { rec, .. } => {
            out.insert(rec.clone());
        }
        IrExpr::Bin { l, r, .. } => {
            collect_expr_reads(l, out);
            collect_expr_reads(r, out);
        }
        IrExpr::Not(e) | IrExpr::IsNull(e) => collect_expr_reads(e, out),
        IrExpr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(e) = operand {
                collect_expr_reads(e, out);
            }
            for (w, v) in whens {
                collect_expr_reads(w, out);
                collect_expr_reads(v, out);
            }
            if let Some(e) = else_expr {
                collect_expr_reads(e, out);
            }
        }
        IrExpr::FuncCall { args, .. } => {
            for a in args {
                collect_expr_reads(a, out);
            }
        }
        _ => {}
    }
}

/// 前缀写集枚举：区域入口之前全部语句（含嵌入 fn_site 与例程调用
/// 递归展开）的 DML 目标表（小写归一）+ 不可枚举副作用标记。
/// 视图物化探测前移到前缀执行之前（engine/set_oriented.rs），要求
/// 该集合与全部视图底表不相交且无 opaque——否则探测时点与物化
/// 时点数据可能不一致，探测结论失效。
pub(crate) fn prefix_write_set(
    prog: &IrProg,
    open_pc: usize,
    catalog: &crate::catalog::RoutineCatalog,
) -> (BTreeSet<String>, bool) {
    let mut out = WriteScan::default();
    scan_stmt_writes(&prog.code[..open_pc], catalog, &mut Vec::new(), &mut out);
    (out.writes, out.opaque)
}

/// 例程写集：fn_site 物化前导调用的例程按同口径枚举（整个 prog
/// 扫描，保守超集——调用可达任意写点均计入）。
pub(crate) fn routine_write_set(
    catalog: &crate::catalog::RoutineCatalog,
    name: &str,
) -> (BTreeSet<String>, bool) {
    let mut out = WriteScan::default();
    scan_routine_writes(catalog, name, &mut Vec::new(), &mut out);
    (out.writes, out.opaque)
}

#[derive(Default)]
struct WriteScan {
    writes: BTreeSet<String>,
    /// 动态 SQL / DDL / 非常规语句 / 未解析或递归例程调用——写集
    /// 无法静态枚举，调用方须按保守口径拒绝依赖数据不变的证明。
    opaque: bool,
}

fn scan_stmt_writes(
    stmts: &[IrStmt],
    catalog: &crate::catalog::RoutineCatalog,
    visiting: &mut Vec<String>,
    out: &mut WriteScan,
) {
    for stmt in stmts {
        match stmt {
            IrStmt::SqlExec { sql, fn_sites, .. } => {
                match classify_exec(sql) {
                    ExecClass::Write(t) => {
                        // 限定名同时登记末段：视图底表以未限定名登记，
                        // `s.t` 与 `t` 须命中同一相交判定。
                        if let Some(seg) = t.rsplit('.').next() {
                            if seg != t {
                                out.writes.insert(seg.to_string());
                            }
                        }
                        out.writes.insert(t);
                    }
                    ExecClass::Read => {}
                    ExecClass::Opaque => out.opaque = true,
                }
                scan_sites(fn_sites, catalog, visiting, out);
            }
            IrStmt::SqlQuery { fn_sites, .. } => {
                scan_sites(fn_sites, catalog, visiting, out);
            }
            IrStmt::OpenCursor {
                source: CursorSource::Static { fn_sites, .. },
                ..
            } => {
                scan_sites(fn_sites, catalog, visiting, out);
            }
            IrStmt::OpenCursor {
                source: CursorSource::Dynamic { .. },
                ..
            } => out.opaque = true,
            IrStmt::SqlExecDynamic { .. } => out.opaque = true,
            IrStmt::CallRoutine { name, .. } => {
                scan_routine_writes(catalog, name, visiting, out);
            }
            _ => {}
        }
    }
}

fn scan_sites(
    sites: &[crate::sql::FnSite],
    catalog: &crate::catalog::RoutineCatalog,
    visiting: &mut Vec<String>,
    out: &mut WriteScan,
) {
    for s in sites {
        scan_routine_writes(catalog, &s.name, visiting, out);
    }
}

fn scan_routine_writes(
    catalog: &crate::catalog::RoutineCatalog,
    name: &str,
    visiting: &mut Vec<String>,
    out: &mut WriteScan,
) {
    let key = name.to_ascii_lowercase();
    if visiting.contains(&key) {
        out.opaque = true;
        return;
    }
    let Some(routine) = catalog.lookup(name) else {
        out.opaque = true;
        return;
    };
    visiting.push(key);
    scan_stmt_writes(&routine.prog.code, catalog, visiting, out);
    visiting.pop();
}

/// SqlExec 首词分类：DML 目标表 / 只读 / 不可静态判定（DDL、SET、
/// WITH..DML 等一律 Opaque——宁可多拒不可漏写）。
enum ExecClass {
    Write(String),
    Read,
    Opaque,
}

fn classify_exec(sql: &str) -> ExecClass {
    use ogsql_parser::token::tokenizer::Tokenizer;
    let Ok(tokens) = Tokenizer::new(sql).tokenize() else {
        return ExecClass::Opaque;
    };
    // 全 token 文本（小写）：ident = 首字符为字母（关键字同为 ident，
    // 由结构校验排除）；括号/点/运算符保留以判定相邻关系。
    let toks: Vec<String> = tokens
        .iter()
        .filter_map(|t| sql.get(t.span.start..t.span.end))
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let is_ident = |i: usize| {
        toks.get(i)
            .is_some_and(|t| t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()))
    };
    // 限定名抽取：ident 起，按 "." ident 续接（"s.t" 单 token 形态天然覆盖）。
    // 返回 (全名, 下一 token 下标)；起点非 ident → None。
    let qualified = |mut i: usize| -> Option<(String, usize)> {
        if !is_ident(i) {
            return None;
        }
        let mut name = toks[i].clone();
        i += 1;
        while toks.get(i).is_some_and(|t| t == ".") && is_ident(i + 1) {
            name.push('.');
            name.push_str(&toks[i + 1]);
            i += 2;
        }
        Some((name, i))
    };
    let keyword = |i: usize, kw: &str| toks.get(i).is_some_and(|t| t == kw);
    match toks.first().map(String::as_str) {
        Some("insert") | Some("merge") => {
            // INSERT [ALL] INTO / MERGE INTO：ALL 多表插入保守拒绝。
            if !keyword(1, "into") {
                return ExecClass::Opaque;
            }
            qualified(2)
                .map(|(t, _)| ExecClass::Write(t))
                .unwrap_or(ExecClass::Opaque)
        }
        Some("update") => {
            // UPDATE <表> SET：目标后必须紧跟 SET——`UPDATE (子查询)`、
            // `UPDATE ONLY t` 等非常规形态一律 Opaque。
            match qualified(1) {
                Some((t, i)) if keyword(i, "set") => ExecClass::Write(t),
                _ => ExecClass::Opaque,
            }
        }
        Some("delete") => {
            // DELETE FROM <表> [WHERE|USING]：目标后仅允许子句关键字或语句尾。
            if !keyword(1, "from") {
                return ExecClass::Opaque;
            }
            match qualified(2) {
                Some((t, i)) if i >= toks.len() || keyword(i, "where") || keyword(i, "using") => {
                    ExecClass::Write(t)
                }
                _ => ExecClass::Opaque,
            }
        }
        Some("select") => {
            // SELECT ... INTO <表> 也是写；其余只读。
            if toks.iter().any(|t| t == "into") {
                ExecClass::Opaque
            } else {
                ExecClass::Read
            }
        }
        Some("values") | Some("table") => ExecClass::Read,
        _ => ExecClass::Opaque,
    }
}

#[cfg(test)]
mod tests {
    use super::super::live_out_check;
    use super::super::loop_prog;
    use super::*;
    use crate::ir::BufferPolicy;

    fn region(prog: &IrProg) -> CursorRegion {
        super::super::extract(prog).unwrap()
    }

    #[test]
    fn should_accept_clean_prefix_ending_with_commit() {
        let mut prog = loop_prog(2, 0);
        prog.code[0] = IrStmt::SqlExec {
            sql: "DELETE FROM t".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
            buffer_policy: BufferPolicy::Immediate,
        };
        prog.code[1] = IrStmt::TxCommit;
        assert_eq!(prefix_is_clean(&prog, &region(&prog)), Ok(()));
    }

    #[test]
    fn should_reject_dirty_prefix_without_commit() {
        let mut prog = loop_prog(1, 0);
        prog.code[0] = IrStmt::SqlExec {
            sql: "INSERT INTO t VALUES (1)".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
            buffer_policy: BufferPolicy::Immediate,
        };
        assert_eq!(
            prefix_is_clean(&prog, &region(&prog)),
            Err(SkipReason::TransactionBoundaryInsideLoop)
        );
    }

    #[test]
    fn should_accept_empty_prefix() {
        let prog = loop_prog(0, 0);
        assert_eq!(prefix_is_clean(&prog, &region(&prog)), Ok(()));
    }

    #[test]
    fn should_reject_prefix_with_branch() {
        let mut prog = loop_prog(1, 0);
        prog.code[0] = IrStmt::Jump(0);
        assert_eq!(
            prefix_is_clean(&prog, &region(&prog)),
            Err(SkipReason::UnsupportedControlFlow)
        );
    }

    #[test]
    fn should_accept_prefix_with_only_reads() {
        let mut prog = loop_prog(1, 0);
        prog.code[0] = IrStmt::SqlQuery {
            sql: "SELECT 1".into(),
            binds: Vec::new(),
            into: vec![AssignTarget::Var(VarName("v".into()))],
            fn_sites: Vec::new(),
        };
        assert_eq!(prefix_is_clean(&prog, &region(&prog)), Ok(()));
    }

    #[test]
    fn should_accept_prefix_cleaned_by_rollback() {
        let mut prog = loop_prog(2, 0);
        prog.code[0] = IrStmt::SqlExec {
            sql: "INSERT INTO t VALUES (1)".into(),
            binds: Vec::new(),
            fn_sites: Vec::new(),
            buffer_policy: BufferPolicy::Immediate,
        };
        prog.code[1] = IrStmt::TxRollback;
        assert_eq!(prefix_is_clean(&prog, &region(&prog)), Ok(()));
    }

    #[test]
    fn should_reject_prefix_with_call_routine() {
        let mut prog = loop_prog(1, 0);
        prog.code[0] = IrStmt::CallRoutine {
            name: "some_proc".into(),
            args: Vec::new(),
            out_targets: Vec::new(),
        };
        assert_eq!(
            prefix_is_clean(&prog, &region(&prog)),
            Err(SkipReason::TransactionBoundaryInsideLoop)
        );
    }

    #[test]
    fn should_extract_write_set_from_insert() {
        let prog = loop_prog(0, 0);
        let region = super::super::extract(&prog).unwrap();
        let writes = write_set(&prog, &region, &Default::default()).unwrap();
        assert!(writes.contains("dst"));
        assert_eq!(writes.len(), 1);
    }

    #[test]
    fn should_return_empty_write_set_for_read_only_region() {
        let mut prog = loop_prog(0, 0);
        prog.code[3] = IrStmt::SqlQuery {
            sql: "SELECT a FROM src".into(),
            binds: Vec::new(),
            into: vec![AssignTarget::Var(VarName("v".into()))],
            fn_sites: Vec::new(),
        };
        let region = super::super::extract(&prog).unwrap();
        let writes = write_set(&prog, &region, &Default::default()).unwrap();
        assert!(writes.is_empty());
    }

    #[test]
    fn should_reject_dynamic_sql_in_write_set() {
        let mut prog = loop_prog(0, 0);
        prog.code[3] = IrStmt::SqlExecDynamic {
            text_expr: Box::new(IrExpr::Const(crate::value::SpinValue::Str(
                "INSERT INTO t VALUES (1)".into(),
            ))),
            into_targets: Vec::new(),
        };
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            write_set(&prog, &region, &Default::default()),
            Err(SkipReason::DynamicSql)
        );
    }

    #[test]
    fn should_extract_read_set_from_cursor_query() {
        let prog = loop_prog(0, 0);
        let region = super::super::extract(&prog).unwrap();
        let reads = read_set(&prog, &region).unwrap();
        assert!(reads.contains("src"));
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn should_pass_disjoint_when_read_write_tables_differ() {
        let prog = loop_prog(0, 0);
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            reads_writes_disjoint(&prog, &region, &Default::default()),
            Ok(())
        );
    }

    #[test]
    fn should_reject_disjoint_when_cursor_reads_write_target() {
        let mut prog = loop_prog(0, 0);
        if let IrStmt::OpenCursor { source, .. } = &mut prog.code[0] {
            *source = CursorSource::Static {
                sql: "SELECT a FROM dst".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
            };
        }
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            reads_writes_disjoint(&prog, &region, &Default::default()),
            Err(SkipReason::TargetReadDependency)
        );
    }

    #[test]
    fn should_pass_live_out_when_no_suffix_reads() {
        let prog = loop_prog(0, 0);
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(live_out_check(&prog, &region), Ok(()));
    }

    #[test]
    fn should_reject_live_out_when_suffix_reads_assigned_var() {
        let mut prog = loop_prog(0, 1);
        prog.code[3] = IrStmt::Assign {
            target: AssignTarget::Var(VarName("v".into())),
            expr: IrExpr::Const(crate::value::SpinValue::Int(1)),
        };
        prog.code[6] = IrStmt::Assign {
            target: AssignTarget::Var(VarName("w".into())),
            expr: IrExpr::Var(VarName("v".into())),
        };
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(
            live_out_check(&prog, &region),
            Err(SkipReason::LiveOutState)
        );
    }

    #[test]
    fn should_pass_live_out_when_suffix_reads_unassigned_var() {
        let mut prog = loop_prog(0, 1);
        prog.code[3] = IrStmt::Assign {
            target: AssignTarget::Var(VarName("v".into())),
            expr: IrExpr::Const(crate::value::SpinValue::Int(1)),
        };
        prog.code[6] = IrStmt::Assign {
            target: AssignTarget::Var(VarName("w".into())),
            expr: IrExpr::Var(VarName("other".into())),
        };
        let region = super::super::extract(&prog).unwrap();
        assert_eq!(live_out_check(&prog, &region), Ok(()));
    }

    #[test]
    fn should_classify_simple_dml_targets() {
        assert!(matches!(
            classify_exec("insert into t values (1)"),
            ExecClass::Write(ref t) if t == "t"
        ));
        assert!(matches!(
            classify_exec("update t set a = 1"),
            ExecClass::Write(ref t) if t == "t"
        ));
        assert!(matches!(
            classify_exec("delete from t where a = 1"),
            ExecClass::Write(ref t) if t == "t"
        ));
        assert!(matches!(
            classify_exec("merge into s.t using u on (1=1) when matched then update set a=1"),
            ExecClass::Write(ref t) if t == "s.t"
        ));
        assert!(matches!(classify_exec("select * from t"), ExecClass::Read));
        assert!(matches!(classify_exec("values (1)"), ExecClass::Read));
    }

    #[test]
    fn should_classify_qualified_target_with_last_segment() {
        let prog = {
            let mut p = loop_prog(1, 0);
            p.code[0] = IrStmt::SqlExec {
                sql: "insert into sch.t values (1)".into(),
                binds: Vec::new(),
                fn_sites: Vec::new(),
                buffer_policy: BufferPolicy::Immediate,
            };
            p
        };
        let (writes, opaque) = prefix_write_set(&prog, 1, &Default::default());
        assert!(!opaque);
        assert!(writes.contains("sch.t"));
        assert!(writes.contains("t"));
    }

    #[test]
    fn should_reject_unusual_dml_shapes_as_opaque() {
        // UPDATE (子查询) / UPDATE ONLY / DELETE ONLY / SELECT INTO /
        // WITH..DML / INSERT ALL：目标不可静态枚举 → opaque。
        for sql in [
            "update (select a from t) x set a = 1",
            "update only t set a = 1",
            "delete from only t where a = 1",
            "select a into t from s",
            "with x as (select 1) insert into t select * from x",
            "insert all into t values (1) into u values (2)",
            "delete t from t join u on (1=1)",
        ] {
            assert!(
                matches!(classify_exec(sql), ExecClass::Opaque),
                "{sql} 应判 Opaque"
            );
        }
    }
}
