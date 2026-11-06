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

/// IR 一次线性遍历的采集结果（链路核验的三个输入面）。
#[derive(Default)]
struct Walk {
    /// 被调用的例程名（小写）
    refs: BTreeSet<String>,
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
pub fn routine_refs(prog: &IrProg) -> BTreeSet<String> {
    walk(prog).refs
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
            | IrStmt::Perform { expr } => walk_expr(expr, &mut w.refs),
            IrStmt::Raise {
                message: Some(expr),
            } => walk_expr(expr, &mut w.refs),
            IrStmt::Raise { message: None } => {}
            IrStmt::Return { value: Some(expr) } => walk_expr(expr, &mut w.refs),
            IrStmt::Return { value: None } => {}
            IrStmt::SqlQuery { sql, fn_sites, .. } | IrStmt::SqlExec { sql, fn_sites, .. } => {
                w.texts.push(sql.clone());
                walk_sites(fn_sites, w);
            }
            IrStmt::SqlExecDynamic { text_expr, .. } => walk_expr(text_expr, &mut w.refs),
            IrStmt::OpenCursor { source, .. } => match source {
                CursorSource::Static { sql, fn_sites, .. } => {
                    w.texts.push(sql.clone());
                    walk_sites(fn_sites, w);
                }
                CursorSource::Dynamic { query_expr } => walk_expr(query_expr, &mut w.refs),
            },
            IrStmt::CallRoutine { name, args, .. } => {
                w.refs.insert(name.to_ascii_lowercase());
                for a in args {
                    walk_expr(a, &mut w.refs);
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

fn walk_expr(expr: &IrExpr, refs: &mut BTreeSet<String>) {
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
            IrExpr::Not(inner) | IrExpr::IsNull(inner) => stack.push(inner),
            IrExpr::FuncCall { name, args } => {
                if name.contains('.') {
                    refs.insert(name.to_ascii_lowercase());
                }
                stack.extend(args.iter());
            }
            IrExpr::SqlFn { args, .. } => stack.extend(args.iter()),
        }
    }
}

/// SQL 文本里的链路对象引用（去重排序）。
fn sql_object_refs(sql: &str) -> BTreeSet<ObjectRef> {
    let toks = tokens(&sql.to_ascii_lowercase());
    let mut out = BTreeSet::new();
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
        if w == "into"
            && !matches!(
                prev_word(&toks, i).as_deref(),
                Some("insert") | Some("merge")
            )
        {
            continue;
        }
        if w == "from" && owners.last().map(|o| from_owner(o)).unwrap_or(false) {
            continue;
        }
        let next = match toks.get(i + 1) {
            Some(Tok::Word(w)) => w.clone(),
            _ => continue,
        };
        if !is_object_name(&next) {
            continue;
        }
        if w != "into" && matches!(toks.get(i + 2), Some(Tok::Punct('('))) {
            continue; // 表函数（unnest/read_csv/…）
        }
        out.insert(ObjectRef {
            kind: ObjectKind::Relation,
            name: normalize(&next),
        });
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
fn normalize(name: &str) -> String {
    let parts: Vec<&str> = name.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() > 2 {
        return format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]);
    }
    parts.join(".")
}

/// 可信的对象名：标识符形态，排除绑定占位符（`$v_x`）与数字字面量。
fn is_object_name(w: &str) -> bool {
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

enum Tok {
    Word(String),
    Punct(char),
}

/// 词法切分：字符串字面量整体丢弃（其中的关键字不是关系位）。
fn tokens(text: &str) -> Vec<Tok> {
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
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<String> {
        sql_object_refs(text)
            .into_iter()
            .map(|r| format!("{:?}:{}", r.kind, r.name))
            .collect()
    }

    #[test]
    fn should_collect_relations_and_sequences_from_sql_text() {
        assert_eq!(
            names("SELECT * FROM T_ST_A a WHERE EXISTS (SELECT 1 FROM V_B)"),
            vec!["Relation:t_st_a", "Relation:v_b"]
        );
        assert_eq!(
            names("INSERT INTO T_LOG SELECT SEQ_AMT.NEXTVAL, X FROM T_SRC"),
            vec!["Relation:t_log", "Relation:t_src", "Sequence:seq_amt"]
        );
    }

    #[test]
    fn should_skip_subqueries_table_functions_and_normalize_schema_prefix() {
        assert_eq!(names("SELECT * FROM (SELECT 1) t"), Vec::<String>::new());
        assert_eq!(
            names("SELECT * FROM UNNEST(L) x"),
            Vec::<String>::new(),
            "表函数（名字紧跟左括号）不是链路对象"
        );
        assert_eq!(
            names("DELETE FROM FASTAAS.T_ST_B"),
            vec!["Relation:fastaas.t_st_b"]
        );
        assert_eq!(
            names("UPDATE GAUSSDB.T_A a SET a.k = 1"),
            vec!["Relation:gaussdb.t_a"]
        );
        assert_eq!(
            names("SELECT SEQ_X.CURRVAL FROM T"),
            vec!["Relation:t", "Sequence:seq_x"]
        );
    }

    use crate::ir::{AssignTarget, CursorSource, IrExpr, IrProg, IrStmt, VarName};
    use crate::sql::FnSite;
    use crate::value::SpinValue;

    fn assign(expr: IrExpr) -> IrStmt {
        IrStmt::Assign {
            target: AssignTarget::Var(VarName("x".into())),
            expr,
        }
    }

    #[test]
    fn should_collect_routine_refs_from_ir_and_skip_bare_builtins() {
        let prog = IrProg {
            code: vec![
                IrStmt::CallRoutine {
                    name: "PKG_A.PRC_B".into(),
                    args: vec![],
                    out_targets: vec![],
                },
                assign(IrExpr::FuncCall {
                    name: "pkg_a.fn_c".into(),
                    args: vec![IrExpr::Const(SpinValue::Int(1))],
                }),
                assign(IrExpr::FuncCall {
                    name: "trunc".into(),
                    args: vec![],
                }),
                IrStmt::SqlQuery {
                    sql: "SELECT 1".into(),
                    binds: vec![],
                    into: vec![],
                    fn_sites: vec![FnSite {
                        table: "t".into(),
                        name: "pkg_d.fn_e".into(),
                        args: vec![],
                        distinct_sql: "SELECT k FROM T_B".into(),
                        binds: vec![],
                    }],
                },
            ],
        };
        let got: Vec<String> = routine_refs(&prog).into_iter().collect();
        assert_eq!(got, vec!["pkg_a.fn_c", "pkg_a.prc_b", "pkg_d.fn_e"]);
    }

    #[test]
    fn should_collect_sql_texts_from_statements_cursors_and_fn_sites() {
        let prog = IrProg {
            code: vec![
                IrStmt::SqlExec {
                    sql: "DELETE FROM T_A".into(),
                    binds: vec![],
                    fn_sites: vec![],
                },
                IrStmt::OpenCursor {
                    cursor: VarName("c".into()),
                    source: CursorSource::Static {
                        sql: "SELECT k FROM T_C".into(),
                        binds: vec![],
                        fn_sites: vec![],
                    },
                },
                IrStmt::SqlQuery {
                    sql: "SELECT 1".into(),
                    binds: vec![],
                    into: vec![],
                    fn_sites: vec![FnSite {
                        table: "t".into(),
                        name: "pkg_d.fn_e".into(),
                        args: vec![],
                        distinct_sql: "SELECT k FROM T_B".into(),
                        binds: vec![],
                    }],
                },
            ],
        };
        assert_eq!(
            walk(&prog).texts,
            vec![
                "DELETE FROM T_A",
                "SELECT k FROM T_C",
                "SELECT 1",
                "SELECT k FROM T_B"
            ],
            "语句 SQL / 静态游标 SQL / 下沉元组查询都进链路"
        );
    }

    #[test]
    fn should_walk_exception_handler_bodies() {
        // 处理器体与主语句同在 `code` 区（TrapEntry 只存索引），线性遍历即覆盖；
        // PushTrap 若再递归一遍，链路文本会重复计数
        let prog = IrProg {
            code: vec![
                IrStmt::PushTrap {
                    entries: vec![crate::ir::TrapEntry {
                        conds: vec!["no_data_found".into()],
                        body_start: 3,
                        body_end: 4,
                    }],
                },
                IrStmt::SqlQuery {
                    sql: "SELECT 1 FROM T_BODY".into(),
                    binds: vec![],
                    into: vec![],
                    fn_sites: vec![],
                },
                IrStmt::PopTrap,
                IrStmt::SqlExec {
                    sql: "DELETE FROM T_HANDLER".into(),
                    binds: vec![],
                    fn_sites: vec![],
                },
            ],
        };
        assert_eq!(
            walk(&prog).texts,
            vec!["SELECT 1 FROM T_BODY", "DELETE FROM T_HANDLER"],
            "按文档序各计一次：处理器体不得漏采，也不得重复"
        );
    }

    #[test]
    fn should_keep_insert_target_even_when_followed_by_column_list() {
        // `INSERT INTO t (a,b)` 的左括号是列清单，不是函数调用
        assert_eq!(
            names("INSERT INTO T_LOG (A, B) VALUES ($v_a, $v_b)"),
            vec!["Relation:t_log"]
        );
    }

    #[test]
    fn should_exclude_lowering_plan_tables_from_object_refs() {
        // SQL 上下文例程调用下沉后，语句里会出现引擎内部结果表（`FnSite::table`）：
        // 它由运行期临时建立，不属项目链路，核验不得报它"缺 DDL"
        let prog = IrProg {
            code: vec![IrStmt::SqlExec {
                sql: "UPDATE T_A t SET t.c = (SELECT sd_v FROM sd_fn0 WHERE sd_a0 IS NOT DISTINCT FROM (t.k))".into(),
                binds: vec![],
                fn_sites: vec![FnSite {
                    table: "sd_fn0".into(),
                    name: "pkg_d.fn_e".into(),
                    args: vec![],
                    distinct_sql: "SELECT DISTINCT k FROM T_B".into(),
                    binds: vec![],
                }],
            }],
        };
        let got: Vec<String> = chain_object_refs(&prog)
            .into_iter()
            .map(|r| format!("{:?}:{}", r.kind, r.name))
            .collect();
        assert_eq!(
            got,
            vec!["Relation:t_a", "Relation:t_b"],
            "链路表与下沉元组查询的源表计入，结果表本身不计入"
        );
    }

    #[test]
    fn should_ignore_from_positions_inside_string_literals_and_extract() {
        assert_eq!(
            names("SELECT 'FROM not_a_table' AS lit FROM T_REAL"),
            vec!["Relation:t_real"]
        );
        assert_eq!(
            names("SELECT EXTRACT(DAY FROM d_create) FROM T_D"),
            vec!["Relation:t_d"]
        );
        assert_eq!(
            names("SELECT count(*) FROM T A JOIN T_B B ON A.k = B.k"),
            vec!["Relation:t", "Relation:t_b"]
        );
    }
}
