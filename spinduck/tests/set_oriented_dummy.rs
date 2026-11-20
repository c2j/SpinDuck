//! §5.1-7 V3-3：sys_dummy CASE 写汇 + 条件 DECIMAL 乘减（§3.3 SymEnv 子集）。
#![cfg(feature = "native")]

use spinduck_core::exec::SqlExecutor;
use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::value::SpinValue;
use spinduck_core::{
    BindMap, DuckExecutor, Engine, EngineError, ExecOutcome, RowStream, RuleRegistry,
};
use std::cell::RefCell;
use std::rc::Rc;

/// 模仿 c_get_data 段：四条维表查找 + sys_dummy CASE 写汇 v_yj +
/// IF 门内 `v_yj := cjje * org - cjje * rate` + INSERT。
fn caller() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_dummy(p_o_succeed OUT VARCHAR2) IS
       v_commision_type VARCHAR2(4);
       v_return_rate_flag VARCHAR2(4);
       v_return_rate_org NUMBER;
       v_return_rate NUMBER;
       v_yj NUMBER;
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, yj11, yj12, tag FROM cur_rows LOOP
         BEGIN
           SELECT commision_type INTO v_commision_type
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_commision_type := '00'; END;
         BEGIN
           SELECT return_rate_flag INTO v_return_rate_flag
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_return_rate_flag := '0'; END;
         BEGIN
           SELECT return_rate_org INTO v_return_rate_org
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_return_rate_org := 0; END;
         BEGIN
           SELECT return_rate INTO v_return_rate
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_return_rate := 0; END;
         BEGIN
           SELECT CASE v_commision_type
                    WHEN '11' THEN r.yj11
                    WHEN '12' THEN r.yj12
                    ELSE 0
                  END
             INTO v_yj
             FROM sys_dummy;
         EXCEPTION WHEN OTHERS THEN v_yj := 0; END;
         IF v_return_rate_flag = '1' THEN
           v_yj := r.cjje * v_return_rate_org - r.cjje * v_return_rate;
         END IF;
         INSERT INTO dst_rows(security_id, yj, tag)
           VALUES (r.security_id, v_yj, r.tag);
       END LOOP;
     END;"
}

/// ④ §3.3 算术覆写门扩展：IF 条件/算术中的变量来自内联函数 OUT 写回
/// （逐目标 Lookup 表达式），按行代入后仍投影为 CASE WHEN … END。
fn caller_fn() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_dummy_fn(p_o_succeed OUT VARCHAR2) IS
       v_commision_type VARCHAR2(4);
       v_ret VARCHAR2(10);
       v_return_rate_flag VARCHAR2(4);
       v_return_rate_org NUMBER;
       v_return_rate NUMBER;
       v_yj NUMBER;
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, yj11, yj12, tag FROM cur_rows LOOP
         BEGIN
           SELECT commision_type INTO v_commision_type
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_commision_type := '00'; END;
         v_ret := fnc_rates(r.security_id, v_return_rate_flag,
                            v_return_rate_org, v_return_rate);
         BEGIN
           SELECT CASE v_commision_type
                    WHEN '11' THEN r.yj11
                    WHEN '12' THEN r.yj12
                    ELSE 0
                  END
             INTO v_yj
             FROM sys_dummy;
         EXCEPTION WHEN OTHERS THEN v_yj := 0; END;
         IF v_return_rate_flag = '1' THEN
           v_yj := r.cjje * v_return_rate_org - r.cjje * v_return_rate;
         END IF;
         INSERT INTO dst_rows(security_id, yj, tag)
           VALUES (r.security_id, v_yj, r.tag);
       END LOOP;
     END;"
}

/// 三个 OUT 各自由独立 trap 包裹的单列查找 + 常量兜底写回（seat_para 同型
/// 子集）：OUT 表达式折叠为逐目标 Lookup，供 IF 门内联代入。
fn fnc_rates() -> &'static str {
    "CREATE OR REPLACE FUNCTION fnc_rates(
       p_sid VARCHAR2,
       p_o_flag OUT VARCHAR2, p_o_org OUT NUMBER, p_o_rate OUT NUMBER
     ) RETURN VARCHAR2 IS
     BEGIN
       BEGIN
         SELECT return_rate_flag INTO p_o_flag
           FROM seat_para WHERE security_id = p_sid;
       EXCEPTION WHEN OTHERS THEN p_o_flag := '0'; END;
       BEGIN
         SELECT return_rate_org INTO p_o_org
           FROM seat_para WHERE security_id = p_sid;
       EXCEPTION WHEN OTHERS THEN p_o_org := 0; END;
       BEGIN
         SELECT return_rate INTO p_o_rate
           FROM seat_para WHERE security_id = p_sid;
       EXCEPTION WHEN OTHERS THEN p_o_rate := 0; END;
       RETURN p_sid;
     END;"
}

/// ⑤ INSERT 参数复杂表达式（byfund 同型子集）：`+`、简单 CASE、
/// 字符串/数值字面量、CAST…AS DECIMAL、`*`——expr::prove 语法面扩展。
fn caller_expr() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_expr(p_o_succeed OUT VARCHAR2) IS
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, yj11, yj12, bs, tag FROM cur_rows LOOP
         BEGIN
           INSERT INTO dst_rows(security_id, yj, tag)
             VALUES (r.security_id,
                     r.cjje + CAST(r.bs AS DECIMAL(10,2))
                            * CASE r.security_id
                                WHEN 'S1' THEN r.yj11
                                ELSE 0
                              END,
                     CASE r.tag WHEN 'a' THEN 'hit' ELSE r.tag END);
         EXCEPTION WHEN OTHERS THEN NULL; END;
       END LOOP;
     END;"
}

fn engine_with(ddl: &[&str], oriented: SetOrientedMode) -> Engine {
    engine_with_routines(ddl, oriented, &[caller()])
}

fn engine_with_routines(ddl: &[&str], oriented: SetOrientedMode, routines: &[&str]) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in ddl {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    for sql in routines {
        eng.register(sql).unwrap();
    }
    eng.set_oriented = oriented;
    eng
}

fn dst_rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    let mut stream = eng
        .ex
        .query(
            "SELECT security_id, yj, tag FROM dst_rows ORDER BY tag",
            &BindMap::new(),
        )
        .unwrap();
    let mut rows = Vec::new();
    loop {
        let chunk = stream.fetch_many(1024);
        if chunk.is_empty() {
            break;
        }
        rows.extend(chunk);
    }
    rows
}

fn args() -> [SpinValue; 1] {
    [SpinValue::Null]
}

/// 记录执行期 SQL 的执行器包装（区分入口守卫与批写）。
struct SeenQueries {
    inner: DuckExecutor,
    seen: Rc<RefCell<Vec<String>>>,
}

impl SqlExecutor for SeenQueries {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.seen.borrow_mut().push(sql.to_string());
        self.inner.query(sql, binds)
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.inner.execute(sql, binds)
    }
}

fn engine_seen(ddl: &[&str]) -> (Engine, Rc<RefCell<Vec<String>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut eng = Engine::new(
        Box::new(SeenQueries {
            inner: DuckExecutor::open_in_memory().unwrap(),
            seen: seen.clone(),
        }),
        RuleRegistry::default(),
    );
    for sql in ddl {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(caller()).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    (eng, seen)
}

const BASE_DDL: &[&str] = &[
    "CREATE TABLE seat_para(security_id VARCHAR, commision_type VARCHAR, \
       return_rate_flag VARCHAR, return_rate_org DECIMAL(10,4), return_rate DECIMAL(10,4))",
    "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(15,2), \
       yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR)",
    "CREATE TABLE dst_rows(security_id VARCHAR, yj DECIMAL(20,6), tag VARCHAR)",
    "CREATE TABLE sys_dummy AS SELECT 1 AS x",
];

fn ddl1() -> Vec<&'static str> {
    BASE_DDL
        .iter()
        .copied()
        .chain([
            "INSERT INTO seat_para VALUES \
               ('S1','11','1',0.0500,0.0200),\
               ('S2','12','1',0.1000,0.0100),\
               ('S3','99','0',0.5000,0.5000)",
            "INSERT INTO cur_rows VALUES \
               ('S1',1000.00,111.11,222.22,'a'),\
               ('S2',500.50,11.11,22.22,'b'),\
               ('S3',77.77,7.77,8.88,'c'),\
               ('S4',9.99,1.11,2.22,'d')",
        ])
        .collect()
}

#[test]
fn should_reject_sys_dummy_block_when_not_total_and_verify_decimal_mul_sub() {
    scenario_positive();
    scenario_unproven_case_branch();
    scenario_decimal_overflow();
    scenario_inlined_out_arith();
    scenario_expr_params();
    scenario_cursor_fnsite();
    scenario_insert_fnsite();
    scenario_agg_fnsite();
}

/// fnc_grp_mode：SQL 上下文下沉例程（游标 WHERE 内 fn_site 同型）。
fn fnc_grp_mode() -> &'static str {
    "CREATE FUNCTION fnc_grp_mode(p_grp VARCHAR) RETURN VARCHAR IS
       v_mode VARCHAR2(10);
     BEGIN
       BEGIN
         SELECT mode INTO v_mode FROM mode_map WHERE grp = p_grp;
       EXCEPTION WHEN OTHERS THEN v_mode := '0'; END;
       RETURN v_mode;
     END;"
}

/// 游标 WHERE 内例程调用（byfund c_get_data 同型）：fn_site 缓存子查询
/// 随游标过滤进入 sd_cur，须由 FnPrelude 物化后方可批写。
fn caller_fnsite() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_fnsite(p_o_succeed OUT VARCHAR2) IS
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, val, tag FROM cur_rows a
               WHERE fnc_grp_mode(a.grp) = '1' LOOP
         INSERT INTO dst_rows(security_id, yj, tag)
           VALUES (r.security_id, r.val, r.tag);
       END LOOP;
     END;"
}

/// ⑥ 游标 WHERE fn_site：Converted 且逐行同 Off（缓存表物化前导）。
fn scenario_cursor_fnsite() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, grp VARCHAR, \
                   val DECIMAL(20,6), tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "CREATE TABLE mode_map(grp VARCHAR, mode VARCHAR)",
            "INSERT INTO mode_map VALUES ('G1','1'),('G2','0')",
            "INSERT INTO cur_rows VALUES \
               ('S1','G1',10.50,'a'),('S2','G2',20.25,'b'),('S3','G1',30.75,'c')",
        ])
        .collect();
    let routines = [fnc_grp_mode(), caller_fnsite()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_fnsite", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 2, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_fnsite", &args()).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "游标 fn_site 必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// 聚合游标 + WHERE fn_site（byfund c_get_data 同型）：SUM/COALESCE/
/// GROUP BY 使 shape::select 不可解析，须走 verbatim sd_cur + 计划期
/// DESCRIBE 探测输出列 schema；WHERE fn_site 仍由 FnPrelude 物化。
fn caller_agg_fnsite() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_agg_fnsite(p_o_succeed OUT VARCHAR2) IS
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT a.grp, coalesce(SUM(a.val), 0) AS total
               FROM cur_rows a
               WHERE a.tag = 'keep' AND fnc_grp_mode(a.grp) = '1'
               GROUP BY a.grp LOOP
         BEGIN
           INSERT INTO dst_rows(security_id, yj, tag)
             VALUES (r.grp, r.total, 'agg');
         EXCEPTION WHEN OTHERS THEN NULL; END;
       END LOOP;
     END;"
}

/// ⑧ 聚合游标 verbatim：Converted 且逐行同 Off（GROUP BY 行集由内核
/// 保证，输出列类型经 DESCRIBE 探测进入投影证明）。
fn scenario_agg_fnsite() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(grp VARCHAR, val DECIMAL(15,2), \
                   tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "CREATE TABLE mode_map(grp VARCHAR, mode VARCHAR)",
            "INSERT INTO mode_map VALUES ('G1','1'),('G2','0')",
            "INSERT INTO cur_rows VALUES \
               ('G1',10.50,'keep'),('G1',30.75,'keep'),('G2',20.25,'keep'),\
               ('G1',99.00,'drop')",
        ])
        .collect();
    let routines = [fnc_grp_mode(), caller_agg_fnsite()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_agg_fnsite", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 1, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_agg_fnsite", &args()).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!("聚合游标必须 Converted: {:?}", eng.set_oriented_reports());
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// fnc_mode2：SQL 上下文下沉例程（INSERT 位 fn_site 同型，byfund
/// fnc_get_rate_info2 子集）。trap+SELECT INTO+RETURN 形态可摘要 PureExpr。
fn fnc_mode2() -> &'static str {
    "CREATE FUNCTION fnc_mode2(p_code VARCHAR, p_kind VARCHAR) RETURN VARCHAR IS
       v_mode VARCHAR2(10);
     BEGIN
       BEGIN
         SELECT mode INTO v_mode FROM mode_map2
           WHERE code = p_code AND kind = p_kind;
       EXCEPTION WHEN OTHERS THEN v_mode := '0'; END;
       RETURN v_mode;
     END;"
}

/// INSERT 参数内例程调用：`CASE fnc_mode2(r.security_id,'K') …` 编译为
/// 参数缓存子查询 + fn_site；批写须 distinct-over-sd_cur 物化前导。
fn caller_insert_fnsite() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_insert_fnsite(p_o_succeed OUT VARCHAR2) IS
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, val, tag FROM cur_rows LOOP
         BEGIN
           INSERT INTO dst_rows(security_id, yj, tag)
             VALUES (r.security_id,
                     CASE fnc_mode2(r.security_id, 'K')
                       WHEN '1' THEN r.val ELSE 0 END,
                     r.tag);
         EXCEPTION WHEN OTHERS THEN NULL; END;
       END LOOP;
     END;"
}

/// ⑦ INSERT 位 fn_site：Converted 且逐行同 Off（distinct-over-sd_cur
/// 物化前导 + 缓存子查询实参渲染）。
fn scenario_insert_fnsite() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, val DECIMAL(15,2), \
                   tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "CREATE TABLE mode_map2(code VARCHAR, kind VARCHAR, mode VARCHAR)",
            "INSERT INTO mode_map2 VALUES \
               ('S1','K','1'),('S2','K','0'),('S3','K','1')",
            "INSERT INTO cur_rows VALUES \
               ('S1',10.50,'a'),('S2',20.25,'b'),('S3',30.75,'c'),('S4',40.00,'d')",
        ])
        .collect();
    let routines = [fnc_mode2(), caller_insert_fnsite()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_insert_fnsite", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_insert_fnsite", &args()).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "INSERT fn_site 必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// ⑤ INSERT 参数复杂表达式：Converted 且逐行同 Off。
/// 覆盖 `expr::prove` 语法面：`+`、`*`、简单 CASE、字符串字面量、
/// CAST…AS DECIMAL（TRY_CAST 无抛错改写）。
fn scenario_expr_params() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(15,2), \
                   yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), bs VARCHAR, tag VARCHAR)"
            } else {
                s
            }
        })
        .chain(["INSERT INTO cur_rows VALUES \
               ('S1',1000.00,111.11,222.22,'2','a'),\
               ('S2',500.50,11.11,22.22,'3','b'),\
               ('S3',77.77,7.77,8.88,'4','c'),\
               ('S4',9.99,1.11,2.22,'5','d')"])
        .collect();
    let routines = [caller_expr()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_expr", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_expr", &args()).unwrap();
    let Status::Converted { sql } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "复杂参数表达式必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert!(sql.contains("CASE"), "{sql}");
    assert!(sql.contains("TRY_CAST"), "CAST 必须无抛错改写: {sql}");
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// ④ IF 门条件/算术变量来自内联函数 OUT 写回（Lookup 表达式）：
/// 代入后投影仍为 CASE WHEN … END，Converted 且逐行同 Off。
fn scenario_inlined_out_arith() {
    let ddl = ddl1();
    let routines = [fnc_rates(), caller_fn()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_dummy_fn", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4);

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_dummy_fn", &args()).unwrap();
    let Status::Converted { sql } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "内联 OUT 变量的 IF 算术门必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert!(sql.contains("CASE"), "{sql}");
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// ① 正例：CASE 全分支可证 + 乘减宽度 ≤38 → Converted，逐行同 Off。
/// S1: '11'→yj11=111.11，flag='1'→1000*0.05-1000*0.02=30.0000
/// S2: '12'→yj12=22.22，flag='1'→500.5*0.10-500.5*0.01=45.045
/// S3: '99'→ELSE 0，flag='0'→保留 0；S4: seat_para 无行→全兜底。
fn scenario_positive() {
    let ddl = ddl1();
    let mut off = engine_with(&ddl, SetOrientedMode::Off);
    let expected = off.call("caller_dummy", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4);
    assert_eq!(
        expected.outs.get("p_o_succeed"),
        Some(&SpinValue::Str("0".into()))
    );

    let (mut eng, _seen) = engine_seen(&ddl);
    eng.call("caller_dummy", &args()).unwrap();
    let Status::Converted { sql } = eng.set_oriented_reports()[0].status.clone() else {
        panic!("{:?}", eng.set_oriented_reports());
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    // CASE 写汇与乘减须真实下沉（ELSE 0 为业务分支，非异常兜底）。
    assert!(sql.contains("CASE"), "{sql}");
    assert!(sql.contains("sys_dummy"), "{sql}");
    assert!(sql.contains('*'), "{sql}");
}

/// ② CASE 分支列不可证（yj12 DOUBLE）：整段退回，解释器走原路径，
/// OUT 形参与 dst 行和 Off 一致。
fn scenario_unproven_case_branch() {
    let ddl2: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(15,2), \
                   yj11 DECIMAL(15,2), yj12 DOUBLE, tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "INSERT INTO seat_para VALUES ('S1','11','1',0.0500,0.0200)",
            "INSERT INTO cur_rows VALUES ('S1',1000.00,111.11,222.22,'a')",
        ])
        .collect();
    let mut off2 = engine_with(&ddl2, SetOrientedMode::Off);
    let expected2 = off2.call("caller_dummy", &args()).unwrap();
    let expected_rows2 = dst_rows(&off2);

    let mut eng2 = engine_with(&ddl2, SetOrientedMode::BatchAtomic);
    let actual2 = eng2.call("caller_dummy", &args()).unwrap();
    assert!(
        !matches!(
            eng2.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "CASE 分支不可证必须整段退回: {:?}",
        eng2.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng2), expected_rows2, "退回后结果须与 Off 一致");
    assert_eq!(actual2.outs, expected2.outs, "OUT 形参须与 Off 一致");
}

/// ③ 乘减宽度 >38：计划期 UnprovenExpression，不留运行期溢出。
/// cjje DECIMAL(30,2) * org DECIMAL(30,8)：乘积精度 30+30+1=61 > 38。
fn scenario_decimal_overflow() {
    let ddl3: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE seat_para") {
                "CREATE TABLE seat_para(security_id VARCHAR, commision_type VARCHAR, \
                   return_rate_flag VARCHAR, return_rate_org DECIMAL(30,8), return_rate DECIMAL(30,8))"
            } else if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(30,2), \
                   yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "INSERT INTO seat_para VALUES ('S1','11','1',0.05000000,0.02000000)",
            "INSERT INTO cur_rows VALUES ('S1',1000.00,111.11,222.22,'a')",
        ])
        .collect();
    let mut off3 = engine_with(&ddl3, SetOrientedMode::Off);
    let expected3 = off3.call("caller_dummy", &args()).unwrap();
    let expected_rows3 = dst_rows(&off3);

    let mut eng3 = engine_with(&ddl3, SetOrientedMode::BatchAtomic);
    let actual3 = eng3.call("caller_dummy", &args()).unwrap();
    assert_eq!(
        eng3.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenExpression
        },
        "乘减超宽必须计划期拒绝: {:?}",
        eng3.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng3), expected_rows3, "退回后结果须与 Off 一致");
    assert_eq!(actual3.outs, expected3.outs);
}
