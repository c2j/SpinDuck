//! 集合化 top1 语义守护。

use super::*;

fn setup_copy_seat(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_seat IS v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, fund FROM source_rows LOOP \
        BEGIN SELECT commision_type INTO v_ct FROM par_fund_seat_para \
        WHERE fund_code = r.fund AND ROWNUM = 1 ORDER BY seat_no; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_seat_para(fund_code VARCHAR, seat_no BIGINT, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_seat(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_guard_top1_tie_consistency_at_entry() {
    // 设计 §4.4 B1 有序 Top-1：无 PK/UNIQUE 表 ORDER BY ... ROWNUM = 1
    // （编译期改写 LIMIT 1）关系化为按组 row_number 首行；入口并列一致性
    // 守卫覆盖 0 行 / 无并列 / 同值并列 / 异值并列（含 NULL）四场景。
    let converted = [
        vec![],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 2, 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, 'A')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, NULL), ('F1', 1, NULL)"],
    ];
    for seeds in converted {
        let mut baseline = setup_copy_seat(&seeds);
        baseline.call("copy_seat", &[]).unwrap();
        let expected = rows_copy_seat(&baseline);

        let mut eng = setup_copy_seat(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_seat", &[]).unwrap();
        assert_eq!(rows_copy_seat(&eng), expected, "seeds: {seeds:?}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "seeds: {seeds:?}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
    for seeds in [
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, NULL)"],
    ] {
        let mut baseline = setup_copy_seat(&seeds);
        baseline.call("copy_seat", &[]).unwrap();
        let expected = rows_copy_seat(&baseline);

        let mut eng = setup_copy_seat(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_seat", &[]).unwrap();
        assert_eq!(rows_copy_seat(&eng), expected, "seeds: {seeds:?}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::AmbiguousTop1Tie
            },
            "seeds: {seeds:?}"
        );
    }
}

fn setup_multi_column_seat(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_seat IS v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, fund FROM source_rows LOOP \
        BEGIN SELECT commision_type INTO v_ct FROM par_fund_seat_para \
        WHERE fund_code = r.fund AND ROWNUM = 1 ORDER BY seat_code DESC, stock_kind DESC; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_seat_para(fund_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_multi_column_seat(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_guard_multi_column_top1_tie_consistency_at_entry() {
    // 设计 §4.4 B1 有序 Top-1 多列 ORDER BY：并列一致性须按完整排序键
    // （seat_code DESC, stock_kind DESC）判定——首列并列由次列打破时不算
    // 歧义；完整键并列且投影异值（含 NULL）才退回 AmbiguousTop1Tie。
    let converted = [
        vec![],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 'S2', 'K1', 'A'), ('F1', 'S1', 'K1', 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 'S1', 'K2', 'A'), ('F1', 'S1', 'K1', 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 'S1', 'K1', 'A'), ('F1', 'S1', 'K1', 'A')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 'S1', 'K1', NULL), ('F1', 'S1', 'K1', NULL)"],
    ];
    for seeds in converted {
        let mut baseline = setup_multi_column_seat(&seeds);
        baseline.call("copy_seat", &[]).unwrap();
        let expected = rows_multi_column_seat(&baseline);

        let mut eng = setup_multi_column_seat(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_seat", &[]).unwrap();
        assert_eq!(rows_multi_column_seat(&eng), expected, "seeds: {seeds:?}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "seeds: {seeds:?}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
    for seeds in [
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 'S1', 'K1', 'A'), ('F1', 'S1', 'K1', 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 'S1', 'K1', 'A'), ('F1', 'S1', 'K1', NULL)"],
    ] {
        let mut baseline = setup_multi_column_seat(&seeds);
        baseline.call("copy_seat", &[]).unwrap();
        let expected = rows_multi_column_seat(&baseline);

        let mut eng = setup_multi_column_seat(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_seat", &[]).unwrap();
        assert_eq!(rows_multi_column_seat(&eng), expected, "seeds: {seeds:?}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::AmbiguousTop1Tie
            },
            "seeds: {seeds:?}"
        );
    }
}

fn setup_copy_route(seed: &str) -> Engine {
    let sql = "CREATE PROCEDURE copy_route IS v_kind VARCHAR2(2); BEGIN \
        FOR r IN SELECT a FROM source_rows LOOP \
        BEGIN SELECT kind_id INTO v_kind FROM swh_all_kind \
        WHERE operation_kind = 'X'; \
        EXCEPTION WHEN OTHERS THEN v_kind := '0'; END; \
        IF v_kind = '1' THEN \
        INSERT INTO target_rows(val) VALUES (r.a + 1000); \
        ELSE \
        INSERT INTO target_rows(val) VALUES (r.a); \
        END IF; \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT)",
        "CREATE TABLE target_rows(val BIGINT)",
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

fn rows_copy_route(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query("SELECT val FROM target_rows ORDER BY val", &BindMap::new())
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_route_else_branch_when_invariant_kind_guard_proves_then_unreachable() {
    // 设计 §4.3 区域入口循环不变探针：v_kind 由兜底查找求得（0 行/多行 →
    // '0'，命中 NULL 保留），IF v_kind = '1' 双臂路由。
    // D7 双臂：THEN 臂 `INSERT ... VALUES (r.a + 1000)` 的表达式写在
    // 臂内无残余 trap 可证「无需坏行过滤」→ 臂生成 UnsupportedWrite，
    // 按「单臂失败整体失败」整个区域拒绝转换（与判别数据无关，
    // 解释器回退后行集仍与 Off 全等）。
    for seed in [
        "INSERT INTO swh_all_kind VALUES ('1', 'OTHER')",
        "INSERT INTO swh_all_kind VALUES ('0', 'X')",
        "INSERT INTO swh_all_kind VALUES (NULL, 'X')",
        "INSERT INTO swh_all_kind VALUES ('0', 'X'), ('1', 'X')",
    ] {
        let mut baseline = setup_copy_route(seed);
        baseline.call("copy_route", &[]).unwrap();
        let expected = rows_copy_route(&baseline);

        let mut eng = setup_copy_route(seed);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_route", &[]).unwrap();
        assert_eq!(rows_copy_route(&eng), expected, "seed: {seed}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnsupportedWrite
            },
            "seed: {seed}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
    let seed = "INSERT INTO swh_all_kind VALUES ('1', 'X')";
    let mut baseline = setup_copy_route(seed);
    baseline.call("copy_route", &[]).unwrap();
    let expected = rows_copy_route(&baseline);
    assert_eq!(expected[0][0], SpinValue::Int(1010));

    let mut eng = setup_copy_route(seed);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_route", &[]).unwrap();
    assert_eq!(rows_copy_route(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedWrite
        }
    );
}

fn setup_copy_combined(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_combined IS v_p VARCHAR2(20); v_ct VARCHAR2(10); \
        v_yj VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, fund FROM source_rows LOOP \
        BEGIN \
        SELECT partner_name INTO v_p FROM par_fund_partner \
        WHERE fund_code = r.fund AND fund_code IN \
        (SELECT fund_code FROM par_fund_seat WHERE seat_type = 'X'); \
        SELECT commision_type INTO v_ct FROM par_fund_seat_para \
        WHERE fund_code = r.fund AND ROWNUM = 1 ORDER BY seat_no; \
        SELECT purchase_yj05 INTO v_yj FROM sys_dummy; \
        EXCEPTION WHEN OTHERS THEN v_p := '0'; v_ct := '0'; v_yj := '9'; END; \
        INSERT INTO target_rows(val, pname, ct, yj) VALUES (r.a, v_p, v_ct, v_yj); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, pname VARCHAR, ct VARCHAR, yj VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_partner(fund_code VARCHAR, partner_name VARCHAR)",
        "CREATE TABLE par_fund_seat(fund_code VARCHAR, seat_type VARCHAR)",
        "CREATE TABLE par_fund_seat_para(fund_code VARCHAR, seat_no BIGINT, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR, purchase_yj05 VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X', '5')",
        "INSERT INTO par_fund_partner VALUES ('F1', 'Alpha')",
        "INSERT INTO par_fund_seat VALUES ('F1', 'X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_combined(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, pname, ct, yj FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_handle_correlated_subquery_and_top1_tie_and_trap_precedence() {
    // 设计 §6.2 命名守护（三合一）：同一 WHEN OTHERS 闭包内
    // #1 含 IN (SELECT ...) 相关子查询、#2 无 PK 表 ROWNUM=1 有序 Top-1、
    // #3 sys_dummy 投影；任一前置查找抛错 → 三变量同落兜底（异常优先级，
    // #2 未命中绝不穿透进 #3 的 ELSE）；Top-1 入口并列一致性守卫覆盖
    // 0 行 / 无并列 / 同值并列（Converted）与异值并列（AmbiguousTop1Tie 退回）。
    // Converted：0 行（#2 抛错→全兜底）/ 无并列 / 同值并列（含 NULL）。
    for seeds in [
        vec![],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 2, 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, 'A')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, NULL), ('F1', 1, NULL)"],
    ] {
        let mut baseline = setup_copy_combined(&seeds);
        baseline.call("copy_combined", &[]).unwrap();
        let expected = rows_copy_combined(&baseline);
        // F2 无 partner/seat_para：#1 抛错 → v_yj 必为 '9'，绝不穿透取 '5'。
        assert_eq!(
            expected[1][3],
            SpinValue::Str("9".into()),
            "seeds: {seeds:?}"
        );
        // 0 行场景：F1 的 #2 抛错 → 即便 #1 命中 Alpha，v_p 也落 '0'（优先级）。
        if seeds.is_empty() {
            assert_eq!(expected[0][1], SpinValue::Str("0".into()));
            assert_eq!(expected[0][3], SpinValue::Str("9".into()));
        }

        let mut eng = setup_copy_combined(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_combined", &[]).unwrap();
        assert_eq!(rows_copy_combined(&eng), expected, "seeds: {seeds:?}");
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

#[test]
fn should_fall_back_on_top1_ties_in_correlated_trap_group() {
    for seeds in [
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, 'B')"],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, NULL)"],
    ] {
        let mut baseline = setup_copy_combined(&seeds);
        baseline.call("copy_combined", &[]).unwrap();
        let expected = rows_copy_combined(&baseline);

        let mut eng = setup_copy_combined(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_combined", &[]).unwrap();
        assert_eq!(rows_copy_combined(&eng), expected, "seeds: {seeds:?}");
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::AmbiguousTop1Tie
            },
            "seeds: {seeds:?}"
        );
    }
}
