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

fn engine_with(ddl: &[&str], oriented: SetOrientedMode) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in ddl {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(caller()).unwrap();
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
