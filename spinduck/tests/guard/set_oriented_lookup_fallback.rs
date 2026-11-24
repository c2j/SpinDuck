//! 集合化 fallback 语义守护。

use super::*;

fn setup_copy_case(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_case IS v_ct VARCHAR2(10); v_yj VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, fund, yj_a, yj_b, yj_def FROM source_rows LOOP \
        BEGIN \
        SELECT commision_type INTO v_ct FROM par_fund_seat_para \
        WHERE fund_code = r.fund AND ROWNUM = 1 ORDER BY seat_no; \
        SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_def END \
        INTO v_yj FROM sys_dummy; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; v_yj := '9'; END; \
        INSERT INTO target_rows(val, ct, yj) VALUES (r.a, v_ct, v_yj); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR, yj_a VARCHAR, yj_b VARCHAR, yj_def VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR, yj VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1', 'AA', 'BB', 'DD'), (20, 'F2', 'XX', 'YY', 'ZZ')",
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

fn rows_copy_case(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct, yj FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_project_case_sink_over_cursor_fields_gated_by_operand_lookup() {
    // 设计 §4.4 B1 异构 CASE 写汇：同一 WHEN OTHERS 闭包内 Top-1 操作数
    // v_ct + `SELECT CASE v_ct WHEN ... THEN r.f ... ELSE r.f END INTO v_yj
    // FROM sys_dummy`。CASE 分支取游标字段，操作数取同组查找的命中值
    // （含 NULL→ELSE）；任一前置抛错 → v_yj 同落兜底，绝不穿透取分支值。
    // F1 命中 'A'/'B'/未匹配 ELSE/NULL→ELSE；F2 无 seat_para → 全兜底。
    for (seeds, want_yj) in [
        (
            vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A')"],
            "AA",
        ),
        (
            vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'B')"],
            "BB",
        ),
        (
            vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'C')"],
            "DD",
        ),
        (
            vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, NULL)"],
            "DD",
        ),
    ] {
        let mut baseline = setup_copy_case(&seeds);
        baseline.call("copy_case", &[]).unwrap();
        let expected = rows_copy_case(&baseline);
        assert_eq!(
            expected[0][2],
            SpinValue::Str(want_yj.into()),
            "seeds: {seeds:?}"
        );
        assert_eq!(
            expected[1][2],
            SpinValue::Str("9".into()),
            "seeds: {seeds:?}"
        );

        let mut eng = setup_copy_case(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_case", &[]).unwrap();
        assert_eq!(rows_copy_case(&eng), expected, "seeds: {seeds:?}");
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
fn should_default_case_sink_when_top1_has_no_rows() {
    {
        let seeds: Vec<&str> = vec![];
        let mut baseline = setup_copy_case(&seeds);
        baseline.call("copy_case", &[]).unwrap();
        let expected = rows_copy_case(&baseline);
        assert_eq!(expected[0][2], SpinValue::Str("9".into()));
        let mut eng = setup_copy_case(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_case", &[]).unwrap();
        assert_eq!(rows_copy_case(&eng), expected);
        assert!(matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ));
    }
}

#[test]
fn should_fall_back_before_case_sink_on_ambiguous_top1_tie() {
    {
        let seeds = vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 1, 'B')"];
        let mut baseline = setup_copy_case(&seeds);
        baseline.call("copy_case", &[]).unwrap();
        let expected = rows_copy_case(&baseline);
        let mut eng = setup_copy_case(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_case", &[]).unwrap();
        assert_eq!(rows_copy_case(&eng), expected);
        assert_eq!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::AmbiguousTop1Tie
            }
        );
    }
}

fn setup_copy_nested(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_nested IS v_ct VARCHAR2(10); v_yj VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, fund FROM source_rows LOOP \
        BEGIN \
        SELECT commision_type INTO v_ct FROM par_fund_seat_para \
        WHERE fund_code = r.fund AND ROWNUM = 1 ORDER BY seat_no; \
        SELECT purchase_yj05 INTO v_yj FROM sys_dummy; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; v_yj := '9'; END; \
        INSERT INTO target_rows(val, ct, yj) VALUES (r.a, v_ct, v_yj); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, fund VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR, yj VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'F1'), (20, 'F2')",
        "CREATE TABLE par_fund_seat_para(fund_code VARCHAR, seat_no BIGINT, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR, purchase_yj05 VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X', '5')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_nested(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct, yj FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_gate_second_lookup_on_first_hit_in_nested_trap() {
    // 设计 §4.4 B1 嵌套异常优先级：同一 WHEN OTHERS 内连续两条 SqlQuery，
    // #1（par_fund_seat_para）0 行/多行抛错时 #2（sys_dummy 投影）绝不执行，
    // 两变量同落兜底；合并后 v_yj 必须以 __cnt_1 = 1 前置门控，
    // 不得让 #1 未命中穿透进 sys_dummy 的 ELSE 分支。
    for seeds in [
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A')"],
        vec![],
        vec!["INSERT INTO par_fund_seat_para VALUES ('F1', 1, 'A'), ('F1', 2, 'B')"],
    ] {
        let mut baseline = setup_copy_nested(&seeds);
        baseline.call("copy_nested", &[]).unwrap();
        let expected = rows_copy_nested(&baseline);
        // F2 无匹配：#1 抛错 → v_yj 必为 '9'，绝不穿透取 sys_dummy 的 '5'。
        assert_eq!(
            expected[1][2],
            SpinValue::Str("9".into()),
            "seeds: {seeds:?}"
        );

        let mut eng = setup_copy_nested(&seeds);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_nested", &[]).unwrap();
        assert_eq!(rows_copy_nested(&eng), expected, "seeds: {seeds:?}");
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

fn setup_copy_if_trap(seed: &str) -> Engine {
    let sql = "CREATE PROCEDURE copy_if_trap IS v_out VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, k FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
        EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
        BEGIN \
        IF v_route = '1' THEN v_out := 'U'; \
        ELSE SELECT val INTO v_out FROM dim_tab WHERE dim_tab.k = r.k; END IF; \
        EXCEPTION WHEN OTHERS THEN v_out := '0'; END; \
        INSERT INTO target_rows(val, o) VALUES (r.a, v_out); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, k VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, o VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'K1'), (20, 'K2'), (30, 'K3')",
        "CREATE TABLE route_tab(kind VARCHAR, op VARCHAR)",
        seed,
        "CREATE TABLE dim_tab(k VARCHAR, val VARCHAR)",
        "INSERT INTO dim_tab VALUES ('K1', 'AA'), ('K2', 'BB')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_if_trap(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, o FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_route_invariant_if_inside_const_fallback_trap() {
    // 设计 §4.3 + §4.4 B1 + D7：条件为循环不变兜底查找变量
    // v_route = '1' 的 IF 路由——THEN 写 'U'、ELSE 走兜底查找组。
    // D7 双臂：THEN/ELSE 各自独立分析，运行期判别式选臂。判别值不等
    // '1'（含 NULL——SQL `=` 三值语义落 ELSE，与 PL IF 及 PR#62
    // route::pick 口径一致）→ ELSE 臂 Converted。
    for seed in [
        "INSERT INTO route_tab VALUES ('1', 'OTHER')",
        "INSERT INTO route_tab VALUES ('0', 'X')",
        "INSERT INTO route_tab VALUES (NULL, 'X')",
        "INSERT INTO route_tab VALUES ('0', 'X'), ('1', 'X')",
    ] {
        let mut baseline = setup_copy_if_trap(seed);
        baseline.call("copy_if_trap", &[]).unwrap();
        let expected = rows_copy_if_trap(&baseline);
        assert_eq!(expected[0][1], SpinValue::Str("AA".into()), "seed: {seed}");
        assert_eq!(expected[1][1], SpinValue::Str("BB".into()), "seed: {seed}");
        assert_eq!(expected[2][1], SpinValue::Str("0".into()), "seed: {seed}");

        let mut eng = setup_copy_if_trap(seed);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_if_trap", &[]).unwrap();
        assert_eq!(rows_copy_if_trap(&eng), expected, "seed: {seed}");
        let report = &eng.set_oriented_reports()[0];
        assert!(
            matches!(
                report.status,
                Status::Converted {
                    arm: Some(Arm::Else),
                    ..
                }
            ),
            "seed: {seed}, report: {report:?}"
        );
    }
    // THEN 可达：判别值 '1' → 选 THEN 臂批写 'U'，与解释执行逐行一致。
    let seed = "INSERT INTO route_tab VALUES ('1', 'X')";
    let mut baseline = setup_copy_if_trap(seed);
    baseline.call("copy_if_trap", &[]).unwrap();
    let expected = rows_copy_if_trap(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("U".into()));

    let mut eng = setup_copy_if_trap(seed);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_if_trap", &[]).unwrap();
    assert_eq!(rows_copy_if_trap(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted {
                arm: Some(Arm::Then),
                ..
            }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}
fn setup_copy_partial(seed: &str) -> Engine {
    let sql = "CREATE PROCEDURE copy_partial IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
        EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
        BEGIN \
        IF v_route = '1' THEN v_yj := 'U'; \
        ELSE \
        SELECT ct INTO v_ct FROM dim_tab WHERE dim_tab.k = r.k; \
        SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_a END INTO v_yj FROM sys_dummy; \
        END IF; \
        EXCEPTION WHEN OTHERS THEN v_yj := '0'; END; \
        INSERT INTO target_rows(val, yj) VALUES (r.a, v_yj); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, k VARCHAR, yj_a VARCHAR, yj_b VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, yj VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'K1', 'AA1', 'BB1'), (20, 'K2', 'AA2', 'BB2'), (30, 'K3', 'AA3', 'BB3')",
        "CREATE TABLE route_tab(kind VARCHAR, op VARCHAR)",
        seed,
        "CREATE TABLE dim_tab(k VARCHAR, ct VARCHAR)",
        "INSERT INTO dim_tab VALUES ('K1', 'A'), ('K2', 'B')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_partial(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, yj FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_allow_partial_fallback_when_intermediate_is_group_local_dead() {
    // 设计 §4.4 B1 部分兜底：ELSE 组含中间查找 v_ct 与 CASE 写汇 v_yj，
    // 处理器仅重置 v_yj（v_ct 只被同组 CASE 操作数读取、非 INSERT 绑定、
    // 非出口活跃 → 组内局部死）。批写以 group_hits 合取门控：任一成员落空
    // → v_yj 兜底 '0'，v_ct 残值不可观察，与解释执行逐行一致。
    let seed = "INSERT INTO route_tab VALUES ('1', 'OTHER')";
    let mut baseline = setup_copy_partial(seed);
    baseline.call("copy_partial", &[]).unwrap();
    let expected = rows_copy_partial(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("AA1".into()));
    assert_eq!(expected[1][1], SpinValue::Str("BB2".into()));
    assert_eq!(expected[2][1], SpinValue::Str("0".into()));

    let mut eng = setup_copy_partial(seed);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_partial", &[]).unwrap();
    assert_eq!(rows_copy_partial(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}
fn setup_copy_partial_live() -> Engine {
    let sql = "CREATE PROCEDURE copy_partial_live IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
        EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
        BEGIN \
        IF v_route = '1' THEN v_yj := 'U'; \
        ELSE \
        SELECT ct INTO v_ct FROM dim_tab WHERE dim_tab.k = r.k; \
        SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_a END INTO v_yj FROM sys_dummy; \
        END IF; \
        EXCEPTION WHEN OTHERS THEN v_yj := '0'; END; \
        INSERT INTO target_rows(val, yj, ct) VALUES (r.a, v_yj, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, k VARCHAR, yj_a VARCHAR, yj_b VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, yj VARCHAR, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'K1', 'AA1', 'BB1'), (20, 'K2', 'AA2', 'BB2'), (30, 'K3', 'AA3', 'BB3')",
        "CREATE TABLE route_tab(kind VARCHAR, op VARCHAR)",
        "INSERT INTO route_tab VALUES ('1', 'OTHER')",
        "CREATE TABLE dim_tab(k VARCHAR, ct VARCHAR)",
        "INSERT INTO dim_tab VALUES ('K1', 'A'), ('K2', 'B')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_partial_live(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, yj, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_reject_partial_fallback_when_intermediate_is_live_out() {
    // 部分兜底_soundness 边界：中间变量 v_ct 被 INSERT 读取（出口活跃），
    // 处理器仅重置 v_yj 时 v_ct 残值会进入输出 → 不可证等价，必须拒绝集合化，
    // 无损退回解释执行（K3 落空时 v_ct 保留上一轮残值，与 Off 逐行一致）。
    let mut baseline = setup_copy_partial_live();
    baseline.call("copy_partial_live", &[]).unwrap();
    let expected = rows_copy_partial_live(&baseline);

    let mut eng = setup_copy_partial_live();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_partial_live", &[]).unwrap();
    assert_eq!(rows_copy_partial_live(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        }
    );
}

fn setup_copy_route_bound() -> Engine {
    // N-1 回归 fixture：route 查找过滤引用帧变量 v_f（循环不变量但帧绑定），
    // 判别式渲染产物含 `$v_*` 占位符；bind-free 前提不成立，计划期必须拒绝。
    let sql = "CREATE PROCEDURE copy_route_bound IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); v_f VARCHAR2(10); BEGIN \
        v_f := 'X'; \
        FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = v_f; \
        EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
        BEGIN \
        IF v_route = '1' THEN v_yj := 'U'; \
        ELSE \
        SELECT ct INTO v_ct FROM dim_tab WHERE dim_tab.k = r.k; \
        SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_a END INTO v_yj FROM sys_dummy; \
        END IF; \
        EXCEPTION WHEN OTHERS THEN v_yj := '0'; END; \
        INSERT INTO target_rows(val, yj) VALUES (r.a, v_yj); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, k VARCHAR, yj_a VARCHAR, yj_b VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, yj VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'K1', 'AA1', 'BB1'), (20, 'K2', 'AA2', 'BB2'), (30, 'K3', 'AA3', 'BB3')",
        "CREATE TABLE route_tab(kind VARCHAR, op VARCHAR)",
        "INSERT INTO route_tab VALUES ('1', 'X')",
        "CREATE TABLE dim_tab(k VARCHAR, ct VARCHAR)",
        "INSERT INTO dim_tab VALUES ('K1', 'A'), ('K2', 'B')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

#[test]
fn should_reject_route_lookup_with_frame_bound_filter() {
    // N-1：route 判别式/守卫以空 BindMap 求值；过滤含帧变量时计划期
    // 必须 Skipped(UnsupportedControlFlow) 回退解释器——不得 KERNEL-0010 崩溃。
    let mut baseline = setup_copy_route_bound();
    baseline.call("copy_route_bound", &[]).unwrap();
    let expected = rows_copy_partial(&baseline);

    let mut eng = setup_copy_route_bound();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_route_bound", &[]).unwrap();
    assert_eq!(rows_copy_partial(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        },
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}

fn setup_copy_both_arms(route: &str) -> Engine {
    // 双臂均可集合化的路由 IF（purchase kind IF 同构，合并树移植）：
    // THEN/ELSE 各含「维表查找 + sys_dummy CASE 写汇」，写集一致
    // {v_ct, v_yj}，处理器仅重置 v_yj，v_ct 为组内局部死。
    let sql = "CREATE PROCEDURE copy_both_arms IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
        EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
        BEGIN \
        IF v_route = '1' THEN \
        SELECT ct_t INTO v_ct FROM dim_tab WHERE dim_tab.k = 'T1'; \
        SELECT CASE v_ct WHEN 'A' THEN r.yj_a ELSE r.yj_b END INTO v_yj FROM sys_dummy; \
        ELSE \
        SELECT ct INTO v_ct FROM dim_tab WHERE dim_tab.k = r.k; \
        SELECT CASE v_ct WHEN 'A' THEN r.yj_a WHEN 'B' THEN r.yj_b ELSE r.yj_a END INTO v_yj FROM sys_dummy; \
        END IF; \
        EXCEPTION WHEN OTHERS THEN v_yj := '0'; END; \
        INSERT INTO target_rows(val, yj) VALUES (r.a, v_yj); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, k VARCHAR, yj_a VARCHAR, yj_b VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, yj VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'K1', 'AA1', 'BB1'), (20, 'K2', 'AA2', 'BB2'), (30, 'K3', 'AA3', 'BB3')",
        "CREATE TABLE route_tab(kind VARCHAR, op VARCHAR)",
        &format!("INSERT INTO route_tab VALUES ('{route}', 'X')"),
        "CREATE TABLE dim_tab(k VARCHAR, ct VARCHAR, ct_t VARCHAR)",
        "INSERT INTO dim_tab VALUES ('K1', 'A', 'A'), ('K2', 'B', 'B'), ('T1', 'X', 'A')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_both_arms(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, yj FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_convert_route_if_when_both_arms_are_convertible() {
    // 分臂备选计划：THEN/ELSE 均可集合化时，入口按路由条件选择对应计划
    // ——THEN 臂激活时不再 Skipped，而是按 THEN 语义 Converted。
    // 分臂判别式：THEN/ELSE 均可集合化时判别式选臂——THEN 臂激活
    // 不再整体 Skipped，按 THEN 语义 Converted（合并树移植，D7 准入超集）。
    for route in ["1", "0"] {
        let mut baseline = setup_copy_both_arms(route);
        baseline.call("copy_both_arms", &[]).unwrap();
        let expected = rows_copy_both_arms(&baseline);

        let mut eng = setup_copy_both_arms(route);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("copy_both_arms", &[]).unwrap();
        assert_eq!(rows_copy_both_arms(&eng), expected, "route: {route}");
        assert!(
            matches!(
                eng.set_oriented_reports()[0].status,
                Status::Converted { .. }
            ),
            "route: {route}, report: {:?}",
            eng.set_oriented_reports()[0]
        );
    }
}

#[test]
fn should_reject_at_runtime_for_lp_takeover_when_then_arm_unconvertible() {
    // SO→LP 派发缺口守护（合并树 should_parallelize_region_when_then_arm_
    // unconvertible 的 SO 侧；本分支无 loop-parallel，LP Parallelized 断言
    // 随 PR#62 合入补齐）。THEN 臂 `SELECT id, payload INTO v_fee, v_extra`
    // 为双变量 INTO——臂内准入拒绝 → analyze_then_arm 失败 → 静默降级
    // 单臂（discriminator=None，route_guards 守 THEN 不可达）；kind='1'
    // 数据走 THEN → 运行态守卫拒绝 → Skipped{UnsupportedControlFlow}，
    // 该拒绝点即 PR#62 try_loop_parallel_region 的接管触发点。
    // kind='0' 走 ELSE → 单臂 Converted；两种数据行集均与 Off 全等。
    const ROUTE_PROC_WIDE: &str = "CREATE PROCEDURE p_route_wide IS \
    v_kind VARCHAR; v_fee VARCHAR; v_extra VARCHAR; \
    BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN \
            SELECT kind_id INTO v_kind FROM switches WHERE operation_kind = 'SW'; \
        EXCEPTION WHEN OTHERS THEN v_kind := '0'; END; \
        BEGIN \
            IF v_kind = '1' THEN \
                SELECT id, payload INTO v_fee, v_extra FROM source_rows WHERE source_rows.id = r.id; \
            ELSE \
                SELECT fee INTO v_fee FROM fee_tbl WHERE fee_tbl.id = r.id; \
            END IF; \
        EXCEPTION WHEN OTHERS THEN v_fee := 'D'; END; \
        INSERT INTO target_rows(id, fee) VALUES (r.id, v_fee); \
    END LOOP; END;";
    for kind in ["1", "0"] {
        let setup = |eng: &mut Engine| {
            for sql in [
                "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
                "CREATE TABLE target_rows(id BIGINT, fee VARCHAR)",
                "CREATE TABLE switches(operation_kind VARCHAR, kind_id VARCHAR)",
                "CREATE TABLE fee_tbl(id BIGINT, fee VARCHAR)",
                "CREATE TABLE sys_dummy AS SELECT 1",
                "INSERT INTO source_rows VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')",
                "INSERT INTO fee_tbl VALUES (1,'f1'),(2,'f2'),(3,'f3'),(4,'f4'),(5,'f5')",
                &format!("INSERT INTO switches VALUES ('SW', '{kind}')"),
            ] {
                eng.ex.execute(sql, &BindMap::new()).unwrap();
            }
            eng.register(ROUTE_PROC_WIDE).unwrap();
        };
        let mut baseline = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        setup(&mut baseline);
        baseline.call("p_route_wide", &[]).unwrap();
        let expected = baseline
            .ex
            .query(
                "SELECT id, fee FROM target_rows ORDER BY id",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20);

        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        setup(&mut eng);
        eng.set_oriented = SetOrientedMode::BatchAtomic;
        eng.call("p_route_wide", &[]).unwrap();
        let actual = eng
            .ex
            .query(
                "SELECT id, fee FROM target_rows ORDER BY id",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20);
        assert_eq!(actual, expected, "kind: {kind}");
        let report = &eng.set_oriented_reports()[0];
        if kind == "1" {
            // THEN 活跃：降级单臂计划运行态守卫拒绝 → SO 缺口（LP 接管点）。
            assert_eq!(
                report.status,
                Status::Skipped {
                    reason: SkipReason::UnsupportedControlFlow
                },
                "kind: {kind}, report: {report:?}"
            );
        } else {
            // ELSE 活跃：降级单臂 ELSE 计划 Converted{arm:None}。
            assert!(
                matches!(report.status, Status::Converted { arm: None, .. }),
                "kind: {kind}, report: {report:?}"
            );
        }
    }
}
