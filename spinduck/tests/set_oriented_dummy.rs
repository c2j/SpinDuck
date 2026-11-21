//! §5.1-7 V3-3：sys_dummy CASE 写汇 + 条件 DECIMAL 乘减（§3.3 SymEnv 子集）。
#![cfg(feature = "native")]

use spinduck_core::exec::SqlExecutor;
use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
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
            "SELECT security_id, yj, tag FROM dst_rows ORDER BY tag, security_id",
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
    scenario_view_lookup();
    scenario_inline_lookup_args();
    scenario_inline_derived_or();
    scenario_dec_wide_guard();
    scenario_dec_wide_fallback();
    scenario_miss_emu();
    scenario_miss_fallbacks();
}

/// byfund c_get_data 同型：NO_DATA_FOUND 保护的 SELECT INTO +
/// 处理器逐行覆写 OUT 参数 + INTO 变量循环外常量初始化（carryover）。
fn caller_miss() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_miss(p_o_succeed OUT VARCHAR2) IS
       v_kind VARCHAR2(4);
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, amt, tag FROM cur_rows LOOP
         BEGIN
           SELECT bond_kind INTO v_kind
             FROM bond_para
            WHERE security_id = r.security_id;
         EXCEPTION
           WHEN no_data_found THEN
             p_o_succeed := 'no:' || r.security_id;
           WHEN too_many_rows THEN
             p_o_succeed := 'many:' || r.security_id;
           WHEN OTHERS THEN
             p_o_succeed := 'err:' || r.security_id;
         END;
         INSERT INTO dst_rows(security_id, yj, tag)
           VALUES (r.security_id, r.amt, r.tag || ':' || v_kind);
       END LOOP;
     END;"
}

fn miss_ddl(bond_rows: &[&str], cur_rows: &[&str]) -> Vec<String> {
    let mut ddl: Vec<String> = BASE_DDL
        .iter()
        .map(|s| s.to_string())
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, amt DECIMAL(15,2), \
                   tag VARCHAR)"
                    .to_string()
            } else {
                s
            }
        })
        .collect();
    ddl.push("CREATE TABLE bond_para(security_id VARCHAR, bond_kind VARCHAR)".into());
    if !bond_rows.is_empty() {
        ddl.push(format!(
            "INSERT INTO bond_para VALUES {}",
            bond_rows.join(",")
        ));
    }
    ddl.push(format!(
        "INSERT INTO cur_rows VALUES {}",
        cur_rows.join(",")
    ));
    ddl
}

fn miss_refs(ddl: &[String]) -> Vec<&str> {
    ddl.iter().map(String::as_str).collect()
}

/// miss 仿真全 miss + 处理器赋值逐行均匀：Converted，carryover 初值
/// 与"最后失败行"处理器结果经批写后标量出口回填，逐行同 Off。
fn scenario_miss_emu() {
    let ddl = miss_ddl(
        &[],
        &["('S9',10.50,'a')", "('S9',20.25,'b')", "('S9',30.75,'c')"],
    );
    let routines = [caller_miss()];
    let mut off = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::Off, &routines);
    let expected = off.call("caller_miss", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 3, "Off 语义基线: {expected_rows:?}");
    assert_eq!(
        expected.outs.get("p_o_succeed"),
        Some(&SpinValue::Str("no:S9".into())),
        "Off 基线 OUT: {:?}",
        expected.outs
    );

    let mut eng = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_miss", &args()).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "全 miss 均匀必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// miss 仿真回退面：命中/落空混合（carryover 次序不可批量刻画）、
/// 维度重复（too_many_rows）、处理器赋值逐行非均匀——均须 Skipped
/// 且结果与 Off 逐行一致。
fn scenario_miss_fallbacks() {
    // 混合命中/落空。
    let ddl = miss_ddl(&["('S1','K1')"], &["('S1',10.50,'a')", "('S2',20.25,'b')"]);
    let routines = [caller_miss()];
    let mut off = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::Off, &routines);
    let expected = off.call("caller_miss", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    let mut eng = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_miss", &args()).unwrap();
    assert!(
        !matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "混合命中/落空必须回退: {:?}",
        eng.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng), expected_rows);
    assert_eq!(actual.outs, expected.outs, "回退后 OUT 须与 Off 一致");

    // 维度重复（too_many_rows）。
    let ddl = miss_ddl(
        &["('S1','K1')", "('S1','K2')"],
        &["('S1',10.50,'a')", "('S2',20.25,'b')"],
    );
    let mut off = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::Off, &routines);
    let expected = off.call("caller_miss", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    let mut eng = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_miss", &args()).unwrap();
    assert!(
        !matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "维度重复必须回退: {:?}",
        eng.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng), expected_rows);
    assert_eq!(actual.outs, expected.outs, "回退后 OUT 须与 Off 一致");

    // 全 miss 但处理器赋值逐行不同（security_id 不同 → 消息不同）。
    let ddl = miss_ddl(&[], &["('S1',10.50,'a')", "('S2',20.25,'b')"]);
    let mut off = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::Off, &routines);
    let expected = off.call("caller_miss", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    let mut eng = engine_with_routines(&miss_refs(&ddl), SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_miss", &args()).unwrap();
    assert!(
        !matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "处理器赋值非均匀必须回退: {:?}",
        eng.set_oriented_reports()
    );
    assert_eq!(dst_rows(&eng), expected_rows);
    assert_eq!(actual.outs, expected.outs, "回退后 OUT 须与 Off 一致");
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

/// 视图查找（byfund v_par_fund_check_para 同型）：UNION ALL 视图经
/// 入口物化前导落临时表，相关键 GROUP BY 派生表渲染；重复键 sd_cnt=2
/// 落兜底，与原 too_many_rows→WHEN OTHERS 语义一致。
fn caller_view_lookup() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_view_lookup(p_o_succeed OUT VARCHAR2) IS
       v_x VARCHAR2(20);
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, tag FROM cur_rows LOOP
         BEGIN
           SELECT v.val INTO v_x FROM v_dim v WHERE v.k = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_x := 'D'; END;
         INSERT INTO dst_rows(security_id, yj, tag)
           VALUES (r.security_id, r.cjje, v_x);
       END LOOP;
     END;"
}

/// ⑨ 视图查找：Converted 且逐行同 Off（K1 跨 UNION ALL 重复→兜底，
/// K4 无匹配→兜底，K2/K3 命中）。
fn scenario_view_lookup() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(20,6), \
                   yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "CREATE TABLE dim_a(k VARCHAR, val VARCHAR)",
            "CREATE TABLE dim_b(k VARCHAR, val VARCHAR)",
            "CREATE VIEW v_dim AS SELECT k, val FROM dim_a \
               UNION ALL SELECT k, val FROM dim_b",
            "INSERT INTO dim_a VALUES ('K1','va1'),('K2','va2')",
            "INSERT INTO dim_b VALUES ('K3','vb3'),('K1','vb1')",
            "INSERT INTO cur_rows VALUES \
               ('K1',1.00,0,0,'a'),('K2',2.00,0,0,'b'),\
               ('K3',3.00,0,0,'c'),('K4',4.00,0,0,'d')",
        ])
        .collect();
    let routines = [caller_view_lookup()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_view_lookup", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_view_lookup", &args()).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!("视图查找必须 Converted: {:?}", eng.set_oriented_reports());
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

/// ③ 乘减宽度 >38：静态精度超内核上限不再计划期拒绝——改由值域
/// 守卫准入，干净数据 Converted 且与 Off 逐行一致；脏数据回退见⑬。
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
    let Status::Converted { .. } = eng3.set_oriented_reports()[0].status.clone() else {
        panic!(
            "乘减超宽干净数据必须值域守卫 Converted: {:?}",
            eng3.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng3), expected_rows3, "退回后结果须与 Off 一致");
    assert_eq!(actual3.outs, expected3.outs);
}

/// fnc_dim：内联例程内 SELECT INTO（byfund fnc_com_get_seat_para 同型
/// 子集）。形参绑定调用方混合实参：记录字段 / IN 形参变量 / 字符串
/// 字面量——parse_inline 原仅接受字段实参，须扩展 Var（循环不变或
/// 查找结果变量）与 Const（字面量内联改写）两类实参。
fn fnc_dim() -> &'static str {
    "CREATE FUNCTION fnc_dim(p_k VARCHAR, p_kind VARCHAR, p_flag VARCHAR)
     RETURN VARCHAR IS
       v_x VARCHAR2(20);
     BEGIN
       BEGIN
         SELECT val INTO v_x FROM dim_t
           WHERE k = p_k AND kind = p_kind AND flag = p_flag;
       EXCEPTION WHEN OTHERS THEN v_x := 'D'; END;
       RETURN v_x;
     END;"
}

fn caller_inline_lookup() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_inline_lookup(
       p_kind VARCHAR2, p_o_succeed OUT VARCHAR2) IS
       v_x VARCHAR2(20);
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, tag FROM cur_rows LOOP
         v_x := fnc_dim(r.security_id, p_kind, 'K');
         BEGIN
           INSERT INTO dst_rows(security_id, yj, tag)
             VALUES (r.security_id, r.cjje, v_x);
         EXCEPTION WHEN OTHERS THEN NULL; END;
       END LOOP;
     END;"
}

/// ⑩ 内联 Lookup 混合实参：Var 实参（p_kind 循环不变）作静态绑定、
/// Const 实参（'K'）字面量内联、Field 实参（r.security_id）相关键——
/// Converted 且逐行同 Off。
fn scenario_inline_lookup_args() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(20,6), \
                   yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "CREATE TABLE dim_t(k VARCHAR, kind VARCHAR, flag VARCHAR, val VARCHAR)",
            "INSERT INTO dim_t VALUES \
               ('S1','A','K','hit1'),('S2','B','K','hit2'),('S1','A','X','no')",
            "INSERT INTO cur_rows VALUES \
               ('S1',1.00,0,0,'A'),('S2',2.00,0,0,'B'),\
               ('S3',3.00,0,0,'Q'),('S4',4.00,0,0,'X')",
        ])
        .collect();
    let routines = [fnc_dim(), caller_inline_lookup()];
    let call_args = [SpinValue::Str("A".into()), SpinValue::Null];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_inline_lookup", &call_args).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_inline_lookup", &call_args).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "内联 Lookup 混合实参必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// fnc_dtop：派生表 Top-1 内联查找（byfund fnc_com_get_seat_para 同型
/// 子集）——内层多列投影 + WHERE 等值 OR 组 + ORDER BY/LIMIT 1。
fn fnc_dtop() -> &'static str {
    "CREATE FUNCTION fnc_dtop(p_k VARCHAR, p_kind VARCHAR, p_flag VARCHAR)
     RETURN VARCHAR IS
       v_x VARCHAR2(20);
       v_k2 VARCHAR2(20);
     BEGIN
       v_k2 := p_flag || '0';
       BEGIN
         SELECT val INTO v_x FROM (
           SELECT val, val2, flag2 FROM dim_t2
           WHERE k = p_k
             AND (kind = p_kind OR kind = 'Z' OR kind = v_k2)
           ORDER BY flag2 DESC
         ) LIMIT 1;
       EXCEPTION WHEN OTHERS THEN v_x := 'D'; END;
       RETURN v_x;
     END;"
}

fn caller_inline_derived() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_inline_derived(
       p_kind VARCHAR2, p_o_succeed OUT VARCHAR2) IS
       v_x VARCHAR2(20);
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, tag FROM cur_rows LOOP
         v_x := fnc_dtop(r.security_id, r.tag, r.tag);
         BEGIN
           INSERT INTO dst_rows(security_id, yj, tag)
             VALUES (r.security_id, r.cjje, v_x);
         EXCEPTION WHEN OTHERS THEN NULL; END;
       END LOOP;
     END;"
}

/// ⑪ 派生 Top-1 多列内层 + OR 等值组（含表达式实参
/// p_flag||'0'）：S1 {A,Z,A0}→vz1、S2 {B,Z,B0}→vb20、
/// S3 {Q,Z,Q0}→vq3、S4 无行→'D'。
fn scenario_inline_derived_or() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .map(|s| {
            if s.starts_with("CREATE TABLE cur_rows") {
                "CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(20,6), \
                   yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR)"
            } else {
                s
            }
        })
        .chain([
            "CREATE TABLE dim_t2(k VARCHAR, kind VARCHAR, val VARCHAR, \
               val2 VARCHAR, flag2 INTEGER)",
            "INSERT INTO dim_t2 VALUES \
               ('S1','A','va1','x1',2),('S1','B','vb1','x2',5),\
               ('S1','Z','vz1','x3',9),('S2','A','va2','y1',1),('S2','B','vb2','y2',8),('S2','B0','vb20','y3',9),\
               ('S3','Q','vq3','z1',7),('S3','Q0','vq30','z2',3)",
            "INSERT INTO cur_rows VALUES \
               ('S1',1.00,0,0,'A'),('S2',2.00,0,0,'B'),\
               ('S3',3.00,0,0,'Q'),('S4',4.00,0,0,'X')",
        ])
        .collect();
    let routines = [fnc_dtop(), caller_inline_derived()];
    let call_args = [SpinValue::Str("A".into()), SpinValue::Null];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_inline_derived", &call_args).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 4, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_inline_derived", &call_args).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "派生 Top-1 + OR 组必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// DECIMAL(38,x) 聚合列 + `字段 + CASE` 混合算术（byfund `cjje +
/// CASE fffs WHEN '0' THEN accrual ELSE 0 END` 同型）：静态精度证明
/// 超 38 时按值域守卫放行——守卫滤掉的行必然也过不了目标 TRY_CAST，
/// 与解释器 trap 跳过同一批行。覆盖相消（9e29 + -9e29 = 0 可写）、
/// 真溢出（1.8e30 双侧同跳）、普通行三种结局。
fn caller_dec_wide() -> &'static str {
    "CREATE OR REPLACE PROCEDURE caller_dec_wide(p_o_succeed OUT VARCHAR2) IS
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT a.grp AS g, coalesce(SUM(a.v1),0) AS cjje,
               coalesce(SUM(a.v2),0) AS acc, a.flag AS fffs
               FROM cur2 a GROUP BY a.grp, a.flag ORDER BY MAX(CASE WHEN a.v1 > 1e20 THEN 1 ELSE 0 END), a.grp LOOP
         BEGIN
           INSERT INTO dst_rows(security_id, yj, tag)
             VALUES (r.g,
                     r.cjje + CASE r.fffs WHEN '0' THEN r.acc ELSE 0 END,
                     'd');
         EXCEPTION WHEN OTHERS THEN NULL; END;
       END LOOP;
     END;"
}

/// ⑫ DECIMAL(38,8) 宽列值域守卫（干净数据）：Converted 且逐行同 Off。
/// GA 相消行（9e29 + -9e29 = 0）专门验证守卫不误杀范围内大值。
fn scenario_dec_wide_guard() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .chain([
            "CREATE TABLE cur2(grp VARCHAR, v1 DECIMAL(38,8), \
               v2 DECIMAL(38,8), flag VARCHAR)",
            "INSERT INTO cur2 VALUES \
               ('GA','9e29','-9e29','0'),('GC','100.25','50.5','1'),\
               ('GD','10.5','20.25','0')",
        ])
        .collect();
    let routines = [caller_dec_wide()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let expected = off.call("caller_dec_wide", &args()).unwrap();
    let expected_rows = dst_rows(&off);
    assert_eq!(expected_rows.len(), 3, "Off 语义基线: {expected_rows:?}");

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let actual = eng.call("caller_dec_wide", &args()).unwrap();
    let Status::Converted { .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!(
            "DECIMAL 宽列值域守卫必须 Converted: {:?}",
            eng.set_oriented_reports()
        );
    };
    assert_eq!(dst_rows(&eng), expected_rows, "BatchAtomic 与 Off 逐行一致");
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
}

/// ⑬ 同表达式含真溢出行（GB: 9e29+9e29 超 DECIMAL(38,8) 值域）：
/// 解释器按行 INSERT 触发内核溢出→ROLLBACK 全部已写行；批写无法逐行
/// 复现回滚，bad_rows 入口守卫必须令整个区域回退——两侧结果一致
/// （ORDER BY 保证 GB 最后，其前全部回滚 → dst 为空）。
fn scenario_dec_wide_fallback() {
    let ddl: Vec<&str> = BASE_DDL
        .iter()
        .copied()
        .chain([
            "CREATE TABLE cur2(grp VARCHAR, v1 DECIMAL(38,8), \
               v2 DECIMAL(38,8), flag VARCHAR)",
            "INSERT INTO cur2 VALUES \
               ('GA','9e29','-9e29','0'),('GB','9e29','9e29','0'),\
               ('GC','100.25','50.5','1'),('GD','10.5','20.25','0')",
        ])
        .collect();
    let routines = [caller_dec_wide()];
    let mut off = engine_with_routines(&ddl, SetOrientedMode::Off, &routines);
    let off_result = off.call("caller_dec_wide", &args());
    let off_rows = dst_rows(&off);

    let mut eng = engine_with_routines(&ddl, SetOrientedMode::BatchAtomic, &routines);
    let eng_result = eng.call("caller_dec_wide", &args());
    assert!(
        matches!(eng.set_oriented_reports()[0].status, Status::Skipped { .. }),
        "含溢出行必须回退: {:?}",
        eng.set_oriented_reports()
    );
    // 回退跑同一解释器：成败与落库结果必须与 Off 完全一致。
    assert_eq!(eng_result.is_ok(), off_result.is_ok(), "成败须一致");
    assert_eq!(dst_rows(&eng), off_rows, "回退后结果须与 Off 一致");
}
