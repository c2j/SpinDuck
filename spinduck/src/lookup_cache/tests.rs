//! 查找缓存的行为：一次构建、按键探测、写入失效、认不出则原样下发。

use super::*;
use crate::exec::FakeExecutor;
use std::cell::Cell;

pub(super) const PURCHASE: &str = "SELECT coalesce(t.purchase_days, 1) FROM par_sys_purchase AS t \
     WHERE $v_p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date \
     AND t.security_id = $v_r_get_purchase__security_id";

pub(super) fn strs(pairs: &[(&str, &str)]) -> BindMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), SpinValue::Str((*v).to_string())))
        .collect()
}

pub(super) fn purchase_binds(date: &str, security: &str) -> BindMap {
    strs(&[
        ("$v_p_i_date", date),
        ("$v_r_get_purchase__security_id", security),
    ])
}

struct Rec {
    calls: Rc<RefCell<Vec<String>>>,
    scripts: Rc<RefCell<Vec<RowStream>>>,
    execs: Rc<Cell<u64>>,
}

impl Rec {
    fn new(rows: Vec<Row>) -> (Self, Rc<RefCell<Vec<String>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let scripts = Rc::new(RefCell::new(vec![RowStream::new(vec!["c".into()], rows)]));
        (
            Self {
                calls: calls.clone(),
                scripts,
                execs: Rc::new(Cell::new(0)),
            },
            calls,
        )
    }

    fn push(&self, rows: Vec<Row>) {
        self.scripts
            .borrow_mut()
            .push(RowStream::new(vec!["c".into()], rows));
    }
}

impl SqlExecutor for Rec {
    fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        self.calls.borrow_mut().push(sql.to_string());
        let mut scripts = self.scripts.borrow_mut();
        Ok(if scripts.is_empty() {
            RowStream::new(Vec::new(), Vec::new())
        } else {
            scripts.remove(0)
        })
    }

    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.execs.set(self.execs.get() + 1);
        Ok(ExecOutcome { rows_affected: 1 })
    }
}

pub(super) fn cache(inner: Box<dyn SqlExecutor>) -> LookupExecutor {
    LookupExecutor::new(inner, Rc::new(LookupState::default()))
}

fn row(vals: &[&str]) -> Row {
    vals.iter()
        .map(|v| SpinValue::Str((*v).to_string()))
        .collect()
}

#[test]
fn should_keep_select_list_names_on_the_probed_stream() {
    let (rec, _) = Rec::new(vec![row(&["1"]), row(&["2"])]);
    rec.push(vec![row(&["1"]), row(&["2"])]);
    let ex = cache(Box::new(rec));
    let bare = ex
        .query("SELECT n FROM dat ORDER BY n", &BindMap::new())
        .unwrap();
    assert_eq!(bare.schema, vec!["n".to_string()]);
    let alias = ex
        .query("SELECT n AS num FROM dat ORDER BY n", &BindMap::new())
        .unwrap();
    assert_eq!(alias.schema, vec!["num".to_string()]);
}

#[test]
fn should_fall_back_when_the_probe_bind_type_differs_from_the_column() {
    let (rec, calls) = Rec::new(vec![vec![SpinValue::Int(1), SpinValue::Str("AbC".into())]]);
    rec.push(vec![vec![SpinValue::Str("AbC".into())]]);
    let ex = cache(Box::new(rec));
    let mut binds = BindMap::new();
    binds.insert("$v_p".to_string(), SpinValue::Str("1".into()));
    let rows = ex
        .query("SELECT s FROM t_form WHERE id = $v_p", &binds)
        .unwrap()
        .fetch_many(2);
    assert_eq!(rows, vec![vec![SpinValue::Str("AbC".into())]]);
    assert_eq!(
        calls.borrow().len(),
        2,
        "列类型与绑定类型不同时必须回退原语句: {:?}",
        calls.borrow()
    );
}

#[test]
fn should_scan_purchase_once_and_probe_other_security_ids() {
    let (rec, calls) = Rec::new(vec![row(&["0130000001", "7"]), row(&["0130000002", "1"])]);
    let ex = cache(Box::new(rec));
    let first = ex
        .query(PURCHASE, &purchase_binds("20260105", "0130000001"))
        .unwrap()
        .fetch_many(2);
    let second = ex
        .query(PURCHASE, &purchase_binds("20260105", "0130000002"))
        .unwrap()
        .fetch_many(2);
    let missing = ex
        .query(PURCHASE, &purchase_binds("20260105", "missing"))
        .unwrap()
        .fetch_many(2);
    assert_eq!(first, vec![vec![SpinValue::Str("7".into())]]);
    assert_eq!(second, vec![vec![SpinValue::Str("1".into())]]);
    assert!(missing.is_empty());
    assert_eq!(
        calls.borrow().len(),
        1,
        "第二次起不得再扫表: {:?}",
        calls.borrow()
    );
    assert!(
        calls.borrow()[0].contains("coalesce(t.purchase_days, 1)"),
        "构建语句必须让 DuckDB 计算 coalesce: {}",
        calls.borrow()[0]
    );
}

#[test]
fn should_return_every_duplicate_key_for_too_many_rows() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"]), row(&["S1", "8"])]);
    let ex = cache(Box::new(rec));
    let rows = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(2);
    assert_eq!(rows.len(), 2);
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn should_treat_null_equality_bind_as_no_match() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"])]);
    let ex = cache(Box::new(rec));
    let _ = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap();
    let mut binds = purchase_binds("20260105", "S1");
    binds.insert("$v_r_get_purchase__security_id".into(), SpinValue::Null);
    let rows = ex.query(PURCHASE, &binds).unwrap().fetch_many(2);
    assert!(rows.is_empty());
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn should_rebuild_after_write_to_cached_table_only() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"])]);
    rec.push(vec![row(&["S1", "9"])]);
    let ex = cache(Box::new(rec));
    let _ = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap();
    ex.execute("UPDATE other SET a = 1", &BindMap::new())
        .unwrap();
    let kept = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(1);
    assert_eq!(kept[0][0], SpinValue::Str("7".into()));
    ex.execute(
        "DELETE FROM par_sys_purchase WHERE security_id = 'S1'",
        &BindMap::new(),
    )
    .unwrap();
    let rebuilt = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(1);
    assert_eq!(rebuilt[0][0], SpinValue::Str("9".into()));
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn should_rebuild_when_the_date_bind_changes() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"])]);
    rec.push(vec![row(&["S1", "3"])]);
    let ex = cache(Box::new(rec));
    let _ = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap();
    let next = ex
        .query(PURCHASE, &purchase_binds("20260106", "S1"))
        .unwrap()
        .fetch_many(1);
    assert_eq!(next[0][0], SpinValue::Str("3".into()));
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn should_pass_through_case_selects_from_sys_dummy() {
    let sql = "SELECT CASE $v_fee WHEN '11' THEN $v_yj ELSE $v_yj END FROM sys_dummy";
    let (rec, calls) = Rec::new(vec![row(&["9"])]);
    rec.push(vec![row(&["9"])]);
    let ex = cache(Box::new(rec));
    let _ = ex
        .query(sql, &strs(&[("$v_fee", "11"), ("$v_yj", "9")]))
        .unwrap();
    let _ = ex
        .query(sql, &strs(&[("$v_fee", "11"), ("$v_yj", "9")]))
        .unwrap();
    assert_eq!(calls.borrow().len(), 2);
    assert_eq!(calls.borrow()[0], sql);
}

#[test]
fn should_probe_partner_semijoin_from_two_scans() {
    let sql = "SELECT t.fund_code FROM par_fund_partner AS t WHERE $v_p_i_date BETWEEN \
        t.inure_begin_date AND t.inure_end_date AND t.partner_code = $v_gddm AND \
        t.sub_partner_code = $v_sub AND t.market_code = $v_mkt AND t.fund_code IN \
        (SELECT p.fund_code FROM par_fund_seat AS p WHERE p.seat_code = $v_seat AND \
        $v_p_i_date BETWEEN p.inure_begin_date AND p.inure_end_date)";
    let (rec, calls) = Rec::new(vec![
        row(&["A0001", "S1", "001", "F001"]),
        row(&["A0001", "S1", "001", "F009"]),
        row(&["A0002", "S2", "001", "F002"]),
    ]);
    rec.push(vec![row(&["X001", "F001"]), row(&["X002", "F002"])]);
    let ex = cache(Box::new(rec));
    let binds = strs(&[
        ("$v_p_i_date", "20260105"),
        ("$v_gddm", "A0001"),
        ("$v_sub", "S1"),
        ("$v_mkt", "001"),
        ("$v_seat", "X001"),
    ]);
    let hit = ex.query(sql, &binds).unwrap().fetch_many(2);
    let mut other = binds.clone();
    other.insert("$v_gddm".into(), SpinValue::Str("A0002".into()));
    other.insert("$v_sub".into(), SpinValue::Str("S2".into()));
    other.insert("$v_seat".into(), SpinValue::Str("X002".into()));
    let second = ex.query(sql, &other).unwrap().fetch_many(2);
    assert_eq!(hit, vec![vec![SpinValue::Str("F001".into())]]);
    assert_eq!(second, vec![vec![SpinValue::Str("F002".into())]]);
    assert_eq!(calls.borrow().len(), 2, "{:?}", calls.borrow());
}

#[test]
fn should_pick_desc_seat_and_stock_kind_under_limit_1() {
    let sql = "SELECT a.commision_type FROM (SELECT s.commision_fee_type AS commision_type \
        FROM par_fund_seat_para AS s WHERE s.fund_code = $v_fund AND s.market_code = $v_mkt \
        AND s.seat_code IN ($v_seat, '000000') AND s.stock_kind IN \
        ($v_kind, concat(SUBSTRING($v_kind, 1, 2), '00'), '0000') AND $v_date BETWEEN \
        s.inure_begin_date AND s.inure_end_date ORDER BY seat_code DESC, stock_kind DESC) \
        AS a LIMIT 1";
    let (rec, calls) = Rec::new(vec![
        row(&["F1", "001", "000000", "0501", "low"]),
        row(&["F1", "001", "X009", "0000", "seat-wins"]),
        row(&["F1", "001", "X009", "0500", "kind-wins"]),
    ]);
    let ex = cache(Box::new(rec));
    let binds = strs(&[
        ("$v_fund", "F1"),
        ("$v_mkt", "001"),
        ("$v_seat", "X009"),
        ("$v_kind", "0501"),
        ("$v_date", "20260105"),
    ]);
    let rows = ex.query(sql, &binds).unwrap().fetch_many(2);
    assert_eq!(rows, vec![vec![SpinValue::Str("kind-wins".into())]]);
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn should_fall_back_when_limit_1_sort_keys_tie() {
    let sql = "SELECT k FROM t ORDER BY k LIMIT 1";
    let (rec, calls) = Rec::new(vec![row(&["a", "1"]), row(&["a", "2"])]);
    rec.push(vec![row(&["from-duck"])]);
    let ex = cache(Box::new(rec));
    let rows = ex.query(sql, &BindMap::new()).unwrap().fetch_many(2);
    assert_eq!(rows, vec![vec![SpinValue::Str("from-duck".into())]]);
    assert_eq!(calls.borrow().len(), 2);
    assert_eq!(calls.borrow()[1], sql);
}

#[test]
fn should_recognize_the_rewritten_purchase_statements() {
    let samples = [
        PURCHASE,
        "SELECT t.purchase_days FROM par_sys_purchase AS t WHERE t.security_id = $v_p_i_security_id \
         AND $v_p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date",
        "SELECT kind_id FROM swh_all_kind WHERE operation_kind = 'COMMISSION_SWITCH'",
        "SELECT k FROM t ORDER BY k LIMIT 1",
        "SELECT v FROM (SELECT v FROM t ORDER BY v DESC) LIMIT 1",
        "SELECT t.fund_code FROM par_fund_partner AS t WHERE $v_p_i_date BETWEEN t.inure_begin_date \
         AND t.inure_end_date AND t.partner_code = $v_r_get_purchase__gddm AND \
         t.sub_partner_code = $v_r_get_purchase__sub_partner_code AND \
         t.market_code = $v_r_get_purchase__market_code AND t.fund_code IN \
         (SELECT p.fund_code FROM par_fund_seat AS p WHERE p.seat_code = $v_r_get_purchase__seat_code \
         AND $v_p_i_date BETWEEN p.inure_begin_date AND p.inure_end_date)",
    ];
    for sql in samples {
        assert!(shape::recognize(sql).is_some(), "应识别: {sql}");
    }
    assert!(
        shape::recognize("SELECT CASE $v WHEN '1' THEN $a ELSE $b END FROM sys_dummy").is_none()
    );
}

#[test]
fn should_clear_on_call_boundary() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"])]);
    rec.push(vec![row(&["S1", "7"])]);
    let state = Rc::new(LookupState::default());
    let ex = LookupExecutor::new(Box::new(rec), state.clone());
    let _ = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap();
    state.clear();
    let _ = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap();
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn should_not_use_fake_executor_script_order_as_the_contract() {
    // Rec 按脚本入队顺序弹出。FakeExecutor 也是队列，这里钉住它，避免两套假执行器口径混用。
    let fake = FakeExecutor::default();
    fake.push_script(RowStream::new(vec!["a".into()], vec![row(&["first"])]));
    let got = fake
        .query("SELECT 1", &BindMap::new())
        .unwrap()
        .fetch_many(1);
    assert_eq!(got[0][0], SpinValue::Str("first".into()));
}

#[test]
fn should_not_scan_when_the_date_bind_is_null() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"])]);
    let ex = cache(Box::new(rec));
    let mut binds = purchase_binds("20260105", "S1");
    binds.insert("$v_p_i_date".into(), SpinValue::Null);
    let rows = ex.query(PURCHASE, &binds).unwrap().fetch_many(2);
    assert!(rows.is_empty());
    assert!(
        calls.borrow().is_empty(),
        "NULL 日期不得建索引: {:?}",
        calls.borrow()
    );
}

#[test]
fn should_keep_cache_across_begin_and_string_literals() {
    let (rec, calls) = Rec::new(vec![row(&["S1", "7"])]);
    let ex = cache(Box::new(rec));
    let _ = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap();
    ex.execute("BEGIN TRANSACTION", &BindMap::new()).unwrap();
    ex.execute(
        "UPDATE other SET note = 'par_sys_purchase'",
        &BindMap::new(),
    )
    .unwrap();
    let kept = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(1);
    assert_eq!(kept[0][0], SpinValue::Str("7".into()));
    assert_eq!(calls.borrow().len(), 1, "{:?}", calls.borrow());
}

#[test]
fn should_not_retry_a_failed_build_until_the_slot_is_cleared() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let ex = cache(Box::new(FailOnce {
        calls: calls.clone(),
        n: Cell::new(0),
    }));
    let first = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(1);
    let second = ex
        .query(PURCHASE, &purchase_binds("20260105", "S1"))
        .unwrap()
        .fetch_many(1);
    assert_eq!(first[0][0], SpinValue::Str("from-duck".into()));
    assert_eq!(second[0][0], SpinValue::Str("from-duck".into()));
    assert_eq!(calls.borrow().len(), 3, "{:?}", calls.borrow());
    assert_eq!(calls.borrow()[1], PURCHASE);
    assert_eq!(calls.borrow()[2], PURCHASE);
}

struct FailOnce {
    calls: Rc<RefCell<Vec<String>>>,
    n: Cell<u32>,
}

impl SqlExecutor for FailOnce {
    fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
        self.calls.borrow_mut().push(sql.to_string());
        let n = self.n.get();
        self.n.set(n + 1);
        if n == 0 {
            return Err(EngineError::invariant(1, "build down"));
        }
        Ok(RowStream::new(
            vec!["c".into()],
            vec![vec![SpinValue::Str("from-duck".into())]],
        ))
    }

    fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        Ok(ExecOutcome { rows_affected: 0 })
    }
}

#[test]
fn should_fall_back_when_a_sort_key_is_not_ascii() {
    let sql = "SELECT k FROM t ORDER BY k LIMIT 1";
    let (rec, calls) = Rec::new(vec![row(&["乙"]), row(&["甲"])]);
    rec.push(vec![row(&["from-duck"])]);
    let ex = cache(Box::new(rec));
    let rows = ex.query(sql, &BindMap::new()).unwrap().fetch_many(1);
    assert_eq!(rows, vec![vec![SpinValue::Str("from-duck".into())]]);
    assert_eq!(calls.borrow().len(), 2);
    assert_eq!(calls.borrow()[1], sql);
}

#[test]
fn should_treat_or_of_equalities_as_an_in_list() {
    let sql =
        "SELECT s.commision_fee_type FROM par_fund_seat_para AS s WHERE s.fund_code = $v_fund \
        AND s.market_code = $v_mkt AND (s.seat_code = $v_seat OR s.seat_code = '000000') AND \
        (s.stock_kind = $v_kind OR s.stock_kind = concat(SUBSTRING($v_kind, 1, 2), '00') OR \
        s.stock_kind = '0000') AND $v_date BETWEEN s.inure_begin_date AND s.inure_end_date \
        ORDER BY seat_code DESC, stock_kind DESC LIMIT 1";
    let (rec, calls) = Rec::new(vec![
        row(&["F1", "001", "000000", "0501", "low"]),
        row(&["F1", "001", "X009", "0000", "seat-wins"]),
        row(&["F1", "001", "X009", "0500", "kind-wins"]),
    ]);
    let ex = cache(Box::new(rec));
    let binds = strs(&[
        ("$v_fund", "F1"),
        ("$v_mkt", "001"),
        ("$v_seat", "X009"),
        ("$v_kind", "0501"),
        ("$v_date", "20260105"),
    ]);
    let rows = ex.query(sql, &binds).unwrap().fetch_many(2);
    assert_eq!(rows, vec![vec![SpinValue::Str("kind-wins".into())]]);
    assert_eq!(calls.borrow().len(), 1);
}
