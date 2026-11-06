// 解释器测试模块（自 interp.rs 拆分，行为不变，AGENTS §3）。
use super::*;
use crate::exec::BindMap;
use crate::ir::{compile_block_with, VarName};
use crate::rules::RuleRegistry;
use crate::runtime::Frame;
use crate::value::SpinValue;
use ogsql_parser::ast::plpgsql::{
    PlBlock, PlCursorDecl, PlDataType, PlDeclaration, PlStatement, PlVarDecl,
};
use ogsql_parser::parser::Parser;
use ogsql_parser::token::tokenizer::Tokenizer;

fn parse_statement(sql: &str) -> ogsql_parser::ast::Statement {
    let tokens = Tokenizer::new(sql).tokenize().unwrap();
    Parser::new(tokens).parse().remove(0)
}

fn ident(name: &str) -> ogsql_parser::ast::Ident {
    ogsql_parser::ast::Ident {
        value: name.to_string(),
        quote_style: None,
    }
}

fn var(name: &str) -> ogsql_parser::ast::Expr {
    ogsql_parser::ast::Expr::PlVariable(vec![ident(name)])
}

fn assign(target: ogsql_parser::ast::Expr, e: ogsql_parser::ast::Expr) -> PlStatement {
    PlStatement::Assignment {
        target,
        expression: e,
    }
}

/// 端到端探针（真实内核）：静态游标 OPEN/FETCH/EXIT 循环 + 记录字段访问。
/// 对应清算过程主循环形态（§7.3），行为口径对齐 scripts/sim_prc_trd_hz_byfund.py。
/// M6 端到端（真实内核）：Engine 注册 PACKAGE BODY 式过程 → CALL（OUT 写回）
/// → 过程间互调（CALL 另一过程）→ SQL 内 PL 函数经 UDF 桥接（threads=1）。
#[test]
fn should_call_procs_with_out_params_and_udf_bridge_on_real_kernel() {
    use crate::engine::Engine;
    use crate::exec::TraceNoopSink;
    use crate::ir::VarName;
    use crate::runtime::Frame;
    use crate::value::SpinValue;

    let mut eng = Engine::new(
        Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.register("CREATE TABLE dat(a VARCHAR, n INT)").unwrap();
    eng.register("INSERT INTO dat VALUES ('x', 2)").unwrap();
    eng.register("INSERT INTO dat VALUES ('y', 3)").unwrap();
    // 函数：SQL 内将被桥接（threads=1 约束下直接 UDF 通道）
    eng.register(
        "CREATE OR REPLACE FUNCTION fnc_double(p IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN p * 2; END;",
    )
    .unwrap();
    // 过程：IN/OUT + 调用函数（SQL 内 UDF 桥接路径）
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_calc(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN p_o := p_i * 2; END;",
    )
    .unwrap();
    // 过程：CALL 另一过程（ProcedureCall 互调）
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_fwd(p_i IN NUMBER, p_o OUT NUMBER) IS \
             BEGIN prc_calc(p_i, p_o); END;",
    )
    .unwrap();

    let r = eng
        .call("prc_fwd", &[SpinValue::Int(21), SpinValue::Null])
        .unwrap();
    assert_eq!(
        r.outs.get("p_o"),
        Some(&SpinValue::Int(42)),
        "OUT 写回: {:?}",
        r.outs
    );
    // 函数 PL 直调（Engine::call → RETURN 值）
    let fr = eng.call("fnc_double", &[SpinValue::Int(21)]).unwrap();
    assert_eq!(fr.return_value, Some(SpinValue::Int(42)));
    let _ = TraceNoopSink;
    let _ = Frame::new();
    let _ = VarName("x".into());
}

/// 端到端探针：显式游标空集 + EXIT WHEN %NOTFOUND + 记录引用（语料
/// prc_trd_hz_byfund 风险金循环形态）：空集时循环体一次都不执行。
#[test]
fn should_skip_loop_body_when_cursor_is_empty() {
    use crate::engine::Engine;
    use crate::rules::RuleRegistry;

    let mut eng = Engine::new(
        Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute("CREATE TABLE venture(a VARCHAR)", &BindMap::new())
        .unwrap();
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_empty IS \
             DECLARE \
               CURSOR c_v IS SELECT a FROM venture WHERE 1 = 0; \
               r c_v%ROWTYPE; \
               v VARCHAR2(10) := ''; \
             BEGIN \
               OPEN c_v; \
               LOOP \
                 FETCH c_v INTO r; \
                 EXIT WHEN c_v%NOTFOUND; \
                 v := 'x' || r.a; \
               END LOOP; \
               CLOSE c_v; \
             END;",
    )
    .unwrap();
    let out = eng.call("prc_empty", &[]).unwrap();
    assert_eq!(
        out.outs.len(),
        0,
        "空集不应进入循环体（否则 r 未赋值即报错）"
    );
}

/// 端到端探针：SQL 上下文例程调用下沉（§8.3 ②）——UPDATE/SELECT 中的
/// PL 函数按参数元组缓存化，INSERT VALUES 行内求值；结果必须与逐行语义一致。
#[test]
fn should_execute_pl_function_calls_inside_sql_statements() {
    use crate::engine::Engine;
    use crate::rules::RuleRegistry;
    use crate::value::SpinValue;

    let mut eng = Engine::new(
        Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let ex = &mut eng.ex;
    ex.execute("CREATE TABLE items(k VARCHAR, v INT)", &BindMap::new())
        .unwrap();
    ex.execute(
        "CREATE TABLE lookup_tbl(k VARCHAR, tag VARCHAR)",
        &BindMap::new(),
    )
    .unwrap();
    ex.execute(
        "INSERT INTO items VALUES ('a', 1), ('b', 2)",
        &BindMap::new(),
    )
    .unwrap();
    ex.execute(
        "INSERT INTO lookup_tbl VALUES ('a', 'A'), ('b', 'B')",
        &BindMap::new(),
    )
    .unwrap();
    eng.register(
        "CREATE OR REPLACE FUNCTION fnc_tag(p VARCHAR2) RETURN VARCHAR2 IS \
             v VARCHAR2(10); \
             BEGIN \
               SELECT tag INTO v FROM lookup_tbl WHERE k = p; \
               RETURN v; \
             END;",
    )
    .unwrap();
    // 集合形态：UPDATE ... SET v = length(fnc(k)) * 10
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_upd IS \
             BEGIN \
               UPDATE items SET v = length(fnc_tag(k)) * 10 WHERE v = 1; \
               COMMIT; \
             END;",
    )
    .unwrap();
    eng.call("prc_upd", &[]).unwrap();
    // 行内形态：INSERT ... VALUES (v, length(fnc(v)) * 100)
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_ins IS \
             DECLARE vk VARCHAR2(10) := 'a'; \
             BEGIN \
               INSERT INTO items(k, v) VALUES (vk, length(fnc_tag(vk)) * 100); \
               COMMIT; \
             END;",
    )
    .unwrap();
    eng.call("prc_ins", &[]).unwrap();

    let mut rs = eng
        .ex
        .query("SELECT k, v FROM items ORDER BY k, v", &BindMap::new())
        .unwrap();
    let rows: Vec<Vec<String>> = rs
        .fetch_many(10)
        .iter()
        .map(|r| r.iter().map(|v| v.as_str_loose()).collect())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec!["a".to_string(), "10".to_string()],
            vec!["a".to_string(), "100".to_string()],
            vec!["b".to_string(), "2".to_string()],
        ],
        "UPDATE 命中行按 fnc 结果更新、INSERT 行内求值；未命中行不变"
    );
    let _ = SpinValue::Null;
}

/// 端到端探针：@标记函数整体下沉（SqlFn）——嵌套日期算术的中间类型
/// （TIMESTAMP）保留在 SQL 侧，不经 SpinValue 文本化（语料 step4 形态）。
#[test]
fn should_evaluate_nested_date_functions_via_sqlfn_on_real_kernel() {
    use crate::engine::Engine;
    use crate::rules::RuleRegistry;
    use crate::value::SpinValue;

    let rules = RuleRegistry::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )
    .expect("加载种子规则");
    let mut eng = Engine::new(
        Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
        rules,
    );
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_datefn(p_o OUT VARCHAR2) IS \
             DECLARE v_days NUMBER(3) := 90; \
             BEGIN \
               p_o := to_char(to_date('20250715', 'YYYYMMDD') + v_days, 'YYYYMMDD'); \
             END;",
    )
    .unwrap();
    let r = eng.call("prc_datefn", &[SpinValue::Null]).unwrap();
    assert_eq!(
        r.outs.get("p_o"),
        Some(&SpinValue::Str("20251013".into())),
        "20250715 + 90 天 = 20251013"
    );
}

/// 端到端探针：`rownum = 1` 首行谓词改写为 LIMIT 1（语料 42 处，§9 规则）。
/// 覆盖两种语料形态：平铺 WHERE 与"内层 ORDER BY 子查询 + 外层 rownum"。
#[test]
fn should_rewrite_rownum_first_row_predicate_on_real_kernel() {
    use crate::engine::Engine;
    use crate::rules::RuleRegistry;
    use crate::value::SpinValue;

    // patterns_rownum_first_row 默认即 true（DuckDB 无 rownum，不改写必然报错）
    let rules = RuleRegistry::default();
    let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
    ex.execute("CREATE TABLE t(k VARCHAR, v INT)", &BindMap::new())
        .unwrap();
    for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
        ex.execute(
            &format!("INSERT INTO t VALUES ('{k}', {v})"),
            &BindMap::new(),
        )
        .unwrap();
    }
    let mut eng = Engine::new(Box::new(ex), rules);

    // 形态 1：平铺 WHERE rownum = 1（有序 LIMIT 语义）
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_first(p_o OUT VARCHAR) IS \
             BEGIN SELECT k INTO p_o FROM t WHERE rownum = 1 ORDER BY k; END;",
    )
    .unwrap();
    let r = eng.call("prc_first", &[SpinValue::Null]).unwrap();
    assert_eq!(r.outs.get("p_o"), Some(&SpinValue::Str("a".into())));

    // 形态 2：语料典型——内层 ORDER BY 子查询 + 外层 rownum = 1
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_top(p_o OUT INT) IS \
             BEGIN SELECT v INTO p_o FROM (SELECT v FROM t ORDER BY v DESC) WHERE rownum = 1; \
             END;",
    )
    .unwrap();
    let r = eng.call("prc_top", &[SpinValue::Null]).unwrap();
    assert_eq!(
        r.outs.get("p_o"),
        Some(&SpinValue::Int(3)),
        "内层 DESC 首行"
    );
}

/// 端到端探针：PL 上下文的内建函数下沉内核（ABS/SUBSTR/COALESCE/ROUND）。
#[test]
fn should_evaluate_builtin_functions_in_pl_context_on_real_kernel() {
    use crate::engine::Engine;
    use crate::value::SpinValue;

    let rules = RuleRegistry::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/rewrite-rules.toml"),
    )
    .expect("加载种子规则");
    let mut eng = Engine::new(
        Box::new(crate::sql::DuckExecutor::open_in_memory().unwrap()),
        rules,
    );
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_builtin(p_o OUT VARCHAR2) IS \
             DECLARE v_s VARCHAR2(20); \
             BEGIN \
               v_s := SUBSTR('ABCDEF', 1, 2) || '00' || ABS(-3) || ROUND(2.345, 2); \
               p_o := v_s; \
             END;",
    )
    .unwrap();
    let r = eng.call("prc_builtin", &[SpinValue::Null]).unwrap();
    let got = r.outs.get("p_o").cloned().unwrap_or(SpinValue::Null);
    assert_eq!(
        got.as_str_loose(),
        "AB0032.35",
        "内建函数应在 PL 上下文内经内核求值"
    );
}

/// 端到端探针：FOR rec IN <query>（隐式游标脱糖，§7.3.4）真实内核语义。
#[test]
fn should_run_for_loop_over_query_on_real_kernel() {
    use crate::engine::Engine;
    use crate::value::SpinValue;

    let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
    ex.execute("CREATE TABLE dat(a VARCHAR, n INT)", &BindMap::new())
        .unwrap();
    for i in 1..=3 {
        ex.execute(
            &format!("INSERT INTO dat VALUES ('r{i}', {i})"),
            &BindMap::new(),
        )
        .unwrap();
    }
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_for(p_o OUT NUMBER) IS \
             DECLARE v_sum NUMBER := 0; \
             BEGIN \
               FOR r IN (SELECT n FROM dat ORDER BY n) LOOP \
                 v_sum := v_sum + r.n; \
               END LOOP; \
               p_o := v_sum; \
             END;",
    )
    .unwrap();
    let r = eng
        .call("prc_for", &[SpinValue::Null])
        .expect("FOR rec IN query 应可执行");
    assert_eq!(r.outs.get("p_o"), Some(&SpinValue::Int(6)), "{:?}", r.outs);
}

#[test]
fn should_run_cursor_loop_with_record_field_access_on_real_kernel() {
    let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
    ex.execute("CREATE TABLE dat(a VARCHAR, n INT)", &BindMap::new())
        .unwrap();
    for i in 1..=3 {
        ex.execute(
            &format!("INSERT INTO dat VALUES ('row{i}', {i})"),
            &BindMap::new(),
        )
        .unwrap();
    }

    let sel = parse_statement("SELECT a, n FROM dat ORDER BY n");
    let block = PlBlock {
        label: None,
        declarations: vec![
            PlDeclaration::Variable(PlVarDecl {
                name: "v_sum".into(),
                data_type: PlDataType::TypeName("NUMBER".into()),
                default: Some(ogsql_parser::ast::Expr::Literal(
                    ogsql_parser::ast::Literal::Integer(0),
                )),
                constant: false,
                not_null: false,
                collate: None,
            }),
            PlDeclaration::Cursor(PlCursorDecl {
                name: "c_dat".into(),
                arguments: vec![],
                return_type: None,
                query: "SELECT a, n FROM dat ORDER BY n".into(),
                parsed_query: Some(Box::new(sel)),
                scrollable: false,
            }),
        ],
        body: vec![
            assign(
                var("v_sum"),
                ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(0)),
            ),
            assign(
                var("r_n"),
                ogsql_parser::ast::Expr::Literal(ogsql_parser::ast::Literal::Integer(0)),
            ),
            PlStatement::Open(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlOpenStmt {
                    cursor: var("c_dat"),
                    kind: ogsql_parser::ast::plpgsql::PlOpenKind::Simple { arguments: vec![] },
                },
                None,
            )),
            PlStatement::Loop(ogsql_parser::ast::Spanned::new(
                ogsql_parser::ast::plpgsql::PlLoopStmt {
                    label: None,
                    body: vec![
                        PlStatement::Fetch(ogsql_parser::ast::Spanned::new(
                            ogsql_parser::ast::plpgsql::PlFetchStmt {
                                cursor: var("c_dat"),
                                direction: None,
                                bulk_collect: false,
                                into: vec![
                                    ogsql_parser::ast::Expr::ColumnRef(vec![
                                        ident("r"),
                                        ident("a"),
                                    ]),
                                    ogsql_parser::ast::Expr::ColumnRef(vec![
                                        ident("r"),
                                        ident("n"),
                                    ]),
                                ],
                            },
                            None,
                        )),
                        PlStatement::Exit {
                            label: None,
                            condition: Some(ogsql_parser::ast::Expr::CursorAttribute {
                                cursor: Box::new(var("c_dat")),
                                attribute: ogsql_parser::ast::CursorAttributeKind::NotFound,
                            }),
                        },
                        assign(
                            var("v_sum"),
                            ogsql_parser::ast::Expr::BinaryOp {
                                left: Box::new(var("v_sum")),
                                op: "+".into(),
                                right: Box::new(ogsql_parser::ast::Expr::ColumnRef(vec![
                                    ident("r"),
                                    ident("n"),
                                ])),
                            },
                        ),
                    ],
                    end_label: None,
                },
                None,
            )),
            PlStatement::Close {
                cursor: var("c_dat"),
            },
        ],
        exception_block: None,
        end_label: None,
    };

    let prog = compile_block_with(&block, &RuleRegistry::default()).unwrap();
    let mut frame = Frame::new();
    let out = run(&prog, &mut frame, &ex).unwrap();
    assert_eq!(out, RunOutcome::Completed);
    assert_eq!(
        frame.get(&VarName("v_sum".into())),
        Some(&SpinValue::Int(6)) // 1+2+3
    );
}
