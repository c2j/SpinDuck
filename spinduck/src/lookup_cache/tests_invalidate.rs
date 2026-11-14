//! INSERT 失效的前缀扫描行为：只失效写表（读到的表不再过度失效），
//! 万行合并 VALUES 与限定名/引号形态仍必须命中目标表。

use super::tests::{cache, purchase_binds, PURCHASE};
use super::*;
use crate::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};

/// 记录内层查询文本并按脚本回行的假执行器（本模块独立构造，不共享可变 fixture）。
struct Inner {
    queries: Rc<RefCell<Vec<String>>>,
    scripts: RefCell<Vec<RowStream>>,
}

impl Inner {
    fn new(values: &[&str]) -> (Self, Rc<RefCell<Vec<String>>>) {
        let queries = Rc::new(RefCell::new(Vec::new()));
        let scripts = values
            .iter()
            .map(|v| {
                RowStream::new(
                    vec!["c".into()],
                    vec![vec![
                        SpinValue::Str("S1".to_string()),
                        SpinValue::Str((*v).to_string()),
                    ]],
                )
            })
            .collect();
        (
            Self {
                queries: queries.clone(),
                scripts: RefCell::new(scripts),
            },
            queries,
        )
    }
}

impl SqlExecutor for Inner {
    fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        self.queries.borrow_mut().push(sql.to_string());
        let mut scripts = self.scripts.borrow_mut();
        Ok(if scripts.is_empty() {
            RowStream::new(Vec::new(), Vec::new())
        } else {
            scripts.remove(0)
        })
    }
    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

fn probe_value(ex: &LookupExecutor) -> SpinValue {
    ex.query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(1)[0][0]
        .clone()
}

#[test]
fn should_invalidate_tables_written_after_first_insert() {
    for stmt in [
        "INSERT INTO other VALUES (1); DELETE FROM par_sys_purchase",
        "INSERT INTO other VALUES (1); UPDATE par_sys_purchase SET purchase_days = 9",
    ] {
        let (inner, queries) = Inner::new(&["7", "9"]);
        let ex = cache(Box::new(inner));
        assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
        ex.execute(stmt, &BindMap::new()).unwrap();
        assert_eq!(probe_value(&ex), SpinValue::Str("9".into()), "{stmt}");
        assert_eq!(queries.borrow().len(), 2, "{stmt}");
    }
}

#[test]
fn should_fall_back_for_ambiguous_insert_forms() {
    for stmt in [
        "WITH src AS (SELECT 1) INSERT INTO par_sys_purchase SELECT * FROM src",
        "insert/*c*/into par_sys_purchase VALUES (1)",
        "INSERT INTO \"a\"\"b\" SELECT * FROM par_sys_purchase",
        "INSERT INTO other VALUES ('a;''b'); DELETE FROM par_sys_purchase",
        "INSERT INTO other VALUES (1); -- next statement\nUPDATE par_sys_purchase SET purchase_days = 9",
        "INSERT INTO other VALUES ($tag$a;b$tag$); DELETE FROM par_sys_purchase",
    ] {
        assert_eq!(insert_target(stmt), None, "{stmt}");
        let (inner, queries) = Inner::new(&["7", "9"]);
        let ex = cache(Box::new(inner));
        assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
        ex.execute(stmt, &BindMap::new()).unwrap();
        assert_eq!(probe_value(&ex), SpinValue::Str("9".into()), "{stmt}");
        assert_eq!(queries.borrow().len(), 2, "{stmt}");
    }
}

#[test]
fn should_preserve_cache_for_single_insert_select_and_literal_semicolons() {
    for stmt in [
        "INSERT INTO other(a) SELECT security_id FROM par_sys_purchase",
        "/* leading */ INSERT INTO other VALUES ('a;b')",
        "INSERT INTO other VALUES ('a;''b')",
        "INSERT INTO other VALUES (1); -- trailing comment",
    ] {
        let (inner, queries) = Inner::new(&["7"]);
        let ex = cache(Box::new(inner));
        assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
        ex.execute(stmt, &BindMap::new()).unwrap();
        assert_eq!(probe_value(&ex), SpinValue::Str("7".into()), "{stmt}");
        assert_eq!(queries.borrow().len(), 1, "{stmt}");
    }
}

#[test]
fn should_clear_cache_when_fallback_tokenization_fails() {
    let (inner, queries) = Inner::new(&["7", "9"]);
    let ex = cache(Box::new(inner));
    assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
    ex.execute("INSERT INTO other VALUES ('unterminated;", &BindMap::new())
        .unwrap();
    assert_eq!(probe_value(&ex), SpinValue::Str("9".into()));
    assert_eq!(queries.borrow().len(), 2);
}

#[test]
fn should_invalidate_target_on_large_values_insert() {
    let (inner, queries) = Inner::new(&["7", "9"]);
    let ex = cache(Box::new(inner));
    assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
    let mut huge = String::from("INSERT INTO par_sys_purchase VALUES ");
    for i in 0..5000 {
        if i > 0 {
            huge.push(',');
        }
        huge.push_str(&format!("('S{i}', {i})"));
    }
    ex.execute(&huge, &BindMap::new()).unwrap();
    assert_eq!(probe_value(&ex), SpinValue::Str("9".into()));
    assert_eq!(queries.borrow().len(), 2, "写目标表必须失效并重建");
}

#[test]
fn should_not_invalidate_tables_only_read_by_insert_select() {
    let (inner, queries) = Inner::new(&["7"]);
    let ex = cache(Box::new(inner));
    assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
    ex.execute(
        "INSERT INTO other SELECT security_id FROM par_sys_purchase WHERE x = 1",
        &BindMap::new(),
    )
    .unwrap();
    assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
    assert_eq!(
        queries.borrow().len(),
        1,
        "INSERT 读到的表不被写入，缓存不应失效: {:?}",
        queries.borrow()
    );
}

#[test]
fn should_invalidate_qualified_quoted_and_no_space_targets() {
    for stmt in [
        "INSERT INTO sch.par_sys_purchase VALUES (1)",
        "INSERT INTO \"PAR_SYS_PURCHASE\" VALUES (1)",
        "insert into par_sys_purchase(a) values (1)",
    ] {
        let (inner, queries) = Inner::new(&["7", "9"]);
        let ex = cache(Box::new(inner));
        assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
        ex.execute(stmt, &BindMap::new()).unwrap();
        assert_eq!(probe_value(&ex), SpinValue::Str("9".into()), "{stmt}");
        assert_eq!(queries.borrow().len(), 2, "{stmt}");
    }
}

#[test]
fn should_not_be_fooled_by_table_names_inside_values_literals() {
    let (inner, queries) = Inner::new(&["7"]);
    let ex = cache(Box::new(inner));
    assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
    ex.execute(
        "INSERT INTO other VALUES ('insert into par_sys_purchase values (1)')",
        &BindMap::new(),
    )
    .unwrap();
    assert_eq!(probe_value(&ex), SpinValue::Str("7".into()));
    assert_eq!(queries.borrow().len(), 1, "{:?}", queries.borrow());
}

#[test]
fn should_keep_alias_schema_across_repeated_hits() {
    // schema 缓存守护：命中返回的列名（含别名）必须与首次构建逐字一致
    let (inner, queries) = Inner::new(&["7"]);
    let ex = cache(Box::new(inner));
    let sql = "SELECT coalesce(t.purchase_days, 1) AS pd FROM par_sys_purchase AS t \
         WHERE $v_p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date \
         AND t.security_id = $v_r_get_purchase__security_id";
    let first = ex.query(sql, &purchase_binds("20260105", "S1")).unwrap();
    assert_eq!(first.schema, vec!["pd".to_string()]);
    let second = ex.query(sql, &purchase_binds("20260105", "S1")).unwrap();
    assert_eq!(second.schema, vec!["pd".to_string()]);
    assert_eq!(
        queries.borrow().len(),
        1,
        "第二次必须命中缓存: {:?}",
        queries.borrow()
    );
}
