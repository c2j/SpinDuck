//! 集合化 lateral 语义守护。

use super::*;

fn setup_copy_partial_then(seed: &str) -> Engine {
    let sql = "CREATE PROCEDURE copy_partial_then IS v_yj VARCHAR2(10); v_ct VARCHAR2(10); v_route VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, k, yj_a, yj_b FROM source_rows LOOP \
        BEGIN SELECT kind INTO v_route FROM route_tab WHERE op = 'X'; \
        EXCEPTION WHEN OTHERS THEN v_route := '0'; END; \
        BEGIN \
        IF v_route = '1' THEN \
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

fn rows_copy_partial_then(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, yj FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_exempt_unreachable_then_reads_from_group_local_dead_proof() {
    // 设计 §4.3 + §4.4 B1：THEN 由入口路由守卫证明不可达（v_route='1' 不成立），
    // 其内对中间变量 v_ct 的读取（语料 v_yj 块的 THEN CASE 写汇形态）不参与
    // 集合化计划，须从组内局部死证明中豁免；v_ct 仍仅被 ELSE 同组 CASE 操作数
    // 读取、非 INSERT 绑定、非出口活跃 → 可证等价。THEN 可达时守卫失败 →
    // 无损退回解释执行，与 Off 逐行一致。
    // THEN 不可达种子：op='OTHER' 不匹配 WHERE op='X' → v_route 兜底 '0'。
    let seed = "INSERT INTO route_tab VALUES ('1', 'OTHER')";
    let mut baseline = setup_copy_partial_then(seed);
    baseline.call("copy_partial_then", &[]).unwrap();
    let expected = rows_copy_partial_then(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("AA1".into()));
    assert_eq!(expected[1][1], SpinValue::Str("BB2".into()));
    assert_eq!(expected[2][1], SpinValue::Str("0".into()));

    let mut eng = setup_copy_partial_then(seed);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_partial_then", &[]).unwrap();
    assert_eq!(rows_copy_partial_then(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // D7 双臂：THEN 臂对仅 ELSE 写入的 v_ct 按「本臂恒 NULL」折叠
    // （CASE NULL WHEN → 恒取 ELSE 支），两臂均可证 → Converted。
    // 判别值 '1' → 选 THEN 臂批写，与解释执行逐行一致。
    let seed = "INSERT INTO route_tab VALUES ('1', 'X')";
    let mut baseline = setup_copy_partial_then(seed);
    baseline.call("copy_partial_then", &[]).unwrap();
    let expected = rows_copy_partial_then(&baseline);
    let mut eng = setup_copy_partial_then(seed);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_partial_then", &[]).unwrap();
    assert_eq!(rows_copy_partial_then(&eng), expected);
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
fn setup_copy_inlist(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_inlist IS v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, market_code, seat_code, stock_kind FROM source_rows LOOP \
        BEGIN SELECT commision_type INTO v_ct FROM dim \
        WHERE dim.market_code = r.market_code AND dim.seat_code IN (r.seat_code, '000000') \
        AND ROWNUM = 1 ORDER BY seat_code DESC, stock_kind DESC; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, market_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'M1', 'S1', 'K1'), (20, 'M1', 'S2', 'K2'), (30, 'M1', 'S9', 'K9'), (40, 'M2', 'S1', 'K1')",
        "CREATE TABLE dim(market_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR, commision_type VARCHAR)",
        "INSERT INTO dim VALUES ('M1', 'S1', 'K1', 'A'), ('M1', '000000', '0000', 'C'), ('M1', 'S2', 'K2', 'D')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_inlist(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_top1_with_cursor_field_in_list_via_lateral() {
    // 设计 §4.4 B1 有序 Top-1 + 游标字段 IN-list（语料 v_yj ELSE 形态）：
    // IN-list 项含游标字段与字面量，逐行相关 → 独立窗口 CTE 无法引用 sd_cur，
    // 改以 LEFT JOIN LATERAL 逐行 Top-1 渲染；入口并列守卫亦按行 LATERAL
    // 聚合（RANK + FIRST_VALUE + IS DISTINCT FROM，禁 COUNT(DISTINCT)）。
    // 落空行落兜底 '0'；异值并列 → AmbiguousTop1Tie 退回解释，与 Off 逐行一致。
    // 无并列：a10→A（S1 优先），a20→D，a30→C（仅 000000 命中），a40→'0'（M2 落空）。
    let mut baseline = setup_copy_inlist(&[]);
    baseline.call("copy_inlist", &[]).unwrap();
    let expected = rows_copy_inlist(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("A".into()));
    assert_eq!(expected[1][1], SpinValue::Str("D".into()));
    assert_eq!(expected[2][1], SpinValue::Str("C".into()));
    assert_eq!(expected[3][1], SpinValue::Str("0".into()));
    let mut eng = setup_copy_inlist(&[]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_inlist", &[]).unwrap();
    assert_eq!(rows_copy_inlist(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // 异值并列：S1/K1 增 'B' → 完整排序键并列且值异 → 入口守卫拒绝。
    let seeds = ["INSERT INTO dim VALUES ('M1', 'S1', 'K1', 'B')"];
    let mut baseline = setup_copy_inlist(&seeds);
    baseline.call("copy_inlist", &[]).unwrap();
    let expected = rows_copy_inlist(&baseline);
    let mut eng = setup_copy_inlist(&seeds);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_inlist", &[]).unwrap();
    assert_eq!(rows_copy_inlist(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
}
fn setup_copy_expr(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_expr IS v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, market_code, seat_code, stock_kind FROM source_rows LOOP \
        BEGIN SELECT commision_type INTO v_ct FROM dim \
        WHERE dim.market_code = r.market_code AND dim.seat_code IN (r.seat_code, '000000') \
        AND dim.stock_kind IN (r.stock_kind, substr(r.stock_kind, 1, 2) || '00', '0000') \
        AND ROWNUM = 1 ORDER BY seat_code DESC, stock_kind DESC; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, market_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'M1', 'S1', '0501'), (20, 'M1', 'S2', 'K2'), (30, 'M1', 'S9', '0000'), (40, 'M2', 'S1', '0501')",
        "CREATE TABLE dim(market_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR, commision_type VARCHAR)",
        "INSERT INTO dim VALUES ('M1', 'S1', '0500', 'A'), ('M1', '000000', '0000', 'C'), ('M1', 'S2', 'K2', 'D')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_expr(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_top1_with_substr_expression_in_list_item() {
    // 设计 §4.4 B1：IN-list 项含 substr(游标字段,1,2)||'00' 表达式（语料
    // par_fund_seat_para 的 stock_kind IN 形态）。表达式仅由 substr/|| 与
    // 字符串字面量构成、基字段与维表列同为 varchar → 静态类型保持、全定义
    // 不抛错；逐行 LATERAL 渲染。a10 的命中行 stock_kind='0500' 仅经
    // substr('0501',1,2)||'00' 表达式项可达（裸字段 '0501' 不匹配）。
    // a10→A（仅 substr 表达式项命中 0500），a20→D，a30→C，a40→'0'（M2 落空）。
    let mut baseline = setup_copy_expr(&[]);
    baseline.call("copy_expr", &[]).unwrap();
    let expected = rows_copy_expr(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("A".into()));
    assert_eq!(expected[1][1], SpinValue::Str("D".into()));
    assert_eq!(expected[2][1], SpinValue::Str("C".into()));
    assert_eq!(expected[3][1], SpinValue::Str("0".into()));
    let mut eng = setup_copy_expr(&[]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_expr", &[]).unwrap();
    assert_eq!(rows_copy_expr(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // 异值并列：S1/0500 增 'B' → 完整排序键并列且值异 → 入口守卫拒绝。
    let seeds = ["INSERT INTO dim VALUES ('M1', 'S1', '0500', 'B')"];
    let mut baseline = setup_copy_expr(&seeds);
    baseline.call("copy_expr", &[]).unwrap();
    let expected = rows_copy_expr(&baseline);
    let mut eng = setup_copy_expr(&seeds);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_expr", &[]).unwrap();
    assert_eq!(rows_copy_expr(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
}
fn setup_copy_between(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_between(p_i_date IN VARCHAR2) IS v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, market_code, seat_code, stock_kind FROM source_rows LOOP \
        BEGIN SELECT commision_type INTO v_ct FROM dim \
        WHERE dim.market_code = r.market_code AND dim.seat_code IN (r.seat_code, '000000') \
        AND p_i_date BETWEEN dim.inure_begin_date AND dim.inure_end_date \
        AND ROWNUM = 1 ORDER BY seat_code DESC, stock_kind DESC; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct) VALUES (r.a, v_ct); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, market_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'M1', 'S1', 'K1'), (20, 'M1', 'S2', 'K2'), (30, 'M1', 'S9', 'K9'), (40, 'M2', 'S1', 'K1')",
        "CREATE TABLE dim(market_code VARCHAR, seat_code VARCHAR, stock_kind VARCHAR, commision_type VARCHAR, inure_begin_date VARCHAR, inure_end_date VARCHAR)",
        "INSERT INTO dim VALUES ('M1', 'S1', 'K1', 'A', '2026-01-01', '2026-12-31'), ('M1', '000000', '0000', 'C', '2026-01-01', '2026-12-31'), ('M1', 'S2', 'K2', 'D', '2026-01-01', '2026-12-31'), ('M1', 'S1', 'K1', 'E', '2025-01-01', '2025-12-31')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_between(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_top1_with_between_param_bind_via_lateral() {
    // 设计 §4.4 B1 + §4.5-2：LATERAL Top-1 的静态过滤含过程形参 BETWEEN
    // 日期区间（语料 p_i_date BETWEEN inure_begin_date AND inure_end_date）。
    // 形参绑定须随批写携带，且入口并列守卫以同一形参值求值（守卫从运行帧
    // 解析绑定，非空绑定）。过期维表行 E 与 A 同排序键：若守卫漏带 BETWEEN
    // 绑定则误判并列退回；正确携带时 E 被区间排除 → Converted。
    let arg = [SpinValue::Str("2026-01-15".into())];
    // p_i_date=2026-01-15：E 过期被排除，a10→A（无并列），a20→D，a30→C，a40→'0'。
    let mut baseline = setup_copy_between(&[]);
    baseline.call("copy_between", &arg).unwrap();
    let expected = rows_copy_between(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("A".into()));
    assert_eq!(expected[1][1], SpinValue::Str("D".into()));
    assert_eq!(expected[2][1], SpinValue::Str("C".into()));
    assert_eq!(expected[3][1], SpinValue::Str("0".into()));
    let mut eng = setup_copy_between(&[]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_between", &arg).unwrap();
    assert_eq!(rows_copy_between(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // 区间内异值并列：S1/K1 增有效期内的 'B' → 入口守卫拒绝。
    let seeds = ["INSERT INTO dim VALUES ('M1', 'S1', 'K1', 'B', '2026-01-01', '2026-12-31')"];
    let mut baseline = setup_copy_between(&seeds);
    baseline.call("copy_between", &arg).unwrap();
    let expected = rows_copy_between(&baseline);
    let mut eng = setup_copy_between(&seeds);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_between", &arg).unwrap();
    assert_eq!(rows_copy_between(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
}
fn setup_copy_var(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_var IS v_fund VARCHAR2(10); v_ct VARCHAR2(10); BEGIN \
        v_fund := 'F1'; \
        FOR r IN SELECT a, market_code FROM source_rows LOOP \
        BEGIN SELECT commision_type INTO v_ct FROM dim \
        WHERE dim.fund_code = v_fund AND dim.market_code = r.market_code \
        AND ROWNUM = 1 ORDER BY seat_code DESC; \
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
        "CREATE TABLE dim(fund_code VARCHAR, market_code VARCHAR, seat_code VARCHAR, commision_type VARCHAR)",
        "INSERT INTO dim VALUES ('F1', 'M1', 'S1', 'A'), ('F1', 'M2', 'S1', 'B'), ('F2', 'M1', 'S1', 'X')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_var(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_top1_with_invariant_var_equality_filter() {
    // 设计 §4.4 B1：相关 Top-1 的静态过滤含循环不变局部变量等值
    // （语料 s.fund_code = v_fund_code，v_fund_code 在前缀赋值、循环内不变）。
    // 该等值须作为静态谓词随批写携带绑定，入口并列守卫以同一帧值求值。
    // F2/M1/X 行若漏掉 fund 过滤会与 F1/M1/A 同排序键误判并列退回；
    // 正确携带时 M1 仅 A → Converted。
    let mut baseline = setup_copy_var(&[]);
    baseline.call("copy_var", &[]).unwrap();
    let expected = rows_copy_var(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("A".into()));
    assert_eq!(expected[1][1], SpinValue::Str("B".into()));
    assert_eq!(expected[2][1], SpinValue::Str("0".into()));
    let mut eng = setup_copy_var(&[]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_var", &[]).unwrap();
    assert_eq!(rows_copy_var(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // 异值并列：F1/M1/S1 增 'T' → 入口守卫拒绝（守卫须含 fund 过滤）。
    let seeds = ["INSERT INTO dim VALUES ('F1', 'M1', 'S1', 'T')"];
    let mut baseline = setup_copy_var(&seeds);
    baseline.call("copy_var", &[]).unwrap();
    let expected = rows_copy_var(&baseline);
    let mut eng = setup_copy_var(&seeds);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_var", &[]).unwrap();
    assert_eq!(rows_copy_var(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
}
