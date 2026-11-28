//! SQL 域名字解析与 DATE 绑定守护（V60；缺口见 gap report §6.4 名字解析 1/2/3/4 与
//! 类型绑定 1，探针 G.late / C0 / D0 / X2 / D1 / G.margin.fn / PR.hoist 的守护版）。
//!
//! 参照语义（openGauss 5.0.0 A 模式实测 2026-11-28）：
//! - SQL 域内表别名遮蔽外层 PL 名——`FROM supplier s` 与 `FOR s IN` 同名时，
//!   该句 SQL 里的 `s`（表名/别名/限定前缀）按别名执行；INSERT 的 `s.field`
//!   是外层循环记录的字段访问（无 FROM 域，名字仍归 PL）。
//! - DATE 变量按日期值参与 SQL 比较与绑定（`d > v_od`、`d = v_od`、键集翻页）；
//!   PL 算术 `v_od + 1` 参照为次日（本仓 DATE 以 ISO 文本承载，算术未支持——
//!   显式拒绝并登记（现为 BIZ-0112：ISO 文本进算术器按非数字字面量拒绝），
//!   不做静默文本算术）。
//!
//! 修复前形态：变量替换不感知标识符位置——表别名/表名/INSERT 列清单被替换成
//! `$v_*`（FROM supplier AS $v_s → 运行期 ENG-0044）；DATE 绑定值是
//! `Date32(8058)` Debug 文本 → 内核 invalid date field format。
#![cfg(feature = "native")]

use spinduck_core::norm::norm_value;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn engine(ddl: &[&str]) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ]
    .iter()
    .chain(ddl.iter())
    {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn rows_text(eng: &Engine, sql: &str) -> Vec<String> {
    eng.ex
        .query(sql, &BindMap::new())
        .unwrap()
        .fetch_many(1000)
        .into_iter()
        .map(|row| row.iter().map(norm_value).collect::<Vec<_>>().join(" | "))
        .collect()
}

/// 缺口 1a（G.late 守护版）：内层游标的表别名 `s`/`n` 与记录变量 `s` 同名——
/// 游标 SQL 按别名执行，INSERT 的 `s.s_name` 按循环记录字段访问。
#[test]
fn should_execute_cursor_sql_by_table_alias_shadowing_record_name() {
    let ddl = [
        "CREATE TABLE supplier(s_suppkey INTEGER, s_name VARCHAR(20), s_nationkey INTEGER)",
        "CREATE TABLE nation(n_nationkey INTEGER, n_name VARCHAR(20))",
        "CREATE TABLE result_late(s_name VARCHAR(20))",
        "INSERT INTO supplier VALUES (1, 'A', 1), (2, 'B', 2)",
        "INSERT INTO nation VALUES (1, 'GERMANY'), (2, 'FRANCE')",
    ];
    let sql = "CREATE OR REPLACE PROCEDURE prc_late IS \
        BEGIN \
        DELETE FROM result_late; COMMIT; \
        FOR s IN (SELECT s.s_suppkey, s.s_name \
                    FROM supplier s, nation n \
                   WHERE s.s_nationkey = n.n_nationkey \
                     AND n.n_name = 'GERMANY') LOOP \
          BEGIN \
            INSERT INTO result_late(s_name) VALUES (s.s_name); \
          EXCEPTION WHEN OTHERS THEN NULL; \
          END; \
        END LOOP; \
        COMMIT; \
        END;";
    let mut eng = engine(&ddl);
    eng.register(sql).unwrap();
    eng.call("prc_late", &[]).unwrap();
    assert_eq!(
        rows_text(&eng, "SELECT s_name FROM result_late ORDER BY s_name"),
        vec!["A"]
    );
}

/// 缺口 1b（C0 / sudf_20b 守护版）：参数名与表名同名——FROM 的表名是 SQL 域，
/// WHERE 里的裸参数名是 PL 域，两者同名互不干扰。
#[test]
fn should_keep_table_name_when_parameter_shadows_it() {
    let ddl = [
        "CREATE TABLE item(i_item_sk INTEGER, i_class VARCHAR(20))",
        "INSERT INTO item VALUES (1, 'A'), (2, 'B')",
    ];
    let sql = "CREATE OR REPLACE FUNCTION f_shadow(item IN INTEGER) RETURN NUMBER IS \
         x NUMBER; BEGIN \
         SELECT COUNT(*) INTO x FROM item WHERE i_item_sk = item; RETURN x; END;";
    let mut eng = engine(&ddl);
    eng.register(sql).unwrap();
    let r = eng
        .call("f_shadow", &[SpinValue::Int(1)])
        .unwrap()
        .return_value
        .unwrap();
    assert_eq!(
        r,
        SpinValue::Int(1),
        "参数 item 只替换 WHERE 裸名，不动 FROM 表名"
    );
}

/// 缺口 1c（D0 / sudf_8 守护版）：列别名与局部变量同名——`as cnt` 的 cnt 是
/// SQL 别名定义，不替换；FETCH INTO 按位取值不受影响。
#[test]
fn should_keep_select_alias_when_same_as_variable() {
    let ddl = [
        "CREATE TABLE returns(i_class VARCHAR(20), i_item_sk INTEGER)",
        "INSERT INTO returns VALUES ('A', 1), ('A', 2), ('B', 3)",
    ];
    let sql = "CREATE OR REPLACE FUNCTION f_alias RETURN VARCHAR(20) IS \
         cnt NUMBER; cls VARCHAR(20); max_cnt NUMBER := 0; max_cls VARCHAR(20); \
         CURSOR c1 IS SELECT i_class, COUNT(i_class) AS cnt FROM returns GROUP BY i_class; \
         BEGIN \
         OPEN c1; \
         LOOP \
         FETCH c1 INTO cls, cnt; \
         EXIT WHEN c1%NOTFOUND; \
         IF cnt > max_cnt THEN max_cnt := cnt; max_cls := cls; END IF; \
         END LOOP; \
         CLOSE c1; \
         RETURN max_cls; END;";
    let mut eng = engine(&ddl);
    eng.register(sql).unwrap();
    let r = eng.call("f_alias", &[]).unwrap().return_value.unwrap();
    assert_eq!(
        r,
        SpinValue::Str("A".into()),
        "别名 cnt 不被变量替换，游标逐行取数"
    );
}

/// 缺口 1d（X2 守护版）：INSERT 列清单里的列名与循环变量同名——列清单是
/// SQL 域，VALUES 里的同名标识符是 PL 变量。
#[test]
fn should_keep_insert_column_list_when_loop_var_same_name() {
    let ddl = ["CREATE TABLE result_seq(i INTEGER, sq INTEGER)"];
    let sql = "CREATE OR REPLACE PROCEDURE prc_seq(p_n IN INTEGER) IS \
         BEGIN \
         DELETE FROM result_seq; COMMIT; \
         FOR i IN 1 .. p_n LOOP \
         INSERT INTO result_seq(i, sq) VALUES (i, i * i); \
         END LOOP; COMMIT; END;";
    let mut eng = engine(&ddl);
    eng.register(sql).unwrap();
    eng.call("prc_seq", &[SpinValue::Int(3)]).unwrap();
    assert_eq!(
        rows_text(&eng, "SELECT i, sq FROM result_seq ORDER BY i"),
        vec!["1 | 1", "2 | 4", "3 | 9"]
    );
}

/// 回归守护：`CAST(x AS <类型关键字>)` 的类型词是 Keyword——AS 别名保护
/// 若只被 Ident 消费，会武装到下一个 Ident，把 `CASE r.security_id` 的
/// 记录变量 `r` 误保护成裸名（内核 Referenced table "r" not found；
/// set_oriented_dummy scenario_expr_params Off 基线曾因此 0 行）。
#[test]
fn should_bind_record_var_after_cast_as_keyword_type() {
    let ddl = [
        "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(15,2), bs VARCHAR)",
        "CREATE TABLE dst_cast(security_id VARCHAR, yj DECIMAL(20,6), tag VARCHAR)",
        "INSERT INTO cur_rows VALUES ('S1',1000.00,'2')",
    ];
    let sql = "CREATE OR REPLACE PROCEDURE prc_cast IS \
        BEGIN \
        DELETE FROM dst_cast; COMMIT; \
        FOR r IN SELECT security_id, cjje, bs FROM cur_rows LOOP \
          INSERT INTO dst_cast(security_id, yj, tag) \
            VALUES (r.security_id, \
                    r.cjje + CAST(r.bs AS DECIMAL(10,2)) \
                           * CASE r.security_id WHEN 'S1' THEN 1 ELSE 0 END, \
                    'x'); \
        END LOOP; COMMIT; END;";
    let mut eng = engine(&ddl);
    eng.register(sql).unwrap();
    eng.call("prc_cast", &[]).unwrap();
    assert_eq!(
        rows_text(
            &eng,
            "SELECT COUNT(*), MAX(CASE WHEN yj = 1002 THEN 1 ELSE 0 END) FROM dst_cast"
        ),
        vec!["1 | 1"],
        "CAST…AS DECIMAL 的 Keyword 类型词不得吞掉 AS 窗口，r.* 字段访问须绑定"
    );
}

/// 缺口 2（G.margin.fn / PR.hoist 守护版）：DATE 变量绑定进 SQL 的比较与
/// 键集翻页（`d > v_od OR (d = v_od AND k > v_ok)`）按日期值执行。
#[test]
fn should_bind_date_variable_as_date_value_in_sql() {
    let ddl = [
        "CREATE TABLE ord(k INTEGER, d DATE)",
        "INSERT INTO ord VALUES (1, DATE '2020-01-15'), (2, DATE '2020-01-16'), (3, DATE '2020-01-17')",
    ];
    // 忠实版键集翻页：v_od DATE 参与比较（修复前绑定值是 Date32(*) Debug 文本）
    let sql = "CREATE OR REPLACE PROCEDURE p_march(p_buy OUT NUMBER) IS \
         v_ok NUMBER; v_od DATE; \
         BEGIN \
         p_buy := NULL; \
         SELECT k, d INTO v_ok, v_od FROM ord ORDER BY d, k LIMIT 1; \
         WHILE v_ok IS NOT NULL LOOP \
         p_buy := v_ok; \
         BEGIN \
         SELECT k, d INTO v_ok, v_od FROM ord \
         WHERE d > v_od OR (d = v_od AND k > v_ok) \
         ORDER BY d, k LIMIT 1; \
         EXCEPTION WHEN NO_DATA_FOUND THEN v_ok := NULL; \
         END; \
         END LOOP; \
         END;";
    let mut eng = engine(&ddl);
    eng.register(sql).unwrap();
    // call 的实参清单须含 OUT 槽位（缺实参即 BIZ-0151）
    eng.call("p_march", &[SpinValue::Null]).unwrap();
    assert_eq!(
        eng.call("p_march", &[SpinValue::Null])
            .unwrap()
            .outs
            .get("p_buy"),
        Some(&SpinValue::Int(3)),
        "DATE 绑定比较按日期值翻页至最后一行"
    );
    let mut eng2 = engine(&ddl);
    // 参照口径：翻页走完全部行，最后 p_buy = 最后一行的 k
    eng2.ex
        .execute(
            "INSERT INTO ord VALUES (4, DATE '2020-01-18')",
            &BindMap::new(),
        )
        .unwrap();
    eng2.register(sql).unwrap();
    eng2.call("p_march", &[SpinValue::Null]).unwrap();
    let last = rows_text(&eng2, "SELECT k FROM ord ORDER BY d DESC LIMIT 1");
    assert_eq!(last, vec!["4"]);
}

/// 回归守护：`IS NOT DISTINCT FROM` 的 FROM 是运算符而非子句 FROM——
/// 误判会让 expect_table 把后续 `(expr)` 当子查询组整体跳过、组内 PL 变量
/// 失去替换（fn-site `sd_a0 IS NOT DISTINCT FROM (vk)` → KERNEL-0010）。
#[test]
fn should_bind_variable_in_is_not_distinct_from() {
    let ddl = [
        "CREATE TABLE nd(a INTEGER)",
        "INSERT INTO nd VALUES (1), (NULL)",
    ];
    let mut eng = engine(&ddl);
    let sql = "CREATE OR REPLACE FUNCTION f_nd RETURN NUMBER IS \
         v NUMBER; x NUMBER; BEGIN v := 1; \
         SELECT COUNT(*) INTO x FROM nd WHERE a IS NOT DISTINCT FROM v; \
         RETURN x; END;";
    eng.register(sql).unwrap();
    // v=1：仅 a=1 命中（NULL 行 IS NOT DISTINCT FROM 1 为假）
    assert_eq!(
        eng.call("f_nd", &[]).unwrap().return_value,
        Some(SpinValue::Int(1))
    );
}

/// 回归守护：`SUBSTRING(… FROM …)` 的 FROM 是函数语法——表达式括号域内的
/// FROM 不得进入 FROM 状态，否则 FROM 后的操作数（此处 PL 变量 v）被
/// 位置保护、失去替换。
#[test]
fn should_bind_variable_inside_substring_from() {
    let ddl = [
        "CREATE TABLE nm(name VARCHAR(20))",
        "INSERT INTO nm VALUES ('BC'), ('AB')",
    ];
    let mut eng = engine(&ddl);
    let sql = "CREATE OR REPLACE FUNCTION f_sub RETURN NUMBER IS \
         v NUMBER; x NUMBER; BEGIN v := 1; \
         SELECT COUNT(*) INTO x FROM nm WHERE SUBSTRING(name FROM v) = 'BC'; \
         RETURN x; END;";
    eng.register(sql).unwrap();
    // SUBSTRING(name FROM 1) = name → 仅 'BC' 命中
    assert_eq!(
        eng.call("f_sub", &[]).unwrap().return_value,
        Some(SpinValue::Int(1))
    );
}

/// 缺口 2 边界（显式登记）：PL 侧 DATE 算术 `v_od + 1` 参照为次日，本仓 DATE
/// 以 ISO 文本承载、算术未支持——必须显式报错（BIZ-0112），不得静默文本算术。
#[test]
fn should_reject_date_arithmetic_loudly_until_supported() {
    let ddl = [
        "CREATE TABLE ord(d DATE)",
        "INSERT INTO ord VALUES (DATE '2020-01-15')",
    ];
    let mut eng = engine(&ddl);
    let sql = "CREATE OR REPLACE FUNCTION f_dplus RETURN NUMBER IS v_od DATE; x NUMBER; BEGIN \
         SELECT d INTO v_od FROM ord; x := v_od + 1; RETURN x; END;";
    eng.register(sql).unwrap();
    let error = eng.call("f_dplus", &[]).unwrap_err();
    // DATE 算术以 ISO 文本进算术器 → 非数字字面量 BIZ-112（显式拒绝，非静默）；
    // 参照语义（次日）登记为未支持（V60 已知差距）
    assert_eq!(error.code.0, "BIZ-0112", "{}", error.message);
}
