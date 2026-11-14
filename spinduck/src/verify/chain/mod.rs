//! 链路引用抽取（纯函数，不触碰内核）：SQL 文本 → 对象引用，IR → 例程与 SQL。
//!
//! 关系位口径：静态近似扫描（不下内核、不解析 AST）。取 `FROM`/`JOIN`/`UPDATE`
//! 之后与 `INSERT|MERGE INTO` 之后的首个标识符；`EXTRACT/SUBSTRING/… FROM x`
//! 的 `FROM` 不是关系位（按括号属主排除）；名字紧跟左括号 = 表函数，跳过。
//! 已知误报面：把关系名写成函数调用的语料会被跳过，由场景执行兜住。
//! SQL 上下文例程调用下沉出的结果表（`FnSite::table`）是引擎运行期对象，
//! 不属于项目链路——按 IR 记录的名字排除，不靠命名模式猜测。

use crate::ir::{CursorSource, IrExpr, IrProg, IrStmt};
use std::collections::BTreeSet;

/// 链路对象类别（内核目录里的三种形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObjectKind {
    Relation,
    Sequence,
}

/// 一条对象引用（归一名 + 类别）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    /// 归一名：小写；`db.schema.name` 形态截尾两段
    pub name: String,
}

/// IR 一次线性遍历的采集结果（四个面：核验用三个，闭包多用一个候选面）。
#[derive(Default)]
struct Walk {
    /// 被调用的例程名（小写）
    refs: BTreeSet<String>,
    /// 表达式级函数调用的**全部**名字（小写，含裸名）：闭包按例程目录逐个解析，
    /// 命中才算被调（A2-5）。与 `refs` 分开是因为判定面不同：`refs` 是"确定是例程"，
    /// 这里是"候选"，裸名内建函数（`trunc`、`nvl`）永远落在候选而不是 `refs`。
    expr_names: BTreeSet<String>,
    /// SQL 文本，文档序
    texts: Vec<String>,
    /// 下沉结果表名：内核里不存在、也不该被核验的项目对象
    plan_tables: BTreeSet<String>,
}

fn walk(prog: &IrProg) -> Walk {
    let mut w = Walk::default();
    walk_code(&prog.code, &mut w);
    w
}

/// 程序内全部例程引用（链路断点判定输入）。
///
/// 与运行期分派同口径：`CALL`（裸名或全名）与**带包限定**的表达式函数调用计入；
/// 裸名 `FuncCall` 是内建/内核函数（解释器一律下内核求值），不计入链路。
/// 裸名可能是项目函数的那一面由 [`expr_func_names`] 承担，不在这里放宽。
pub fn routine_refs(prog: &IrProg) -> BTreeSet<String> {
    walk(prog).refs
}

/// 表达式级函数调用的全部名字（小写、去重、含裸名）——被调函数的候选面。
///
/// 抽出候选而非结论：裸名到底是内建函数还是项目函数，只有例程目录能判，而链路抽取
/// 是纯函数（不持目录）。消费方是 `plan::closure` 的 BFS：命中目录即把被调函数进闭包，
/// 未命中按内建函数丢弃（口径同 `runtime::eval::eval_funccall` 的单次精确查找）。
/// 带包限定的名字也在此面中，但那些已由 [`routine_refs`] 收走，消费方按点号跳过即可。
pub fn expr_func_names(prog: &IrProg) -> BTreeSet<String> {
    walk(prog).expr_names
}

/// 程序内全部链路对象引用（下沉结果表除外）。
pub fn chain_object_refs(prog: &IrProg) -> BTreeSet<ObjectRef> {
    let w = walk(prog);
    let mut out = BTreeSet::new();
    for text in &w.texts {
        out.extend(
            sql_object_refs(text)
                .into_iter()
                .filter(|r| !(r.kind == ObjectKind::Relation && w.plan_tables.contains(&r.name))),
        );
    }
    out
}

/// 线性遍历指令序列。
///
/// `PushTrap` / `PopTrap` 只是标记：处理器体与主语句同处一个 `code` 区
/// （`TrapEntry` 存的是索引区间），线性遍历已覆盖，递归展开会重复计数。
fn walk_code(code: &[IrStmt], w: &mut Walk) {
    for st in code {
        match st {
            IrStmt::Nop
            | IrStmt::PushTrap { .. }
            | IrStmt::Jump(_)
            | IrStmt::TxCommit
            | IrStmt::TxRollback
            | IrStmt::CloseCursor { .. }
            | IrStmt::Fetch { .. }
            | IrStmt::PopTrap => {}
            IrStmt::Assign { expr, .. }
            | IrStmt::JumpIfNot { cond: expr, .. }
            | IrStmt::JumpIf { cond: expr, .. }
            | IrStmt::Perform { expr } => walk_expr(expr, w),
            IrStmt::Raise {
                message: Some(expr),
            } => walk_expr(expr, w),
            IrStmt::Raise { message: None } => {}
            IrStmt::Return { value: Some(expr) } => walk_expr(expr, w),
            IrStmt::Return { value: None } => {}
            IrStmt::SqlQuery { sql, fn_sites, .. } | IrStmt::SqlExec { sql, fn_sites, .. } => {
                w.texts.push(sql.clone());
                walk_sites(fn_sites, w);
            }
            IrStmt::FoldedQuery { sql, expr, .. } => {
                w.texts.push(sql.clone());
                walk_expr(expr, w);
            }
            IrStmt::SqlExecDynamic { text_expr, .. } => walk_expr(text_expr, w),
            IrStmt::OpenCursor { source, .. } => match source {
                CursorSource::Static { sql, fn_sites, .. } => {
                    w.texts.push(sql.clone());
                    walk_sites(fn_sites, w);
                }
                CursorSource::Dynamic { query_expr } => walk_expr(query_expr, w),
            },
            IrStmt::CallRoutine { name, args, .. } => {
                w.refs.insert(name.to_ascii_lowercase());
                for a in args {
                    walk_expr(a, w);
                }
            }
        }
    }
}

/// SQL 上下文例程调用下沉点：名字进例程引用，元组查询进 SQL 文本，
/// 结果表名记入内部对象集合（供链路对象引用剔除）。
fn walk_sites(fn_sites: &[crate::sql::FnSite], w: &mut Walk) {
    for s in fn_sites {
        w.refs.insert(s.name.to_ascii_lowercase());
        w.texts.push(s.distinct_sql.clone());
        w.plan_tables.insert(s.table.to_ascii_lowercase());
    }
}

fn walk_expr(expr: &IrExpr, w: &mut Walk) {
    let mut stack = vec![expr];
    while let Some(x) = stack.pop() {
        match x {
            IrExpr::Const(_)
            | IrExpr::Var(_)
            | IrExpr::Field { .. }
            | IrExpr::CursorAttr { .. } => {}
            IrExpr::Bin { l, r, .. } => {
                stack.push(r);
                stack.push(l);
            }
            IrExpr::Case {
                operand,
                whens,
                else_expr,
            } => {
                stack.extend(operand.as_deref());
                for (condition, result) in whens {
                    stack.push(condition);
                    stack.push(result);
                }
                stack.extend(else_expr.as_deref());
            }
            IrExpr::Not(inner) | IrExpr::IsNull(inner) => stack.push(inner),
            IrExpr::FuncCall { name, args, .. } => {
                let lower = name.to_ascii_lowercase();
                if lower.contains('.') {
                    w.refs.insert(lower.clone());
                }
                w.expr_names.insert(lower);
                stack.extend(args.iter());
            }
            IrExpr::SqlFn { args, .. } => stack.extend(args.iter()),
        }
    }
}

/// SQL 文本里的链路对象引用（去重排序）。
///
/// crate 内可见：`plan` 的闭包用它把**视图体**的基表补进部署清单，与例程体同一条
/// 抽取管线，不开第二套词法。
pub(crate) fn sql_object_refs(sql: &str) -> BTreeSet<ObjectRef> {
    let lower = sql.to_ascii_lowercase();
    let toks = tokens(&lower);
    let mut out = sequence_calls(&lower);
    let mut owners: Vec<String> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let w = match t {
            Tok::Word(w) => w.as_str(),
            Tok::Punct('(') => {
                owners.push(match toks.get(i.saturating_sub(1)) {
                    Some(Tok::Word(prev)) => prev.clone(),
                    _ => String::new(),
                });
                continue;
            }
            Tok::Punct(')') => {
                owners.pop();
                continue;
            }
            Tok::Punct(_) => continue,
        };
        if let Some(seq) = w
            .strip_suffix(".nextval")
            .or_else(|| w.strip_suffix(".currval"))
        {
            out.insert(ObjectRef {
                kind: ObjectKind::Sequence,
                name: normalize(seq),
            });
            continue;
        }
        if !matches!(w, "from" | "join" | "update" | "into") {
            continue;
        }
        if !in_relation_slot(&toks, i, w, &owners) {
            continue;
        }
        let next = match toks.get(i + 1) {
            Some(Tok::Word(w)) => w.clone(),
            Some(Tok::Punct('(')) => {
                out.extend(subquery_tail_refs(&toks, i + 1));
                continue;
            }
            _ => continue,
        };
        let (name, end) = join_dotted(&toks, i + 1, next);
        if !is_object_name(&name) {
            continue;
        }
        if w != "into" && matches!(toks.get(end + 1), Some(Tok::Punct('('))) {
            continue; // 表函数（unnest/read_csv/…）
        }
        out.insert(ObjectRef {
            kind: ObjectKind::Relation,
            name: normalize(&name),
        });
        out.extend(comma_join_refs(&toks, end + 1));
    }
    out
}

/// 这个词元位上的 `from|join|update|into` 是不是真的关系位。
///
/// 两条排除各有一个实测来源：`SELECT … INTO v_x`（PL 变量赋值）不是 `INSERT INTO`，
/// 按关系处理会凭空多出一张表；`EXTRACT(unit FROM ts)` 一类函数属主后面的 `FROM` 是
/// 参数关键字，不是关系位。
fn in_relation_slot(toks: &[Tok], i: usize, w: &str, owners: &[String]) -> bool {
    if w == "into"
        && !matches!(
            prev_word(toks, i).as_deref(),
            Some("insert") | Some("merge")
        )
    {
        return false;
    }
    !(w == "from" && owners.last().map(|o| from_owner(o)).unwrap_or(false))
}

/// 关系位第一格就是子查询（`FROM (SELECT 1) x, t_b b`）时的逗号连表。
///
/// 括号对本身不是链路对象，故越过整对括号（连同其别名）继续找逗号；括号不配平就放弃，
/// 宁少不误。
fn subquery_tail_refs(toks: &[Tok], open: usize) -> Vec<ObjectRef> {
    match matching_paren(toks, open) {
        Some(close) => comma_join_refs(toks, close + 1),
        None => Vec::new(),
    }
}

/// 逗号连表里其余的关系名：从第一个关系名的**下一格**往后扫。
///
/// Oracle 语料用 `FROM a t, b s` 连表，`FROM` 只出现一次，故第一个名字之后的逗号也要当
/// 关系分隔符。三条停手条件保住"宁少不误"：别名只有紧跟逗号时才算别名（否则是
/// `WHERE`/`SET` 之类的子句词）、逗号后既不是名字也不是左括号即停、限定名拼回后仍要过
/// [`is_object_name`] 与表函数排除。列清单 `INSERT INTO t (a, b)` 的逗号在左括号之后，
/// 第一个停手条件就挡住了。
fn comma_join_refs(toks: &[Tok], after: usize) -> Vec<ObjectRef> {
    let mut out = Vec::new();
    let mut i = after;
    loop {
        if matches!(toks.get(i), Some(Tok::Word(_)))
            && matches!(toks.get(i + 1), Some(Tok::Punct(',')))
        {
            i += 1; // 别名
        }
        if !matches!(toks.get(i), Some(Tok::Punct(','))) {
            return out;
        }
        let mut k = i + 1;
        if matches!(toks.get(k), Some(Tok::Punct('('))) {
            // 逗号后的子查询：整对越过再找下一个逗号（`FROM a, (SELECT 1) x, b`）
            let Some(close) = matching_paren(toks, k) else {
                return out;
            };
            k = close + 1;
            if matches!(toks.get(k), Some(Tok::Word(_))) {
                k += 1; // 子查询别名
            }
            i = k;
            continue;
        }
        let first = match toks.get(k) {
            Some(Tok::Word(w)) => w.clone(),
            _ => return out,
        };
        let (name, end) = join_dotted(toks, k, first);
        if !is_object_name(&name) || matches!(toks.get(end + 1), Some(Tok::Punct('('))) {
            return out;
        }
        out.push(ObjectRef {
            kind: ObjectKind::Relation,
            name: normalize(&name),
        });
        i = end + 1;
    }
}

/// 与 `open` 位左括号配对的右括号下标（词元级；字符串与注释已由 [`tokens`] 消掉）。
fn matching_paren(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < toks.len() {
        match toks[i] {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 关系位名字：把被空格拆开的限定名拼回来（`bigfund . t_a` → `bigfund.t_a`）。
///
/// 改写器按 token 重排文本，限定名两侧会带空格；返回（拼接后的名字, 末位下标），
/// 末位下标供表函数判定继续往后看一位。
fn join_dotted(toks: &[Tok], at: usize, first: String) -> (String, usize) {
    let mut name = first;
    let mut end = at;
    while matches!(toks.get(end + 1), Some(Tok::Punct('.'))) {
        match toks.get(end + 2) {
            Some(Tok::Word(w)) => {
                name.push('.');
                name.push_str(w);
                end += 2;
            }
            _ => break,
        }
    }
    (name, end)
}

/// 改写形态的序列调用：`nextval('seq')` / `currval('seq')`。
///
/// 词法切分把字符串字面量整体丢弃，序列名只存在于引号内，所以在原文上单独取一次。
/// 代价是注释里被注释掉的 `nextval('…')` 也算一次引用（语料内无此形态，见 validation）。
/// 口径锚在 `patterns.rs::rewrite_sequence_nextval`：它把 `seq.NEXTVAL`
/// 统一改写成这个形态，链路抽取看到的是改写后的文本——只认 `.nextval` 后缀等于抽不到任何序列。
/// 入参须为小写文本（与 `sql_object_refs` 同一份）。
fn sequence_calls(lower: &str) -> BTreeSet<ObjectRef> {
    let mut out = BTreeSet::new();
    for kw in ["nextval('", "currval('"] {
        let mut at = 0usize;
        while let Some(hit) = lower[at..].find(kw) {
            let start = at + hit + kw.len();
            let Some(close) = lower[start..].find('\'') else {
                break;
            };
            let name = &lower[start..start + close];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$'))
            {
                out.insert(ObjectRef {
                    kind: ObjectKind::Sequence,
                    name: normalize(name),
                });
            }
            at = start + close;
        }
    }
    out
}

/// `… FROM x` 中 `x` 不是关系位的函数属主（`EXTRACT(unit FROM ts)` 等）。
fn from_owner(owner: &str) -> bool {
    matches!(
        owner,
        "extract" | "substring" | "position" | "trim" | "overlay"
    )
}

/// 归一：`db.schema.name` 截尾两段（与内核目录 `schema.name` 同口径）。
pub(crate) fn normalize(name: &str) -> String {
    let parts: Vec<&str> = name.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() > 2 {
        return format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]);
    }
    parts.join(".")
}

/// 可信的对象名：标识符形态，排除绑定占位符（`$v_x`）与数字字面量。
pub(crate) fn is_object_name(w: &str) -> bool {
    match w.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    w.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '#'))
}

fn prev_word(toks: &[Tok], i: usize) -> Option<String> {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if let Tok::Word(w) = &toks[j] {
            return Some(w.clone());
        }
    }
    None
}

pub(crate) enum Tok {
    Word(String),
    Punct(char),
}

/// 词法切分：字符串字面量与注释整体丢弃（其中的关键字不是关系位）。
pub(crate) fn tokens(text: &str) -> Vec<Tok> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i] as char;
        if c == '\'' {
            i += 1;
            while i < b.len() && b[i] as char != '\'' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '-' && b.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#') {
            let start = i;
            while i < b.len() {
                let d = b[i] as char;
                if d.is_ascii_alphanumeric() || matches!(d, '_' | '$' | '#' | '.') {
                    i += 1;
                } else {
                    break;
                }
            }
            let w = text[start..i].trim_end_matches('.');
            if !w.is_empty() {
                out.push(Tok::Word(w.to_string()));
            }
            continue;
        }
        if !c.is_whitespace() {
            out.push(Tok::Punct(c));
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests;
