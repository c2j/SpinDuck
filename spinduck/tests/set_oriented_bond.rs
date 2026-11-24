//! §5.1-6 V3-5：v_par_bond 零异常入口守卫（carryover 处理器整段退回）。
#![cfg(feature = "native")]

use spinduck_core::exec::SqlExecutor;
use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::value::SpinValue;
use spinduck_core::{
    BindMap, DuckExecutor, Engine, EngineError, ExecOutcome, RowStream, RuleRegistry,
};
use std::cell::RefCell;
use std::rc::Rc;

const BASE_DDL: &[&str] = &[
    "CREATE TABLE par_sys_bond(security_id VARCHAR, bond_kind VARCHAR, inure_begin_date VARCHAR, inure_end_date VARCHAR)",
    "CREATE VIEW v_par_bond AS SELECT security_id, bond_kind, inure_begin_date, inure_end_date FROM par_sys_bond",
    "CREATE TABLE cur_rows(security_id VARCHAR, sec_name VARCHAR, tag VARCHAR)",
    "CREATE TABLE dst_rows(security_id VARCHAR, kind VARCHAR, tag VARCHAR)",
    "CREATE TABLE sys_dummy AS SELECT 1 AS x",
];

/// 模仿 c_get_data 段 1：trap 包裹的 v_par_bond 查找，WHEN OTHERS 处理器
/// 不改 v_temp_stock_kind（跨行残留），只写 p_o_succeed 后继续 INSERT。
fn caller(view: &str) -> String {
    format!(
        "CREATE OR REPLACE PROCEDURE caller_bond(p_i_date VARCHAR2, p_o_succeed OUT VARCHAR2) IS
           v_temp_stock_kind VARCHAR2(10);
         BEGIN
           v_temp_stock_kind := 'INIT';
           p_o_succeed := '0';
           FOR r IN SELECT security_id, sec_name, tag FROM cur_rows LOOP
             BEGIN
               SELECT bond_kind INTO v_temp_stock_kind
                 FROM {view}
                WHERE security_id = r.security_id
                  AND p_i_date BETWEEN inure_begin_date AND inure_end_date;
             EXCEPTION WHEN OTHERS THEN
               p_o_succeed := 'BOND_ERR';
             END;
             INSERT INTO dst_rows(security_id, kind, tag)
               VALUES (r.security_id, v_temp_stock_kind, r.tag);
           END LOOP;
         END;"
    )
}

fn engine_with(ddl: &[&str], routines: &[&str], oriented: SetOrientedMode) -> Engine {
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
            "SELECT security_id, kind, tag FROM dst_rows ORDER BY tag",
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

fn args() -> [SpinValue; 2] {
    [SpinValue::Str("20250715".into()), SpinValue::Null]
}

/// 记录入口守卫 SQL 的执行器包装（守卫只在 entry_ok 阶段发出）。
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

fn engine_seen(ddl: &[&str], routines: &[&str]) -> (Engine, Rc<RefCell<Vec<String>>>) {
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
    for sql in routines {
        eng.register(sql).unwrap();
    }
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    (eng, seen)
}

#[test]
fn should_fallback_when_v_par_bond_has_zero_overlapping_or_query_error() {
    // ---- ① 每个 security_id 恰 1 行覆盖 p_i_date：Converted ----
    let ddl1: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .chain([
            "INSERT INTO par_sys_bond VALUES \
               ('S1','K1','20200101','20291231'),\
               ('S2','K2','20200101','20291231'),\
               ('S3','K3','20200101','20291231')",
            "INSERT INTO cur_rows VALUES ('S1','n1','a'),('S2','n2','b'),('S3','n3','c')",
        ])
        .collect();
    let routine = caller("v_par_bond");

    let mut off = engine_with(&ddl1, &[routine.as_str()], SetOrientedMode::Off);
    let expected = off.call("caller_bond", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 3);
    assert_eq!(
        expected.outs.get("p_o_succeed"),
        Some(&SpinValue::Str("0".into()))
    );

    let (mut eng, seen) = engine_seen(&ddl1, &[routine.as_str()]);
    eng.call("caller_bond", &args()).unwrap();
    let Status::Converted { sql, .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!("{:?}", eng.set_oriented_reports());
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");

    // 守卫 SQL：子查询先做日期过滤、仅按 security_id 计数、外层
    // COALESCE(__cnt,0) <> 1；GROUP BY 不得含日期列。
    let guard = seen
        .borrow()
        .iter()
        .find(|q| q.contains("__cnt"))
        .unwrap_or_else(|| panic!("缺少 __cnt 守卫: {:?}", seen.borrow()))
        .clone();
    let group_by = guard.split("GROUP BY").nth(1).unwrap();
    assert!(group_by.contains("security_id"), "{guard}");
    assert!(
        !group_by.contains("inure_begin_date") && !group_by.contains("inure_end_date"),
        "{guard}"
    );
    let before_group = guard.split("GROUP BY").next().unwrap();
    assert!(before_group.contains("inure_begin_date"), "{guard}");
    assert!(before_group.contains("inure_end_date"), "{guard}");
    assert!(guard.contains("COALESCE"), "{guard}");
    assert!(guard.contains("<> 1"), "{guard}");

    // 主 SQL：1:1 LEFT JOIN；不用 MAX / HAVING / 外层 __cnt=1 滤行。
    assert!(sql.contains("LEFT JOIN"), "{sql}");
    assert!(sql.contains("v_par_bond"), "{sql}");
    assert!(!sql.contains("MAX("), "{sql}");
    assert!(!sql.contains("HAVING"), "{sql}");
    assert!(!sql.contains("__cnt"), "{sql}");

    // ---- ② 缺 1 行（no_data_found）：守卫退回，残留与文案同 Off ----
    let ddl2: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .chain([
            "INSERT INTO par_sys_bond VALUES \
               ('S1','K1','20200101','20291231'),\
               ('S3','K3','20200101','20291231')",
            "INSERT INTO cur_rows VALUES ('S1','n1','a'),('S2','n2','b'),('S3','n3','c')",
        ])
        .collect();
    let mut off2 = engine_with(&ddl2, &[routine.as_str()], SetOrientedMode::Off);
    let expected2 = off2.call("caller_bond", &args()).unwrap();
    let expected_rows2 = dst_rows(&off2);

    let mut eng2 = engine_with(&ddl2, &[routine.as_str()], SetOrientedMode::BatchAtomic);
    let actual2 = eng2.call("caller_bond", &args()).unwrap();
    assert!(
        !matches!(
            eng2.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "缺行必须整段退回: {:?}",
        eng2.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng2), expected_rows2, "退回后结果须与 Off 一致");
    assert_eq!(
        actual2.outs, expected2.outs,
        "p_o_succeed 文案须与 Off 一致"
    );

    // ---- ③ 同 security_id 两段重叠日期均覆盖 p_i_date（too_many_rows）----
    let ddl3: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .chain([
            "INSERT INTO par_sys_bond VALUES \
               ('S1','K1','20200101','20291231'),\
               ('S1','K1B','20250101','20261231'),\
               ('S2','K2','20200101','20291231'),\
               ('S3','K3','20200101','20291231')",
            "INSERT INTO cur_rows VALUES ('S1','n1','a'),('S2','n2','b'),('S3','n3','c')",
        ])
        .collect();
    let mut off3 = engine_with(&ddl3, &[routine.as_str()], SetOrientedMode::Off);
    let expected3 = off3.call("caller_bond", &args()).unwrap();
    let expected_rows3 = dst_rows(&off3);

    let mut eng3 = engine_with(&ddl3, &[routine.as_str()], SetOrientedMode::BatchAtomic);
    let actual3 = eng3.call("caller_bond", &args()).unwrap();
    assert!(
        !matches!(
            eng3.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "重叠行必须整段退回: {:?}",
        eng3.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng3), expected_rows3);
    assert_eq!(actual3.outs, expected3.outs);

    // ---- ④ 视图证明链推进后仍拒绝：不发守卫 SQL ----
    // CAST(bond_kind AS INTEGER) 使 prove_view 全定义证明失败；视图
    // 物化前导可解析该视图，证明推进至投影类型约束才拒绝
    // （UnprovenTargetConstraints），同样不发守卫 SQL。
    let mut ddl4: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .filter(|s| !s.starts_with("CREATE VIEW"))
        .collect();
    ddl4.push(
        "CREATE VIEW v_par_bond AS SELECT security_id, CAST(bond_kind AS INTEGER) AS bond_kind, inure_begin_date, inure_end_date FROM par_sys_bond",
    );
    ddl4.extend([
        "INSERT INTO par_sys_bond VALUES ('S1','K1','20200101','20291231')",
        "INSERT INTO cur_rows VALUES ('S1','n1','a')",
    ]);
    let mut off4 = engine_with(&ddl4, &[routine.as_str()], SetOrientedMode::Off);
    let expected4 = off4.call("caller_bond", &args()).unwrap();
    let expected_rows4 = dst_rows(&off4);

    let (mut eng4, seen4) = engine_seen(&ddl4, &[routine.as_str()]);
    let actual4 = eng4.call("caller_bond", &args()).unwrap();
    assert_eq!(
        eng4.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenTargetConstraints
        }
    );
    assert!(
        !seen4.borrow().iter().any(|q| q.contains("__cnt")),
        "证明失败不得发出守卫 SQL: {:?}",
        seen4.borrow()
    );
    assert_eq!(dst_rows(&eng4), expected_rows4);
    assert_eq!(actual4.outs, expected4.outs);
}
