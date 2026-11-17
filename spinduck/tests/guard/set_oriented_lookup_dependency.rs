//! 集合化 dependency 语义守护。

use super::*;

#[test]
fn should_skip_top1_var_filter_when_var_reassigned_in_loop() {
    // 设计 §4.4 B1 循环不变性：过滤变量在循环体内被重新赋值即非不变，
    // 批写单值携带不成立 → 不得转换（行输出仍须等于 Off 基线）。
    let sql = "CREATE PROCEDURE copy_var_mut IS v_fund VARCHAR2(10); v_ct VARCHAR2(10); BEGIN \
        v_fund := 'F1'; \
        FOR r IN SELECT a, market_code FROM source_rows LOOP \
        v_fund := 'F2'; \
        BEGIN SELECT commision_type INTO v_ct FROM dim \
        WHERE dim.fund_code = v_fund AND dim.market_code = r.market_code \
        AND ROWNUM = 1 ORDER BY seat_code DESC; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a BIGINT, market_code VARCHAR)",
            "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
            "INSERT INTO source_rows VALUES (10, 'M1'), (20, 'M2')",
            "CREATE TABLE dim(fund_code VARCHAR, market_code VARCHAR, seat_code VARCHAR, commision_type VARCHAR)",
            "INSERT INTO dim VALUES ('F1', 'M1', 'S1', 'A'), ('F2', 'M1', 'S1', 'X')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let rows_of = |eng: &Engine| {
        eng.ex
            .query(
                "SELECT val, ct FROM target_rows ORDER BY val",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20)
    };
    let mut baseline = setup();
    baseline.call("copy_var_mut", &[]).unwrap();
    let expected = rows_of(&baseline);
    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_var_mut", &[]).unwrap();
    assert_eq!(rows_of(&eng), expected);
    assert!(
        matches!(eng.set_oriented_reports()[0].status, Status::Skipped { .. }),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}
fn setup_copy_derived(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_derived IS v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, market_code FROM source_rows LOOP \
        BEGIN SELECT a.commision_type INTO v_ct FROM ( \
        SELECT s.commision_type FROM dim s WHERE s.market_code = r.market_code \
        ORDER BY s.seat_code DESC) a WHERE rownum = 1; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, market_code VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'M1'), (20, 'M2'), (30, 'M3')",
        "CREATE TABLE dim(market_code VARCHAR, seat_code VARCHAR, commision_type VARCHAR)",
        "INSERT INTO dim VALUES ('M1', 'S1', 'A'), ('M1', 'S2', 'B'), ('M2', 'S1', 'C')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_derived(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_top1_wrapped_in_derived_table() {
    // 设计 §4.4 B1（语料 v_commision_fee_type 形态）：有序 Top-1 写成
    // SELECT a.col FROM (SELECT s.col ... WHERE 相关键 ORDER BY ...) a
    // WHERE rownum = 1。编译期 rownum→外层 LIMIT 1，内层派生表保留 ORDER BY；
    // 须识别包装并关系化为按组首行（相关键 market_code，排序 seat_code DESC）。
    // M1 首行 seat DESC → S2/B；M2 → C；M3 无匹配 → '0'。
    let mut baseline = setup_copy_derived(&[]);
    baseline.call("copy_derived", &[]).unwrap();
    let expected = rows_copy_derived(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("B".into()));
    assert_eq!(expected[1][1], SpinValue::Str("C".into()));
    assert_eq!(expected[2][1], SpinValue::Str("0".into()));
    let mut eng = setup_copy_derived(&[]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_derived", &[]).unwrap();
    assert_eq!(rows_copy_derived(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // 异值并列：M1/S2 增 'T'（同排序键）→ 入口守卫拒绝。
    let seeds = ["INSERT INTO dim VALUES ('M1', 'S2', 'T')"];
    let mut baseline = setup_copy_derived(&seeds);
    baseline.call("copy_derived", &[]).unwrap();
    let expected = rows_copy_derived(&baseline);
    let mut eng = setup_copy_derived(&seeds);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_derived", &[]).unwrap();
    assert_eq!(rows_copy_derived(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
}

fn setup_copy_fund(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_fund(p_i_date IN VARCHAR2) IS v_p VARCHAR2(20); BEGIN \
        FOR r IN SELECT a, fund, seat FROM source_rows LOOP \
        BEGIN SELECT t.partner_name INTO v_p FROM fp t \
        WHERE p_i_date BETWEEN t.lo AND t.hi AND t.fund_code = r.fund \
        AND t.fund_code IN (SELECT p.fund_code FROM fs p \
        WHERE p.seat_code = r.seat AND p_i_date BETWEEN p.lo AND p.hi); \
        EXCEPTION WHEN OTHERS THEN v_p := '0'; END; \
        INSERT INTO target_rows(val, pname) VALUES (r.a, v_p); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR, seat VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, pname VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1', 'S1'), (20, 'F2', 'S2'), (30, 'F3', 'S3'), (40, 'F4', 'S4')",
        "CREATE TABLE fp(fund_code VARCHAR, partner_name VARCHAR, lo VARCHAR, hi VARCHAR)",
        "CREATE TABLE fs(fund_code VARCHAR, seat_code VARCHAR, lo VARCHAR, hi VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_fund(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, pname FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_correlated_in_semi_join_via_lateral() {
    // 设计 §4.4 B1（语料 v_fund_code 形态）：相关兜底查找的 IN 子查询本身
    // 相关（seat_code = r.seat）且含形参 BETWEEN 日期区间。独立 GROUP BY
    // 派生表无法引用 sd_cur，须改逐行 LATERAL 聚合（保留 EXISTS 半连接语义
    // 与 COUNT 计数）。0 行/多行/命中 NULL/IN 落空/区间过期均与 Off 一致。
    let arg = [SpinValue::Str("2026-01-15".into())];
    let scenarios: Vec<Vec<&str>> = vec![
        // happy：F1/S1 命中 Alpha；F2 席位不符→'0'；F3 区间过期→'0'；F4 无行→'0'。
        vec![
            "INSERT INTO fp VALUES ('F1', 'Alpha', '2026-01-01', '2026-12-31'), ('F2', 'Beta', '2026-01-01', '2026-12-31'), ('F3', 'Gamma', '2026-01-01', '2026-12-31')",
            "INSERT INTO fs VALUES ('F1', 'S1', '2026-01-01', '2026-12-31'), ('F2', 'S1', '2026-01-01', '2026-12-31'), ('F3', 'S3', '2025-01-01', '2025-12-31')",
        ],
        // 多行：F1 两行有效 → TOO_MANY_ROWS → '0'。
        vec![
            "INSERT INTO fp VALUES ('F1', 'Alpha', '2026-01-01', '2026-12-31'), ('F1', 'Alpha2', '2026-01-01', '2026-12-31')",
            "INSERT INTO fs VALUES ('F1', 'S1', '2026-01-01', '2026-12-31')",
        ],
        // 命中 NULL：partner_name 为 NULL 须保留（区别于兜底 '0'）。
        vec![
            "INSERT INTO fp VALUES ('F1', NULL, '2026-01-01', '2026-12-31')",
            "INSERT INTO fs VALUES ('F1', 'S1', '2026-01-01', '2026-12-31')",
        ],
    ];
    for (i, seeds) in scenarios.iter().enumerate() {
        let mut baseline = setup_copy_fund(seeds);
        baseline.call("copy_fund", &arg).unwrap();
        let expected = rows_copy_fund(&baseline);

        let mut eng = setup_copy_fund(seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_fund", &arg).unwrap();
        assert_eq!(
            rows_copy_fund(&eng),
            expected,
            "scenario {i}, seeds: {seeds:?}"
        );
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "scenario {i}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
    // happy 场景逐值钉住：Alpha / '0' / '0' / '0'。
    let mut eng = setup_copy_fund(&scenarios[0]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_fund", &arg).unwrap();
    let got = rows_copy_fund(&eng);
    assert_eq!(got[0][1], SpinValue::Str("Alpha".into()));
    assert_eq!(got[1][1], SpinValue::Str("0".into()));
    assert_eq!(got[2][1], SpinValue::Str("0".into()));
    assert_eq!(got[3][1], SpinValue::Str("0".into()));
}
