//! §5.1-4 LookupTuple：多 OUT 查找函数内联与拒绝守护（设计 §3.2）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry};

const DDL: &[&str] = &[
    "CREATE TABLE seat_para(fund_code VARCHAR, seat_code VARCHAR, ctype VARCHAR, rflag VARCHAR, rorg BIGINT)",
    "CREATE TABLE src_rows(fund_code VARCHAR, seat_code VARCHAR, tag VARCHAR)",
    "CREATE TABLE dst_rows(val VARCHAR, flag VARCHAR, org BIGINT, tag VARCHAR)",
    "CREATE TABLE sys_dummy AS SELECT 1",
    "INSERT INTO seat_para VALUES ('F1','S1','11','0',1),('F2','S2','12','1',2)",
    "INSERT INTO src_rows VALUES ('F1','S1','a'),('F2','S2','b'),('F9','S9','c')",
];

/// 合格形态：前置纯赋值 + PushTrap(WHEN OTHERS) 包裹
/// SqlQuery → 直线拷贝写齐全部 OUT → RETURN；处理器全常量兜底不 Raise。
fn tuple_fn(name: &str, flag_default: &str, org_default: &str, ret_default: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION {name}(
           p_i_fund VARCHAR2, p_i_seat VARCHAR2,
           p_o_flag OUT VARCHAR2, p_o_org OUT NUMBER
         ) RETURN VARCHAR2 IS
           v_type VARCHAR2(10); v_flag VARCHAR2(10); v_org NUMBER;
         BEGIN
           SELECT t.ctype, t.rflag, t.rorg INTO v_type, v_flag, v_org
             FROM seat_para t
            WHERE t.fund_code = p_i_fund AND t.seat_code = p_i_seat;
           p_o_flag := v_flag;
           p_o_org := v_org;
           RETURN v_type;
         EXCEPTION WHEN OTHERS THEN
           p_o_flag := '{flag_default}';
           p_o_org := {org_default};
           RETURN '{ret_default}';
         END;"
    )
}

fn caller(callee: &str) -> String {
    format!(
        "CREATE OR REPLACE PROCEDURE caller_{callee} IS
           v_ret VARCHAR2(10); v_flag VARCHAR2(10); v_org NUMBER;
         BEGIN
           FOR r IN SELECT fund_code, seat_code, tag FROM src_rows LOOP
             v_ret := {callee}(r.fund_code, r.seat_code, v_flag, v_org);
             INSERT INTO dst_rows(val, flag, org, tag)
               VALUES (v_ret, v_flag, v_org, r.tag);
           END LOOP;
         END;"
    )
}

fn engine_with(routines: &[&str]) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in DDL {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    for sql in routines {
        eng.register(sql).unwrap();
    }
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng
}

fn status(eng: &Engine) -> Status {
    eng.set_oriented_reports()[0].status.clone()
}

#[test]
fn should_inline_multi_out_lookup_tuple_and_reject_unregistered_or_partial_out() {
    // ① 合格函数：Converted；SQL 的列/谓词/CASE 兜底全部来自函数体；
    //    兜底常量不同的第二个函数必须产出不同 SQL。
    let a = tuple_fn("fnc_tuple_a", "0", "0", "00");
    let b = tuple_fn("fnc_tuple_b", "9", "9", "99");
    let mut eng = engine_with(&[a.as_str(), caller("fnc_tuple_a").as_str()]);
    eng.call("caller_fnc_tuple_a", &[]).unwrap();
    let Status::Converted { sql, .. } = status(&eng) else {
        panic!("{:?}", eng.set_oriented_reports());
    };
    for piece in [
        "ctype",
        "rflag",
        "rorg",
        "fund_code",
        "seat_code",
        "CASE WHEN",
        "sd_cnt = 1",
        "ELSE '0'",
        "ELSE '00'",
    ] {
        assert!(sql.contains(piece), "missing {piece:?} in {sql}");
    }
    let call_b = caller("fnc_tuple_b");
    let mut eng_b = engine_with(&[b.as_str(), call_b.as_str()]);
    eng_b.call("caller_fnc_tuple_b", &[]).unwrap();
    let Status::Converted { sql: sql_b, .. } = status(&eng_b) else {
        panic!("{:?}", eng_b.set_oriented_reports());
    };
    assert_ne!(sql, sql_b, "兜底常量必须进入生成 SQL");
    assert!(sql_b.contains("ELSE '9'") && sql_b.contains("ELSE '99'"));

    // ② 未注册函数：调用方不转换（Unregistered → UnsupportedCall/UnresolvedFnSite）。
    let call_missing = caller("fnc_not_here");
    let mut eng = engine_with(&[call_missing.as_str()]);
    // 未注册名连解释回退也无法执行（内核无此函数），但准入报告已记录。
    let _ = eng.call("caller_fnc_not_here", &[]);
    assert!(matches!(
        status(&eng),
        Status::Skipped {
            reason: SkipReason::UnsupportedCall | SkipReason::UnresolvedFnSite
        }
    ));

    // ③ 处理器漏赋 OUT / 处理器 Raise / 成功路径拷贝可抛错 → MultiOutCallImpure。
    let miss_out = tuple_fn("fnc_miss_out", "0", "0", "00").replace("p_o_org := 0;", "");
    let raise =
        tuple_fn("fnc_raise", "0", "0", "00").replace("RETURN '00';", "RAISE; RETURN '00';");
    let throw_copy =
        tuple_fn("fnc_throw", "0", "0", "00").replace("p_o_org := v_org;", "p_o_org := v_org + 1;");
    for (f, c) in [
        (miss_out, "fnc_miss_out"),
        (raise, "fnc_raise"),
        (throw_copy, "fnc_throw"),
    ] {
        let call = caller(c);
        let mut eng = engine_with(&[f.as_str(), call.as_str()]);
        // 拒绝路径的解释回退可能合法报错（如处理器 RAISE 上抛），
        // 契约是准入报告而非调用结果。
        let _ = eng.call(&format!("caller_{c}"), &[]);
        assert_eq!(
            status(&eng),
            Status::Skipped {
                reason: SkipReason::MultiOutCallImpure
            },
            "{c}"
        );
    }

    // ④ 函数体含 DML / 循环 / 动态 SQL / 未知调用：不作为 LookupTuple，
    //    调用方不转换。
    let dml = tuple_fn("fnc_dml", "0", "0", "00").replace(
        "p_o_flag := v_flag;",
        "DELETE FROM seat_para WHERE fund_code = 'NONE';\n           p_o_flag := v_flag;",
    );
    let looped = tuple_fn("fnc_loop", "0", "0", "00").replace(
        "SELECT t.ctype",
        "FOR x IN SELECT fund_code FROM seat_para LOOP NULL; END LOOP;\n           SELECT t.ctype",
    );
    let dyn_sql = tuple_fn("fnc_dyn", "0", "0", "00").replace(
        "SELECT t.ctype",
        "EXECUTE IMMEDIATE 'DELETE FROM seat_para WHERE fund_code = ''NONE''';\n           SELECT t.ctype",
    );
    let unknown = tuple_fn("fnc_unknown", "0", "0", "00")
        .replace("v_org NUMBER;", "v_org NUMBER; v_d VARCHAR2(10);")
        .replace(
            "SELECT t.ctype",
            "v_d := fnc_ghost(1);\n           SELECT t.ctype",
        );
    for (f, c) in [
        (dml, "fnc_dml"),
        (looped, "fnc_loop"),
        (dyn_sql, "fnc_dyn"),
        (unknown, "fnc_unknown"),
    ] {
        let call = caller(c);
        let mut eng = engine_with(&[f.as_str(), call.as_str()]);
        eng.call(&format!("caller_{c}"), &[]).unwrap();
        assert!(
            matches!(status(&eng), Status::Skipped { .. }),
            "{c}: {:?}",
            eng.set_oriented_reports()
        );
    }
}
