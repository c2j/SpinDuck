//! 标量函数改写的 SQL、绑定与错误时机行为锁定。

use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::value::SpinValue;
use std::cell::RefCell;
use std::rc::Rc;

type Calls = Rc<RefCell<Vec<(String, BindMap)>>>;

struct Recorder(Calls);

impl SqlExecutor for Recorder {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.borrow_mut().push((sql.into(), binds.clone()));
        Ok(RowStream::new(
            vec!["v".into()],
            vec![vec![SpinValue::Str("ok".into())]],
        ))
    }

    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        Ok(ExecOutcome { rows_affected: 0 })
    }
}

#[test]
fn should_evaluate_simple_case_selector_once_and_skip_other_results() {
    let calls = Calls::default();
    let mut engine = Engine::new(Box::new(Recorder(calls.clone())), RuleRegistry::default());
    engine
        .register(
            "CREATE FUNCTION f_once RETURN NUMBER IS BEGIN \
        RETURN CASE selector() WHEN 'no' THEN 1 / 0 WHEN 'ok' THEN 7 ELSE 1 / 0 END; END;",
        )
        .unwrap();
    assert_eq!(
        engine.call("f_once", &[]).unwrap().return_value,
        Some(SpinValue::Int(7))
    );
    assert_eq!(
        *calls.borrow(),
        vec![("SELECT selector()".into(), BindMap::new())]
    );
}

#[test]
fn should_render_simple_and_searched_case_with_exact_bindings() {
    use spinduck_core::ir::{BindSrc, IrExpr, VarName};
    use spinduck_core::sql::render::{expr_to_sql, expr_to_sql_binds};
    let var = |name: &str| IrExpr::Var(VarName(name.into()));
    let expr = IrExpr::Case {
        operand: Some(Box::new(var("p"))),
        whens: vec![(
            IrExpr::Const(SpinValue::Int(1)),
            IrExpr::Case {
                operand: None,
                whens: vec![(var("flag"), var("result"))],
                else_expr: None,
            },
        )],
        else_expr: Some(Box::new(IrExpr::Const(SpinValue::Str("O'Reilly".into())))),
    };
    assert_eq!(
        expr_to_sql(&expr, &RuleRegistry::default()),
        Some("CASE p WHEN 1 THEN CASE WHEN flag THEN result END ELSE 'O''Reilly' END".into())
    );
    assert_eq!(
        expr_to_sql_binds(&expr),
        Some((
            "CASE $v_p WHEN 1 THEN CASE WHEN $v_flag THEN $v_result END ELSE 'O''Reilly' END"
                .into(),
            vec![
                ("$v_p".into(), BindSrc::Var(VarName("p".into()))),
                ("$v_flag".into(), BindSrc::Var(VarName("flag".into()))),
                ("$v_result".into(), BindSrc::Var(VarName("result".into()))),
            ],
        ))
    );
}

#[test]
fn should_defer_scalar_rewrite_errors_until_execution() {
    let mut engine = Engine::new(
        Box::new(Recorder(Calls::default())),
        RuleRegistry::default(),
    );
    engine
        .register(
            "CREATE PROCEDURE delayed_error(p IN BOOLEAN) IS v NUMBER; BEGIN \
        IF p THEN v := to_number(1, 2); END IF; END;",
        )
        .unwrap();
    engine
        .call("delayed_error", &[SpinValue::Bool(false)])
        .unwrap();
    let err = engine
        .call("delayed_error", &[SpinValue::Bool(true)])
        .unwrap_err();
    assert_eq!(err.code.0, "BIZ-0131");
    assert_eq!(err.message, "TO_NUMBER requires 1 argument");
}

#[test]
fn should_prepare_builtin_fallback_during_compilation() {
    use spinduck_core::ir::{IrExpr, IrStmt};
    let mut engine = Engine::new(
        Box::new(Recorder(Calls::default())),
        RuleRegistry::default(),
    );
    engine
        .register(
            "CREATE PROCEDURE prepared_builtin(p IN VARCHAR2) IS v VARCHAR2(100); BEGIN \
        v := substr(p, 2, 3); END;",
        )
        .unwrap();
    let routine = engine.catalog.lookup("prepared_builtin").unwrap();
    let IrStmt::Assign {
        expr: IrExpr::FuncCall { prepared, .. },
        ..
    } = &routine.prog.code[1]
    else {
        panic!("expected compiled FuncCall");
    };
    assert_eq!(
        prepared.as_ref().unwrap(),
        "SELECT SUBSTRING($p0, $p1, $p2)"
    );
}

#[test]
fn should_prepare_scalar_sql_during_compilation() {
    use spinduck_core::ir::{IrExpr, IrStmt};
    let mut engine = Engine::new(
        Box::new(Recorder(Calls::default())),
        RuleRegistry::default(),
    );
    engine
        .register(
            "CREATE PROCEDURE prepared_sql(p IN VARCHAR2) IS v VARCHAR2(100); BEGIN \
        v := to_number(p); END;",
        )
        .unwrap();
    let routine = engine.catalog.lookup("prepared_sql").unwrap();
    let IrStmt::Assign {
        expr: IrExpr::SqlFn { prepared, .. },
        ..
    } = &routine.prog.code[1]
    else {
        panic!("expected compiled SqlFn");
    };
    let template = prepared.as_ref().unwrap();
    assert_eq!(template.sql, "SELECT CAST($v_p AS DECIMAL(38,6))");
    assert_eq!(
        template.binds,
        vec![(
            "$v_p".into(),
            spinduck_core::ir::BindSrc::Var(spinduck_core::ir::VarName("p".into()))
        )]
    );
}

#[test]
fn should_preserve_scalar_sql_text_and_bindings() {
    let calls = Calls::default();
    let mut engine = Engine::new(Box::new(Recorder(calls.clone())), RuleRegistry::default());
    engine
        .register(
            "CREATE PROCEDURE scalar_sql(p IN VARCHAR2) IS v VARCHAR2(100); BEGIN \
        v := to_char(to_date(p, 'YYYYMMDD') + 1, 'YYYY-MM-DD'); \
        v := substr(p, 2, 3); END;",
        )
        .unwrap();
    engine
        .call("scalar_sql", &[SpinValue::Str("20260105".into())])
        .unwrap();
    assert_eq!(
        *calls.borrow(),
        vec![
            (
                "SELECT strftime((strptime($v_p, '%Y%m%d') + to_days(1)), '%Y-%m-%d')".into(),
                BindMap::from([("$v_p".into(), SpinValue::Str("20260105".into()))])
            ),
            (
                "SELECT SUBSTRING($p0, $p1, $p2)".into(),
                BindMap::from([
                    ("$p0".into(), SpinValue::Str("20260105".into())),
                    ("$p1".into(), SpinValue::Int(2)),
                    ("$p2".into(), SpinValue::Int(3))
                ])
            ),
        ]
    );
}
