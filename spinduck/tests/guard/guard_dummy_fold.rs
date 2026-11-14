//! 哑表表达式折叠前的内核类型与行数语义守护。
#![cfg(feature = "native")]

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;

#[test]
fn should_preserve_dummy_query_step_budget_boundary() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute("CREATE TABLE sys_dummy AS SELECT 1", &BindMap::new())
        .unwrap();
    eng.register(
        "CREATE FUNCTION f_budget RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
        SELECT CASE WHEN TRUE THEN 'ok' ELSE 'no' END INTO v FROM sys_dummy; RETURN v; END;",
    )
    .unwrap();
    eng.step_budget = 3;
    assert_eq!(
        eng.call("f_budget", &[]).unwrap().return_value,
        Some(SpinValue::Str("ok".into()))
    );
    eng.step_budget = 2;
    let error = eng.call("f_budget", &[]).unwrap_err();
    assert_eq!(error.code.0, "ENG-0032");
    assert_eq!(
        error.message,
        "step budget exceeded (2): possible infinite loop"
    );
}

#[test]
fn should_fold_homogeneous_literal_case_with_cardinality_probe() {
    use spinduck_core::ir::{AssignTarget, IrExpr, IrStmt};
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.register("CREATE FUNCTION f_fold RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
        SELECT CASE WHEN NULL THEN 'skip' WHEN TRUE THEN 'ok' ELSE 'no' END INTO v FROM sys_dummy; RETURN v; END;").unwrap();
    let code = &eng.catalog.lookup("f_fold").unwrap().prog.code;
    let IrStmt::FoldedQuery {
        sql,
        target: AssignTarget::Var(name),
        expr: IrExpr::Case { .. },
    } = &code[1]
    else {
        panic!("missing guarded folded query: {:?}", code[1]);
    };
    assert_eq!(sql, "SELECT 1 FROM sys_dummy");
    assert_eq!(name.0, "v");
    assert_eq!(code.len(), 3);
}

#[test]
fn should_preserve_dummy_query_cardinality_and_missing_table() {
    for table in ["sys_dummy", "dual"] {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        eng.register(&format!("CREATE FUNCTION f_rows RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
            SELECT CASE WHEN NULL THEN 'skip' WHEN TRUE THEN 'ok' ELSE 'no' END INTO v FROM {table}; RETURN v; END;")).unwrap();
        let missing = eng.call("f_rows", &[]).unwrap_err();
        assert_eq!(missing.code.0, "KERNEL-0010");
        assert!(missing.message.contains(table), "{}", missing.message);
        eng.ex
            .execute(
                &format!("CREATE TABLE {table}(dummy INTEGER)"),
                &BindMap::new(),
            )
            .unwrap();
        let empty = eng.call("f_rows", &[]).unwrap_err();
        assert_eq!(empty.code.0, "BIZ-0100");
        assert!(empty.message.contains("NO_DATA_FOUND"), "{}", empty.message);
        eng.ex
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &BindMap::new())
            .unwrap();
        assert_eq!(
            eng.call("f_rows", &[]).unwrap().return_value,
            Some(SpinValue::Str("ok".into()))
        );
        eng.ex
            .execute(&format!("INSERT INTO {table} VALUES (2)"), &BindMap::new())
            .unwrap();
        let many = eng.call("f_rows", &[]).unwrap_err();
        assert_eq!(many.code.0, "BIZ-0101");
        assert!(many.message.contains("TOO_MANY_ROWS"), "{}", many.message);
    }
}

#[test]
fn should_keep_unproven_dummy_expressions_and_shapes_as_sql() {
    use spinduck_core::ir::IrStmt;
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for (expr, from) in [
        ("CASE WHEN TRUE THEN 'ok' ELSE 1 END", "sys_dummy"),
        ("CASE WHEN TRUE THEN p ELSE 'no' END", "sys_dummy"),
        ("CASE WHEN TRUE THEN r.f ELSE 1 END", "sys_dummy"),
        ("CASE 'A' WHEN 'a' THEN 1 ELSE 2 END", "sys_dummy"),
        ("CASE WHEN TRUE THEN 1.1 ELSE 2.2 END", "sys_dummy"),
        ("CASE WHEN TRUE THEN 1 ELSE 1 / 0 END", "sys_dummy"),
        ("nextval('s')", "sys_dummy"),
        ("substr('abc', 1, 1)", "sys_dummy"),
        ("CASE WHEN TRUE THEN 1 ELSE 2 END", "sys_dummy WHERE TRUE"),
        ("CASE WHEN TRUE THEN 1 ELSE 2 END", "sys_dummy LIMIT 1"),
        ("CASE WHEN TRUE THEN 1 ELSE 2 END", "sys_dummy ORDER BY 1"),
        ("CASE WHEN TRUE THEN 1 ELSE 2 END", "sys_dummy d"),
        ("CASE WHEN TRUE THEN 1 ELSE 2 END", "sys_dummy, dual"),
        ("CASE WHEN TRUE THEN 1 ELSE 2 END", "other_dummy"),
    ] {
        eng.register(&format!("CREATE OR REPLACE FUNCTION f_keep(p VARCHAR2) RETURN VARCHAR2 IS v VARCHAR2(100); r RECORD; BEGIN \
            SELECT {expr} INTO v FROM {from}; RETURN v; END;")).unwrap();
        let code = &eng.catalog.lookup("f_keep").unwrap().prog.code;
        let queries: Vec<_> = code
            .iter()
            .filter_map(|stmt| match stmt {
                IrStmt::SqlQuery { sql, into, .. } => Some((sql, into.len())),
                _ => None,
            })
            .collect();
        assert_eq!(queries.len(), 1, "{expr} FROM {from}");
        assert_eq!(queries[0].1, 1, "{expr} FROM {from}");
        assert_ne!(queries[0].0, "SELECT 1 FROM sys_dummy");
    }
}

#[test]
fn should_match_kernel_for_proven_literal_case_values() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute("CREATE TABLE sys_dummy AS SELECT 1", &BindMap::new())
        .unwrap();
    for expr in [
        "CASE 2 WHEN 1 THEN 3 WHEN 2 THEN 4 ELSE 5 END",
        "CASE NULL WHEN 1 THEN 3 WHEN NULL THEN 4 END",
        "CASE TRUE WHEN FALSE THEN NULL WHEN TRUE THEN TRUE END",
        "CASE WHEN FALSE THEN 'no' WHEN NULL THEN 'skip' ELSE '' END",
        "CASE WHEN TRUE THEN NULL ELSE 'text' END",
        "CASE WHEN TRUE THEN CASE WHEN FALSE THEN 2 ELSE 3 END ELSE 4 END",
    ] {
        for (name, suffix) in [("f_folded", ""), ("f_kernel", " WHERE TRUE")] {
            eng.register(&format!(
                "CREATE OR REPLACE FUNCTION {name} RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
                SELECT {expr} INTO v FROM sys_dummy{suffix}; RETURN v; END;"
            ))
            .unwrap();
        }
        let expected = eng.call("f_kernel", &[]).unwrap().return_value;
        assert_eq!(
            eng.call("f_folded", &[]).unwrap().return_value,
            expected,
            "{expr}"
        );
    }
}

#[test]
fn should_flush_dummy_writes_before_probe_and_rollback_on_cardinality_error() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute(
            "CREATE TABLE sys_dummy(dummy INTEGER); INSERT INTO sys_dummy VALUES (1)",
            &BindMap::new(),
        )
        .unwrap();
    eng.register("CREATE FUNCTION f_write RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
        INSERT INTO sys_dummy VALUES (2); SELECT CASE WHEN TRUE THEN 'ok' ELSE 'no' END INTO v FROM sys_dummy; RETURN v; END;").unwrap();
    eng.batch_commit = true;
    let error = eng.call("f_write", &[]).unwrap_err();
    assert_eq!(error.code.0, "BIZ-0101");
    assert_eq!(error.message, "TOO_MANY_ROWS");
    let mut rows = eng
        .ex
        .query("SELECT dummy FROM sys_dummy", &BindMap::new())
        .unwrap();
    assert_eq!(rows.fetch_many(2), vec![vec![SpinValue::Int(1)]]);
}

#[test]
fn should_preserve_kernel_case_type_error_on_dummy_query() {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.ex
        .execute(
            "CREATE TABLE sys_dummy AS SELECT 1 AS dummy",
            &BindMap::new(),
        )
        .unwrap();
    eng.register(
        "CREATE FUNCTION f_sql_case RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
        SELECT CASE WHEN TRUE THEN 'ok' ELSE 1 END INTO v FROM sys_dummy; RETURN v; END;",
    )
    .unwrap();
    let err = eng.call("f_sql_case", &[]).unwrap_err();
    assert_eq!(err.code.0, "KERNEL-0010");
    assert!(err.message.contains("Could not convert"), "{}", err.message);
    eng.register(
        "CREATE FUNCTION f_pl_case RETURN VARCHAR2 IS v VARCHAR2(100); BEGIN \
        v := CASE WHEN TRUE THEN 'ok' ELSE 1 END; RETURN v; END;",
    )
    .unwrap();
    assert_eq!(
        eng.call("f_pl_case", &[]).unwrap().return_value,
        Some(SpinValue::Str("ok".into()))
    );
}
