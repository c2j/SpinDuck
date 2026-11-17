//! 集合化 cardinality 语义守护。

use super::*;

#[test]
fn should_reject_loop_invariant_with_target_read_dependency() {
    // 设计 §4.4 B1：每轮 SELECT COUNT(*) FROM 目标表虽无游标列引用，
    // 却随每轮 INSERT 变化（读自己写）；同一事务快照不消除该依赖，
    // 必须 TargetReadDependency 拒绝提升，行为与 Off 逐行一致。
    let sql = "CREATE PROCEDURE copy_cnt IS v NUMBER; BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        SELECT COUNT(*) INTO v FROM target_rows; \
        INSERT INTO target_rows(val) VALUES (r.a); \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(sql).unwrap();
    baseline.call("copy_cnt", &[]).unwrap();
    let expected = rows(&baseline);
    assert_eq!(expected.len(), 3);

    let mut eng = engine();
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_cnt", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::TargetReadDependency
        }
    );
}
fn setup_copy_kind(seed: &str) -> Engine {
    let sql = "CREATE PROCEDURE copy_kind IS v_kind VARCHAR2(10); BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        BEGIN SELECT kind_id INTO v_kind FROM swh_all_kind \
        WHERE operation_kind = 'COMMISSION_SWITCH'; \
        EXCEPTION WHEN OTHERS THEN v_kind := '0'; END; \
        INSERT INTO target_rows(val, kind) VALUES (r.a, v_kind); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT)",
        "CREATE TABLE target_rows(val BIGINT, kind VARCHAR)",
        "INSERT INTO source_rows VALUES (10), (20)",
        "CREATE TABLE swh_all_kind(kind_id VARCHAR, operation_kind VARCHAR)",
        seed,
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_kind(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, kind FROM target_rows ORDER BY rowid",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_handle_null_zero_multi_row_in_loop_invariant_guard() {
    // 设计 §6.2 命名守护：swh_all_kind 空表 / 2 行 / kind_id IS NULL /
    // 单行命中，兜底 '0' 与 NULL 保留语义必须与 Off 逐行一致。
    for seed in [
        "INSERT INTO swh_all_kind VALUES ('1', 'OTHER')",
        "INSERT INTO swh_all_kind VALUES ('1', 'COMMISSION_SWITCH'), ('0', 'COMMISSION_SWITCH')",
        "INSERT INTO swh_all_kind VALUES (NULL, 'COMMISSION_SWITCH')",
        "INSERT INTO swh_all_kind VALUES ('1', 'COMMISSION_SWITCH')",
    ] {
        let mut baseline = setup_copy_kind(seed);
        baseline.call("copy_kind", &[]).unwrap();
        let expected = rows_copy_kind(&baseline);

        let mut eng = setup_copy_kind(seed);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_kind", &[]).unwrap();
        assert_eq!(rows_copy_kind(&eng), expected, "seed: {seed}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "seed: {seed}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
}

fn setup_copy_partner(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_partner IS v_p VARCHAR2(20); BEGIN \
        FOR r IN SELECT a, fund FROM source_rows LOOP \
        BEGIN SELECT partner_name INTO v_p FROM par_fund_partner \
        WHERE fund_code = r.fund AND fund_code IN \
        (SELECT fund_code FROM par_fund_seat WHERE seat_type = 'X'); \
        EXCEPTION WHEN OTHERS THEN v_p := '0'; END; \
        INSERT INTO target_rows(val, pname) VALUES (r.a, v_p); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, pname VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_partner(fund_code VARCHAR, partner_name VARCHAR)",
        "CREATE TABLE par_fund_seat(fund_code VARCHAR, seat_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_partner(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, pname FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_in_subquery_filter_with_transitive_read_disjoint() {
    // 设计 §4.4 B1：含 IN (SELECT ...) 相关子查询的兜底查找整体封装为
    // GROUP BY 派生表；子查询读表（par_fund_seat）纳入传递读写不交与
    // 入口漂移复核。0 行 / 多行 / 命中 NULL 语义与 Off 一致。
    for seeds in [
        vec![
            "INSERT INTO par_fund_partner VALUES ('F1', 'Alpha')",
            "INSERT INTO par_fund_seat VALUES ('F1', 'X')",
        ],
        vec!["INSERT INTO par_fund_partner VALUES ('F1', 'Alpha')"],
        vec![
            "INSERT INTO par_fund_partner VALUES ('F1', 'Alpha'), ('F1', 'Beta')",
            "INSERT INTO par_fund_seat VALUES ('F1', 'X')",
        ],
        vec![
            "INSERT INTO par_fund_partner VALUES ('F1', NULL)",
            "INSERT INTO par_fund_seat VALUES ('F1', 'X')",
        ],
    ] {
        let mut baseline = setup_copy_partner(&seeds);
        baseline.call("copy_partner", &[]).unwrap();
        let expected = rows_copy_partner(&baseline);

        let mut eng = setup_copy_partner(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_partner", &[]).unwrap();
        assert_eq!(rows_copy_partner(&eng), expected, "seeds: {seeds:?}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "seeds: {seeds:?}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
}

fn setup_copy_days(seed: &str) -> Engine {
    let sql = "CREATE PROCEDURE copy_days IS v_days VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, mkt FROM source_rows LOOP \
        BEGIN SELECT purchase_days INTO v_days FROM par_sys_purchase \
        WHERE market_code = r.mkt; \
        EXCEPTION WHEN OTHERS THEN v_days := '0'; END; \
        INSERT INTO target_rows(val, day_cnt) VALUES (r.a, v_days); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, mkt VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, day_cnt VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'SH'), (20, 'SZ')",
        "CREATE TABLE par_sys_purchase(market_code VARCHAR, purchase_days VARCHAR)",
        seed,
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_days(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, day_cnt FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_preserve_nullable_lookup_value_distinct_from_fallback_default() {
    // 设计 §6.2 命名守护：par_sys_purchase 单行命中且 purchase_days IS NULL
    // 时，CASE WHEN __cnt = 1 THEN val ELSE '0' 必须保留 NULL（不得 COALESCE
    // 误替换为 '0'）；0 行 / 同键 2 行才落兜底 '0'。
    // 验收口径为规范排序后的行多重集（设计 §6.2 哈希比对同口径）：
    // LEFT JOIN 批写不承诺插入顺序，按唯一键 val 排序比对。
    for seed in [
        "INSERT INTO par_sys_purchase VALUES ('SH', NULL)",
        "INSERT INTO par_sys_purchase VALUES ('SH', '5'), ('SH', '7')",
        "INSERT INTO par_sys_purchase VALUES ('SZ', '3')",
        "INSERT INTO par_sys_purchase VALUES ('SH', '5'), ('SZ', NULL)",
    ] {
        let mut baseline = setup_copy_days(seed);
        baseline.call("copy_days", &[]).unwrap();
        let expected = rows_copy_days(&baseline);

        let mut eng = setup_copy_days(seed);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_days", &[]).unwrap();
        assert_eq!(rows_copy_days(&eng), expected, "seed: {seed}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "seed: {seed}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
}
