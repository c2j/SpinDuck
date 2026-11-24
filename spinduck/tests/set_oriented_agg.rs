//! §5.1-5 V3-4：条件无 GROUP BY 聚合查表（jyfy_temp/jsf 四分支语义）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::value::SpinValue;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry};

const DDL: &[&str] = &[
    "CREATE TABLE jyfy_temp(zqdm VARCHAR, gddm VARCHAR, bs VARCHAR, jydy VARCHAR, jyfy BIGINT, data_date VARCHAR)",
    "CREATE TABLE cur_rows(stock_kind VARCHAR, zqdm VARCHAR, gddm VARCHAR, gddmzm VARCHAR, scdm VARCHAR, bs VARCHAR, xwdm VARCHAR, jsf BIGINT, tag VARCHAR)",
    "CREATE TABLE dst_rows(jsf BIGINT, tag VARCHAR)",
    "CREATE TABLE sys_dummy AS SELECT 1 AS x",
    // ① IF 成立 + 内部门成立 + 键命中：两行求和 5+7=12
    "INSERT INTO jyfy_temp VALUES ('609100','G1','B','X1',5,'20250715'),('609100','G1','B','X1',7,'20250715')",
    // ② IF 成立 + 内部门成立 + 键不命中：0 行
    // ③ IF 成立但 gddmzm NULL：本可加总的金额在表里，证明被内层 CASE 归零
    "INSERT INTO jyfy_temp VALUES ('609300','G3','B','X3',50,'20250715')",
    // ④ IF 不成立：键命中金额存在，证明 jsf 原值保留
    "INSERT INTO jyfy_temp VALUES ('609400','G4','B','X4',99,'20250715')",
    "INSERT INTO cur_rows VALUES \
       ('0100','609100','G1','000000','001','1','X1',111,'a'),\
       ('0100','609200','G2','000000','001','-1','X2',222,'b'),\
       ('0100','609300','G3',NULL,'001','1','X3',333,'c'),\
       ('0200','609400','G4','000000','001','1','X4',444,'d')",
];

/// 模仿 prc_trd_hz_byfund 段 1：IF 门（stock_kind + zqdm BETWEEN）外层，
/// 聚合 WHERE 内含游标字段门（gddmzm/scdm）与 decode 关联键。
fn caller() -> String {
    "CREATE OR REPLACE PROCEDURE caller_agg IS
       v_jsf BIGINT;
       v_date VARCHAR2(8) := '20250715';
     BEGIN
       FOR r IN SELECT stock_kind, zqdm, gddm, gddmzm, scdm, bs, xwdm, jsf, tag FROM cur_rows LOOP
         IF r.stock_kind = '0100' AND r.zqdm BETWEEN '609100' AND '609999' THEN
           v_jsf := 0;
           BEGIN
             SELECT nvl(sum(nvl(t.jyfy, 0)), 0) INTO v_jsf
               FROM jyfy_temp t
              WHERE t.zqdm = r.zqdm
                AND t.gddm = r.gddm
                AND t.data_date = v_date
                AND t.bs = decode(r.bs, '1', 'B', '-1', 'S')
                AND t.jydy = r.xwdm
                AND r.gddmzm = '000000'
                AND r.scdm = '001';
           EXCEPTION WHEN OTHERS THEN v_jsf := 0; END;
           r.jsf := v_jsf;
         END IF;
         INSERT INTO dst_rows(jsf, tag) VALUES (r.jsf, r.tag);
       END LOOP;
     END;"
        .to_string()
}

fn engine_with(routines: &[&str], oriented: SetOrientedMode) -> Engine {
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
    eng.set_oriented = oriented;
    eng
}

fn dst_rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    let mut stream = eng
        .ex
        .query(
            "SELECT jsf, tag FROM dst_rows ORDER BY tag",
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

#[test]
fn should_lower_jyfy_aggregate_with_exact_four_case_semantics() {
    // Off 基线：四分支逐行语义
    let mut off = engine_with(&[caller().as_str()], SetOrientedMode::Off);
    off.call("caller_agg", &[]).unwrap();
    let expected = dst_rows(&off);
    assert_eq!(expected.len(), 4);
    // 四分支锚点：SUM=12 / 空集=0 / 内部门假=0(覆盖 jsf) / 外层 IF 假=原值 444
    assert_eq!(expected[0][0], SpinValue::Int(12));
    assert_eq!(expected[1][0], SpinValue::Int(0));
    assert_eq!(expected[2][0], SpinValue::Int(0));
    assert_eq!(expected[3][0], SpinValue::Int(444));

    // BatchAtomic：Converted 且结果逐行一致
    let mut eng = engine_with(&[caller().as_str()], SetOrientedMode::BatchAtomic);
    eng.call("caller_agg", &[]).unwrap();
    let Status::Converted { sql, .. } = eng.set_oriented_reports()[0].status.clone() else {
        panic!("{:?}", eng.set_oriented_reports());
    };
    assert_eq!(dst_rows(&eng), expected, "BatchAtomic 与 Off 必须逐行一致");

    // SQL 形态：聚合 CTE 只按关联键分组；decode → CASE；内外双层 CASE；
    // LEFT JOIN 未命中 COALESCE；GROUP BY 不得含 gddmzm/scdm。
    let cte = sql.split("FROM (").next().unwrap_or(&sql);
    assert!(sql.contains("jyfy_temp"), "{sql}");
    assert!(sql.contains("SUM("), "{sql}");
    assert!(sql.contains("GROUP BY"), "{sql}");
    let group_by = sql
        .split("GROUP BY")
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap();
    assert!(
        !group_by.contains("gddmzm") && !group_by.contains("scdm"),
        "{sql}"
    );
    assert!(
        group_by.contains("zqdm")
            && group_by.contains("gddm")
            && group_by.contains("bs")
            && group_by.contains("jydy"),
        "{group_by}"
    );
    assert!(sql.contains("CASE"), "{sql}");
    assert!(sql.contains("WHEN '1' THEN 'B'"), "{sql}");
    assert!(sql.contains("WHEN '-1' THEN 'S'"), "{sql}");
    assert!(sql.contains("gddmzm"), "{sql}");
    assert!(sql.contains("scdm"), "{sql}");
    assert!(sql.contains("COALESCE"), "{sql}");
    assert!(sql.contains("LEFT JOIN"), "{sql}");
    assert!(sql.contains("_cur.jsf") || sql.contains("jsf"), "{sql}");
    let _ = cte;

    // 反例（同测试内）：聚合表达式不可证全定义 → UnprovenQueryTotalness。
    let bad = caller().replace(
        "nvl(sum(nvl(t.jyfy, 0)), 0)",
        "nvl(sum(nvl(t.jyfy, 0)), 0) + r.gddm",
    );
    let mut eng_bad = engine_with(&[bad.as_str()], SetOrientedMode::BatchAtomic);
    let _ = eng_bad.call("caller_agg", &[]);
    assert_eq!(
        eng_bad.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenQueryTotalness
        }
    );
}
