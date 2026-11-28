//! Hirn & Grust `goto_savings`：真实函数形态的 PL 除法守护（探针 G.savings 的守护版）。
//!
//! 这个案例是 2026-11-28 除法缺口的出处：`p_pct := (1 - v_new / v_total) * 100`
//! 的节省比例，修复前与手写等价 SQL 差在第二位小数（3.14 对 3.135917）。
//!
//! 锁定方式与探针包同口径：同一内存 DuckDB 上跑两遍——解释器逐行（例程逐字
//! 取自 `golden/paper_set_oriented_probes/tpch/sql/routines/g01_goto_tpch.sql`）
//! 与手写等价 SQL（逐字取自 `golden/paper_set_oriented_probes/tpch/cases/oracle.sql`，
//! 只把 LIMIT 150 的订单窗口留给小数据），两边 `ROUND(savings_pct, 6)` 后逐值
//! 归一比对。数据是缩小的确定性夹具：覆盖换供应商（changes）与成本持平不改
//! （MIN 按 suppkey 决胜）、整除与不整除两种占比。
#![cfg(feature = "native")]

use spinduck_core::norm::norm_value;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const GOTO_SAVINGS: &str = "CREATE OR REPLACE PROCEDURE goto_savings( \
    p_orderkey IN INTEGER, p_pct OUT NUMBER, p_changes OUT NUMBER) IS \
    v_ok NUMBER; v_items NUMBER; v_l_partkey NUMBER; v_l_suppkey NUMBER; \
    v_l_quantity NUMBER; v_ps_suppkey NUMBER; v_ps_cost NUMBER; \
    v_min_cost NUMBER; v_new_supplier NUMBER; \
    v_total NUMBER := 0; v_new NUMBER := 0; v_changes NUMBER := 0; \
BEGIN \
    p_pct := NULL; p_changes := NULL; \
    BEGIN \
        SELECT o.o_orderkey INTO v_ok FROM orders o WHERE o.o_orderkey = p_orderkey; \
    EXCEPTION WHEN NO_DATA_FOUND THEN RETURN; END; \
    SELECT COUNT(*) INTO v_items FROM lineitem l WHERE l.l_orderkey = p_orderkey; \
    FOR item IN 1 .. v_items LOOP \
        SELECT l.l_partkey, l.l_suppkey, l.l_quantity \
            INTO v_l_partkey, v_l_suppkey, v_l_quantity \
            FROM lineitem l \
            WHERE l.l_orderkey = p_orderkey AND l.l_linenumber = item; \
        SELECT ps.ps_suppkey, ps.ps_supplycost \
            INTO v_ps_suppkey, v_ps_cost FROM partsupp ps \
            WHERE ps.ps_partkey = v_l_partkey AND ps.ps_suppkey = v_l_suppkey; \
        SELECT MIN(ps.ps_supplycost) INTO v_min_cost FROM partsupp ps \
            WHERE ps.ps_partkey = v_l_partkey AND ps.ps_availqty >= v_l_quantity; \
        SELECT MIN(ps.ps_suppkey) INTO v_new_supplier FROM partsupp ps \
            WHERE ps.ps_supplycost = v_min_cost AND ps.ps_partkey = v_l_partkey; \
        IF v_new_supplier <> v_ps_suppkey THEN \
            v_changes := v_changes + 1; \
        END IF; \
        v_total := v_total + v_ps_cost * v_l_quantity; \
        v_new := v_new + v_min_cost * v_l_quantity; \
    END LOOP; \
    p_pct := (1 - v_new / v_total) * 100; \
    p_changes := v_changes; \
END;";

const DRIVER: &str = "CREATE OR REPLACE PROCEDURE prc_goto_savings IS \
    v_pct NUMBER; v_changes NUMBER; \
BEGIN \
    DELETE FROM result_goto_savings; COMMIT; \
    FOR o IN (SELECT o_orderkey FROM orders WHERE o_orderstatus = 'O' \
              ORDER BY o_orderkey LIMIT 150) LOOP \
        BEGIN \
            goto_savings(o.o_orderkey, v_pct, v_changes); \
            INSERT INTO result_goto_savings(o_orderkey, savings_pct, changes) \
                VALUES (o.o_orderkey, v_pct, v_changes); \
        EXCEPTION WHEN OTHERS THEN NULL; \
        END; \
    END LOOP; \
    COMMIT; \
END;";

const ORACLE_SQL: &str = "INSERT INTO oracle_goto_savings \
WITH li AS ( \
    SELECT l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity FROM lineitem \
    WHERE l_orderkey IN (SELECT o_orderkey FROM orders WHERE o_orderstatus = 'O' \
                         ORDER BY o_orderkey LIMIT 150)), \
cur AS ( \
    SELECT li.*, ps.ps_supplycost AS cur_cost FROM li, partsupp ps \
    WHERE ps.ps_partkey = li.l_partkey AND ps.ps_suppkey = li.l_suppkey), \
mn AS ( \
    SELECT cur.*, (SELECT MIN(ps.ps_supplycost) FROM partsupp ps \
        WHERE ps.ps_partkey = cur.l_partkey AND ps.ps_availqty >= cur.l_quantity) AS min_cost \
    FROM cur), \
nw AS ( \
    SELECT mn.*, (SELECT MIN(ps.ps_suppkey) FROM partsupp ps \
        WHERE ps.ps_supplycost = mn.min_cost AND ps.ps_partkey = mn.l_partkey) AS new_supp \
    FROM mn) \
SELECT l_orderkey AS o_orderkey, \
    (1 - SUM(min_cost * l_quantity) / SUM(cur_cost * l_quantity)) * 100 AS savings_pct, \
    SUM(CASE WHEN new_supp <> l_suppkey THEN 1 ELSE 0 END) AS changes \
FROM nw GROUP BY l_orderkey ORDER BY l_orderkey";

/// 缩小版夹具：订单 1 两条明细（一条换供应商、一条成本持平不改），
/// 订单 2 一条明细（换供应商）；订单 3 是 'F'，驱动不过滤进来。
fn setup() -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE orders(o_orderkey INTEGER, o_orderstatus CHAR(1))",
        "CREATE TABLE lineitem(l_orderkey INTEGER, l_linenumber INTEGER, \
         l_partkey INTEGER, l_suppkey INTEGER, l_quantity DECIMAL(15,2))",
        "CREATE TABLE partsupp(ps_partkey INTEGER, ps_suppkey INTEGER, \
         ps_availqty INTEGER, ps_supplycost DECIMAL(15,2))",
        "CREATE TABLE result_goto_savings(o_orderkey INTEGER, \
         savings_pct DECIMAL(38,10), changes INTEGER)",
        "CREATE TABLE oracle_goto_savings(o_orderkey INTEGER, \
         savings_pct DECIMAL(38,10), changes INTEGER)",
        "INSERT INTO orders VALUES (1, 'O'), (2, 'O'), (3, 'F')",
        "INSERT INTO lineitem VALUES \
         (1, 1, 101, 1, 3.00), (1, 2, 102, 1, 7.00), (2, 1, 101, 1, 4.00)",
        "INSERT INTO partsupp VALUES \
         (101, 1, 100, 10.00), (101, 2, 50, 7.50), (101, 3, 200, 12.00), \
         (102, 1, 10, 5.00), (102, 2, 99, 5.00)",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(GOTO_SAVINGS).unwrap();
    eng.register(DRIVER).unwrap();
    eng
}

fn normalized_rows(eng: &Engine, table: &str) -> Vec<Vec<String>> {
    let sql = format!(
        "SELECT o_orderkey, ROUND(savings_pct, 6) AS pct, changes \
         FROM {table} ORDER BY o_orderkey"
    );
    eng.ex
        .query(&sql, &BindMap::new())
        .unwrap()
        .fetch_many(100)
        .into_iter()
        .map(|row| row.iter().map(norm_value).collect())
        .collect()
}

#[test]
fn should_match_pl_savings_with_handwritten_sql_reference() {
    let mut eng = setup();
    eng.call("prc_goto_savings", &[]).unwrap();
    eng.ex.execute(ORACLE_SQL, &BindMap::new()).unwrap();

    let pl = normalized_rows(&eng, "result_goto_savings");
    let oracle = normalized_rows(&eng, "oracle_goto_savings");
    // 订单 1：明细 1 换供应商（10.00 → 7.50）、明细 2 成本持平（MIN 按 suppkey
    // 决胜留原商）；占比 (1 - 57.50/65.00)*100 不整除。订单 2：(1 - 30/40)*100。
    assert_eq!(
        pl,
        vec![
            vec!["1".to_string(), "11.538462".to_string(), "1".to_string()],
            vec!["2".to_string(), "25".to_string(), "1".to_string()],
        ]
    );
    assert_eq!(pl, oracle, "PL 例程与手写等价 SQL 必须逐值一致");
}

#[test]
fn should_match_pl_savings_on_null_order_via_reference() {
    // 不存在的订单：OUT 保持 NULL——与参照侧同一窗口也无该行一致。
    // call 的实参清单须含 OUT 槽位（缺实参即 BIZ-0151）。
    let mut eng = setup();
    let r = eng
        .call(
            "goto_savings",
            &[SpinValue::Int(999), SpinValue::Null, SpinValue::Null],
        )
        .unwrap();
    assert_eq!(r.outs.get("p_pct"), Some(&SpinValue::Null));
    assert_eq!(r.outs.get("p_changes"), Some(&SpinValue::Null));
}
