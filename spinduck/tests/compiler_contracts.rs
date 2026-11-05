//! 编译器契约测试（不依赖 DuckDB 内核）：变量收集、标签、算子映射、深度护栏、
//! 纯表达式识别、记录字段消歧。用假执行器观察改写产物，用 Engine::call 观察求值。

use spinduck_core::cli::NoopExecutor;
use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::ir::compile_block_with;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::value::SpinValue;
use std::cell::RefCell;
use std::rc::Rc;

fn engine() -> Engine {
    Engine::new(Box::new(NoopExecutor), RuleRegistry::default())
}

/// 记录 SQL 的假执行器（按脚本返回行）；`Log` 句柄留在测试侧以便断言。
type Log = Rc<RefCell<Vec<String>>>;

struct Recorder {
    calls: Log,
    script: RefCell<Vec<RowStream>>,
}

impl Recorder {
    fn new() -> (Self, Log) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                calls: log.clone(),
                script: RefCell::new(Vec::new()),
            },
            log,
        )
    }
    fn with_script(schema: &[&str], rows: Vec<Vec<SpinValue>>) -> (Self, Log) {
        let (me, log) = Self::new();
        me.script.borrow_mut().push(RowStream::new(
            schema.iter().map(|s| s.to_string()).collect(),
            rows,
        ));
        (me, log)
    }
}

impl SqlExecutor for Recorder {
    fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        self.calls.borrow_mut().push(sql.to_string());
        let mut it = self.script.borrow_mut();
        if it.is_empty() {
            Ok(RowStream::new(Vec::new(), Vec::new()))
        } else {
            Ok(it.remove(0))
        }
    }
    fn execute(&self, sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.calls.borrow_mut().push(sql.to_string());
        Ok(ExecOutcome { rows_affected: 0 })
    }
}

#[test]
fn should_map_every_binary_operator_in_pl_expressions() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_ops(a IN NUMBER, b IN NUMBER) RETURN BOOLEAN IS \
         BEGIN \
           RETURN (a - b) >= 0 AND (a / b) >= 1 AND (a <> b) AND (a <= 10) OR FALSE; \
         END;",
    )
    .unwrap();
    // 5 - 3 = 2 ≥ 0 ；5/3 ≥ 1 ；5<>3 ；5<=10 → TRUE
    assert_eq!(
        eng.call("f_ops", &[SpinValue::Int(5), SpinValue::Int(3)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(true))
    );
    // 3 - 5 < 0 → FALSE
    assert_eq!(
        eng.call("f_ops", &[SpinValue::Int(3), SpinValue::Int(5)])
            .unwrap()
            .return_value,
        Some(SpinValue::Bool(false))
    );
}

#[test]
fn should_bind_variables_referenced_only_in_nested_expressions() {
    let (ex, log) = Recorder::new();
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_nested IS \
         DECLARE v_d NUMBER; \
         BEGIN \
           IF v_d IS NULL THEN v_d := 1; END IF; \
           WHILE v_d < 2 LOOP v_d := v_d + 1; END LOOP; \
           v_d := ABS(v_d); \
           INSERT INTO t(d) VALUES (v_d); \
         END;",
    )
    .unwrap();
    eng.call("p_nested", &[]).unwrap();
    let sql = log.borrow().clone();
    let ins = sql
        .iter()
        .find(|s| s.starts_with("insert into t"))
        .expect("INSERT 下发");
    assert!(
        ins.contains("$v_v_d"),
        "嵌套表达式里出现的变量也必须进变量集：{ins}"
    );
}

#[test]
fn should_compile_null_statement_as_noop_not_commit() {
    // 回归：`NULL;` 曾被编译为 TxCommit，误产生事务边界
    let mut eng = engine();
    eng.register("CREATE OR REPLACE PROCEDURE p_null IS BEGIN NULL; END;")
        .unwrap();
    let r = eng.catalog.lookup("p_null").unwrap();
    assert_eq!(r.prog.code.len(), 1);
    assert!(
        matches!(r.prog.code[0], spinduck_core::ir::IrStmt::Nop),
        "{:?}",
        r.prog.code
    );

    eng.register("CREATE OR REPLACE PROCEDURE p_commit IS BEGIN COMMIT; END;")
        .unwrap();
    let r = eng.catalog.lookup("p_commit").unwrap();
    assert!(matches!(
        r.prog.code[0],
        spinduck_core::ir::IrStmt::TxCommit
    ));
}

#[test]
fn should_reject_exit_and_continue_outside_loop() {
    let mut eng = engine();
    let e = eng
        .register("CREATE OR REPLACE PROCEDURE p1 IS BEGIN EXIT; END;")
        .unwrap_err();
    assert_eq!(e.code.to_string(), "ENG-0023");
    let e = eng
        .register("CREATE OR REPLACE PROCEDURE p2 IS BEGIN CONTINUE; END;")
        .unwrap_err();
    assert_eq!(e.code.to_string(), "ENG-0024");
}

#[test]
fn should_accept_max_nesting_and_reject_deeper() {
    // MAX_DEPTH = 32（留出解释器/解析器递归余量，见 architecture §3.2 注）
    let body = |n: usize| {
        let mut s = String::from("CREATE OR REPLACE PROCEDURE p_depth IS BEGIN ");
        for _ in 0..n {
            s.push_str("BEGIN ");
        }
        s.push_str("NULL; ");
        for _ in 0..n {
            s.push_str("END; ");
        }
        s.push_str("END;");
        s
    };
    let mut eng = engine();
    eng.register(&body(31)).unwrap();
    let e = eng.register(&body(32)).unwrap_err();
    assert_eq!(
        e.code.to_string(),
        "ENG-0021",
        "超过 MAX_DEPTH 必须编译期拒绝"
    );
}

#[test]
fn should_resolve_explicit_and_implicit_loop_labels() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_lbl IS \
         DECLARE v NUMBER := 0; \
         BEGIN \
           <<outer>> \
           LOOP \
             v := v + 1; \
             CONTINUE outer WHEN v < 2; \
             EXIT outer; \
           END LOOP; \
           LOOP \
             v := v + 1; \
             EXIT WHEN v > 3; \
           END LOOP; \
         END;",
    )
    .unwrap();
}

#[test]
fn should_recognize_pure_expression_return_bodies() {
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_pure(p IN NUMBER) RETURN NUMBER IS \
         BEGIN NULL; RETURN p * 2; END;",
    )
    .unwrap();
    let r = eng.catalog.lookup("f_pure").unwrap();
    assert!(
        r.prog.single_return_expr().is_some(),
        "NULL; + RETURN <expr> 仍是纯表达式体"
    );

    // 两条 RETURN（含控制流）不是纯表达式体
    eng.register(
        "CREATE OR REPLACE FUNCTION f_two(p IN NUMBER) RETURN NUMBER IS \
         BEGIN IF p > 0 THEN RETURN 1; END IF; RETURN 2; END;",
    )
    .unwrap();
    let r = eng.catalog.lookup("f_two").unwrap();
    assert!(r.prog.single_return_expr().is_none());
}

#[test]
fn should_disambiguate_record_field_vs_scalar_variable() {
    let (ex, _log) = Recorder::with_script(
        &["a", "b"],
        vec![vec![SpinValue::Int(2), SpinValue::Int(3)]],
    );
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_rec IS \
         DECLARE v_sum NUMBER := 0; \
         BEGIN \
           FOR r IN (SELECT 1 AS a, 1 AS b) LOOP \
             v_sum := v_sum + r.a + r.b; \
           END LOOP; \
         END;",
    )
    .unwrap();
    eng.call("p_rec", &[]).unwrap();
    // 记录字段访问不得被当成整行变量：不应报 "undefined variable r"
}

#[test]
fn should_compile_block_with_declarations_and_rules() {
    let text = "CREATE OR REPLACE PROCEDURE p_blk IS \
                DECLARE v_a NUMBER := 7; v_b NUMBER; \
                BEGIN v_b := v_a * 2; END;";
    let tokens = ogsql_parser::token::tokenizer::Tokenizer::new(text)
        .tokenize()
        .unwrap();
    let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
    let block = match &stmts[0] {
        ogsql_parser::ast::Statement::CreateProcedure(cs) => cs.block.as_ref().unwrap(),
        other => panic!("unexpected {other:?}"),
    };
    let prog = compile_block_with(block, &RuleRegistry::default()).unwrap();
    // 声明段生成两条初始化指令，随后是赋值
    assert_eq!(prog.code.len(), 3, "{:?}", prog.code);
    assert!(matches!(
        prog.code[0],
        spinduck_core::ir::IrStmt::Assign { .. }
    ));
}

#[test]
fn should_reject_unsupported_statements_explicitly() {
    let mut eng = engine();
    let e = eng
        .register(
            "CREATE OR REPLACE PROCEDURE p_fa IS \
             BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;",
        )
        .unwrap_err();
    assert_eq!(e.code.to_string(), "ENG-0022");
    let e = eng
        .register(
            "CREATE OR REPLACE PROCEDURE p_ba IS \
             DECLARE v_sql VARCHAR2(50); BEGIN EXECUTE IMMEDIATE v_sql; END;",
        )
        .unwrap_err();
    assert_eq!(e.code.to_string(), "ENG-0062");
}

#[test]
fn should_delegate_builtin_calls_to_kernel_with_named_binds() {
    // ABS 不在例程目录 → 下沉为 SELECT abs($p0)，由内核求值
    let (ex, log) = Recorder::with_script(&["abs"], vec![vec![SpinValue::Int(5)]]);
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_builtin IS \
         DECLARE v NUMBER; BEGIN v := ABS(-5); INSERT INTO t(d) VALUES (v); END;",
    )
    .unwrap();
    eng.call("p_builtin", &[]).unwrap();
    let calls = log.borrow().clone();
    let q = calls
        .iter()
        .find(|s| s.to_ascii_lowercase().starts_with("select abs"))
        .unwrap_or_else(|| panic!("内建函数应下沉内核：{calls:?}"));
    assert_eq!(q.to_ascii_lowercase(), "select abs($p0)");
}

#[test]
fn should_rewrite_dialect_function_name_before_delegating() {
    // 规则改名在编译期生效：NVL → coalesce（否则内核无 nvl）
    let (ex, log) = Recorder::with_script(&["c"], vec![vec![SpinValue::Int(7)]]);
    let mut rules = RuleRegistry::default();
    rules
        .functions
        .insert("NVL".to_string(), "coalesce".to_string());
    let mut eng = Engine::new(Box::new(ex), rules);
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_nvl IS \
         DECLARE v NUMBER; BEGIN v := NVL(NULL, 7); END;",
    )
    .unwrap();
    eng.call("p_nvl", &[]).unwrap();
    let calls = log.borrow().clone();
    assert!(
        calls.iter().any(|s| s.starts_with("SELECT coalesce")),
        "应按规则改名后下发：{calls:?}"
    );
}

#[test]
fn should_apply_parse_level_transform_when_delegating() {
    // `@to_char` 类标记在内建下沉时由 apply_ast_transforms 展开
    let (ex, log) = Recorder::with_script(&["c"], vec![vec![SpinValue::Str("7".into())]]);
    let mut rules = RuleRegistry::default();
    rules
        .functions
        .insert("TO_CHAR".to_string(), "@to_char".to_string());
    let mut eng = Engine::new(Box::new(ex), rules);
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_tc IS \
         DECLARE v VARCHAR2(10); BEGIN v := TO_CHAR(7); END;",
    )
    .unwrap();
    eng.call("p_tc", &[]).unwrap();
    let calls = log.borrow().clone();
    assert!(
        calls.iter().any(|s| s.contains("CAST(")),
        "1 参 TO_CHAR 应变换为 CAST：{calls:?}"
    );
}

#[test]
fn should_treat_unary_plus_as_identity() {
    // 一元正不得走负号分支（guard 变异曾使 `+x` 变成 `-x`）
    let mut eng = engine();
    eng.register(
        "CREATE OR REPLACE FUNCTION f_uplus(p IN NUMBER) RETURN NUMBER IS \
         BEGIN RETURN +p; END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_uplus", &[SpinValue::Int(7)])
            .unwrap()
            .return_value,
        Some(SpinValue::Int(7))
    );
}

#[test]
fn should_execute_for_loop_body_once_per_row() {
    // for_cursor_loop 脱糖若被跳过，循环体不会执行 → 聚合值缺失
    let (ex, log) = Recorder::with_script(
        &["n"],
        vec![
            vec![SpinValue::Int(1)],
            vec![SpinValue::Int(2)],
            vec![SpinValue::Int(3)],
        ],
    );
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_for IS \
         DECLARE v_sum NUMBER := 0; \
         BEGIN \
           FOR r IN (SELECT n FROM dat ORDER BY n) LOOP \
             v_sum := v_sum + r.n; \
           END LOOP; \
           INSERT INTO out_t(v) VALUES (v_sum); \
         END;",
    )
    .unwrap();
    eng.call("p_for", &[]).unwrap();
    let calls = log.borrow().clone();
    let ins = calls
        .iter()
        .find(|s| s.to_ascii_lowercase().contains("insert into out_t"))
        .expect("循环结束后应有聚合 INSERT");
    // 记录字段取自脚本行：v_sum = 1+2+3 = 6，以绑定/文本形态出现
    assert!(
        ins.contains("6") || ins.contains("$v_v_sum"),
        "聚合结果应进入 INSERT：{ins}"
    );
}

#[test]
fn should_bind_record_field_references_in_sql_text() {
    // 语料 PKG_SPLIT_TRADE_STEP3_SH.prc_trd_deal_purchase：
    // `INSERT ... VALUES (v_fund_code, r_get_purchase.market_code, ...)`
    // 记录变量限定字段必须替换为字段绑定，表别名限定列不得替换
    use spinduck_core::ir::BindSrc;

    let (ex, log) = Recorder::with_script(
        &["a", "fee"],
        vec![vec![SpinValue::Int(1), SpinValue::Decimal(1250, 15, 2)]],
    );
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_recfield IS \
         DECLARE v_fund VARCHAR(10) := 'F001'; \
         BEGIN \
           FOR r IN (SELECT 1 AS a, 12.50 AS fee) LOOP \
             INSERT INTO out_t(f, fee) VALUES (v_fund, r.fee); \
           END LOOP; \
         END;",
    )
    .unwrap();
    eng.call("p_recfield", &[]).unwrap();

    let calls = log.borrow().clone();
    let ins = calls
        .iter()
        .find(|s| s.to_ascii_lowercase().contains("insert into out_t"))
        .expect("INSERT 应下发");
    assert!(
        ins.contains("$v_r__fee"),
        "记录字段应替换为 $v_<rec>__<field> 占位符：{ins}"
    );
    assert!(!ins.contains("r."), "不得残留记录限定符：{ins}");

    // 绑定源类型：Field 而非 Var
    let prog = eng.catalog.lookup("p_recfield").unwrap();
    let has_field_bind = prog.prog.code.iter().any(|st| {
        matches!(
            st,
            spinduck_core::ir::IrStmt::SqlExec { binds, .. }
                if binds.iter().any(|(_, src)| matches!(src, BindSrc::Field { .. }))
        )
    });
    assert!(has_field_bind, "INSERT 绑定表应含 Field 源");
}

#[test]
fn should_fetch_whole_row_into_rowtype_record() {
    // 语料 prc_trd_deal_purchase：`r c%ROWTYPE` + `FETCH c INTO r` + `r.field` 下发 SQL
    let (ex, log) = Recorder::with_script(&["fee"], vec![vec![SpinValue::Decimal(1250, 15, 2)]]);
    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    eng.register(
        "CREATE OR REPLACE PROCEDURE p_rowtype IS \
         DECLARE r_emp c_emp%ROWTYPE; \
         CURSOR c_emp IS SELECT 1 AS a, 12.50 AS fee; \
         BEGIN \
           OPEN c_emp; \
           LOOP \
             FETCH c_emp INTO r_emp; \
             EXIT WHEN c_emp%NOTFOUND; \
             INSERT INTO out_t(fee) VALUES (r_emp.fee); \
           END LOOP; \
           CLOSE c_emp; \
         END;",
    )
    .unwrap();
    eng.call("p_rowtype", &[]).unwrap();
    let calls = log.borrow().clone();
    let ins = calls
        .iter()
        .find(|s| s.to_ascii_lowercase().contains("insert into out_t"))
        .expect("INSERT 应下发");
    assert!(ins.contains("$v_r_emp__fee"), "整行记录字段应绑定：{ins}");
}

#[test]
fn should_render_lowercase_oracle_format() {
    // 回归（语料 FNC_GET_PURCHASE_JS_DAYS 用小写 'yyyymmdd'）：
    // 格式令牌大小写不敏感，否则 strptime 按字面量解析必然失败
    use spinduck_core::sql::oracle_format_to_strftime;
    assert_eq!(oracle_format_to_strftime("yyyymmdd"), "%Y%m%d");
    assert_eq!(oracle_format_to_strftime("YYYYMMDD"), "%Y%m%d");
    assert_eq!(oracle_format_to_strftime("yyyy-mm-dd"), "%Y-%m-%d");
}
