//! `src/verify/chain/` 单测：SQL 文本与 IR 的链路引用抽取口径（含逗号连表、注释、表函数）。
//!
//! 从 `chain.rs` 原样搬出（规模上限，AGENTS §3），断言与夹具未改一字。

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
fn should_take_every_relation_in_a_comma_separated_from_list() {
    // 考题语料的连表主形态是逗号（`FROM par_fund_rate_info t, par_drv_config s`）。
    // 只取 `FROM` 后第一个名字，后半边的表对链路就是隐形的：verify 不核它、计划闭包
    // 也不收它，部署出来的库里因此没有这张表（V30.4 实测：语料 4 张视图退化成占位视图）。
    assert_eq!(
        names("SELECT * FROM t_a a, t_b b WHERE a.id = b.id"),
        vec!["Relation:t_a", "Relation:t_b"]
    );
    assert_eq!(
        names("SELECT * FROM t_a , bigfund . t_b WHERE 1 = 1"),
        vec!["Relation:bigfund.t_b", "Relation:t_a"],
        "无别名 + 改写器打印的空格限定名"
    );
    assert_eq!(
        names("SELECT * FROM (SELECT 1) x, t_b b"),
        vec!["Relation:t_b"],
        "逗号后的子查询整对越过，其别名不当成关系名"
    );
    // 负例：括号内与其他子句里的逗号都不是关系分隔符
    assert_eq!(
        names("SELECT * FROM t_a WHERE id IN (1, 2) AND k IN (SELECT k FROM t_b)"),
        vec!["Relation:t_a", "Relation:t_b"]
    );
    assert_eq!(
        names("SELECT * FROM (SELECT a, b FROM t_z) w"),
        vec!["Relation:t_z"],
        "子查询投影列的逗号不能被当成关系分隔符"
    );
    assert_eq!(
        names("INSERT INTO t_a (a, b) VALUES (1, 2)"),
        vec!["Relation:t_a"],
        "列清单的逗号不在关系位"
    );
}

#[test]
fn should_join_qualified_names_printed_with_spaces_around_the_dot() {
    // 改写器按 token 重排文本，`bigfund.t_a` 打印成 `bigfund . t_a`；
    // 关系位若只取下一个 token，会把属主当成对象名（核验因此报出一个不存在的关系）。
    assert_eq!(
        names("delete from bigfund . t_a where id = 1"),
        vec!["Relation:bigfund.t_a"]
    );
    assert_eq!(
        names("insert into bigfund . t_a select 1"),
        vec!["Relation:bigfund.t_a"]
    );
    assert_eq!(
        names("select * from bigfund . unnest ( l ) x"),
        Vec::<String>::new(),
        "拼接后仍要走表函数排除"
    );
}

#[test]
fn should_ignore_names_inside_sql_comments() {
    // 注释不是 SQL 文本：行注释里的 `from t_comment` 若进链路，核验会去内核找一张
    // 不存在的表（DDL 扫描同理，`/*FORCE*/` 会把视图声明挡住）。
    assert_eq!(
        names("SELECT 1 FROM t_real -- from t_comment"),
        vec!["Relation:t_real"]
    );
    assert_eq!(
        names("SELECT 1 /* from t_block */ FROM t_real"),
        vec!["Relation:t_real"]
    );
}

#[test]
fn should_collect_sequences_from_the_rewritten_nextval_form() {
    // 链路抽取看到的是**改写后**的文本：`patterns.rs::rewrite_sequence_nextval` 把
    // `seq.NEXTVAL` 变成 `nextval('seq')`，所以只认后缀形态等于永远抽不到序列。
    // 词法切分会丢弃字符串字面量，序列名只能单独取。
    assert_eq!(
        names("INSERT INTO t_a ( id ) VALUES ( nextval('seq_b') )"),
        vec!["Relation:t_a", "Sequence:seq_b"]
    );
    assert_eq!(
        names("SELECT CURRVAL('Seq_Log') FROM sys_dummy"),
        vec!["Relation:sys_dummy", "Sequence:seq_log"]
    );
    assert_eq!(
        names("SELECT nextval FROM t"),
        vec!["Relation:t"],
        "裸 nextval（无实参）不是序列引用"
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
fn should_report_bare_expression_calls_as_candidates_not_as_routine_refs() {
    // A2-5 的两面分工：`routine_refs` 只给"确定是例程"的名字（裸名一律不算，否则每个
    // 内建函数都成假缺件）；`expr_func_names` 把表达式里的调用名**全**交出去，
    // 由调用方拿例程目录逐个判定。这条断言锁住"裸名只进候选、不进结论"。
    let prog = IrProg {
        code: vec![assign(IrExpr::FuncCall {
            name: "trunc".into(),
            args: vec![],
        })],
    };
    assert!(routine_refs(&prog).is_empty(), "裸名不是例程引用");
    let got: Vec<String> = expr_func_names(&prog).into_iter().collect();
    assert_eq!(got, vec!["trunc"]);

    // 实参里再套一层调用也要收（`pkg_a.fn_c(fnc_hidden())`）：候选面是表达式子树的并集
    let nested = IrProg {
        code: vec![assign(IrExpr::FuncCall {
            name: "PKG_A.fn_c".into(),
            args: vec![IrExpr::FuncCall {
                name: "fnc_hidden".into(),
                args: vec![],
            }],
        })],
    };
    let got: Vec<String> = expr_func_names(&nested).into_iter().collect();
    assert_eq!(got, vec!["fnc_hidden", "pkg_a.fn_c"], "名字一律小写归一");
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
