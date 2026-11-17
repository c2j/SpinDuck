//! 集合化 inline 语义守护。

use super::*;

#[test]
fn should_render_inlined_var_dependency_chain() {
    // 设计 §4.3 A7 / §4.6：内联变量可引用另一内联变量（语料
    // v_tradedate_flag := fnc_com_if_deal_date(market, v_repurchase_date)）。
    // 批写投影须递归解析内联 Var 引用为其表达式（非常量/查找），
    // 日期链 to_char(to_date(v_base)+1) 逐行求值与 Off 一致。
    let sql = "CREATE PROCEDURE copy_chain IS v_base VARCHAR2(20); v_final VARCHAR2(20); BEGIN \
        FOR r IN SELECT a, d FROM source_rows LOOP \
        BEGIN \
        v_base := to_char(to_date(r.d, 'YYYYMMDD') + 1, 'YYYYMMDD'); \
        v_final := to_char(to_date(v_base, 'YYYYMMDD') + 1, 'YYYYMMDD'); \
        INSERT INTO target_rows(val, d) VALUES (r.a, v_final); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a BIGINT, d VARCHAR)",
            "CREATE TABLE target_rows(val BIGINT, d VARCHAR)",
            "INSERT INTO source_rows VALUES (10, '20260101'), (20, '20260131')",
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
                "SELECT val, d FROM target_rows ORDER BY val",
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(20)
    };
    let mut baseline = setup();
    baseline.call("copy_chain", &[]).unwrap();
    let expected = rows_of(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("20260103".into()));
    assert_eq!(expected[1][1], SpinValue::Str("20260202".into()));

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_chain", &[]).unwrap();
    assert_eq!(rows_of(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}

fn setup_copy_cond() -> Engine {
    let sql = "CREATE PROCEDURE copy_cond IS v_x VARCHAR2(20); BEGIN \
        FOR r IN SELECT a, d FROM source_rows LOOP \
        BEGIN \
        v_x := to_char(to_date(r.d, 'YYYYMMDD') + 1, 'YYYYMMDD'); \
        IF r.a > 15 THEN \
        v_x := to_char(to_date(r.d, 'YYYYMMDD') + 2, 'YYYYMMDD'); \
        END IF; \
        INSERT INTO target_rows(val, d) VALUES (r.a, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, d VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, d VARCHAR)",
        "INSERT INTO source_rows VALUES (10, '20260101'), (20, '20260131')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_cond(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, d FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_conditional_reassignment_as_per_row_case() {
    // 设计 §4.3（语料 v_repurchase_date 形态最小化）：内联变量赋值后随
    // 一个逐行谓词 IF 条件重赋值（THEN 单条 SqlFn 覆盖，末端跳回汇合点）。
    // 须关系化为逐行 CASE WHEN 谓词 THEN 覆盖值 ELSE 基值 END，而非
    // 要求 THEN 不可达；输出与 Off 逐行一致。
    let mut baseline = setup_copy_cond();
    baseline.call("copy_cond", &[]).unwrap();
    let expected = rows_copy_cond(&baseline);
    // a10 谓词假 → +1 = 20260102；a20 谓词真 → +2 = 20260202。
    assert_eq!(expected[0][1], SpinValue::Str("20260102".into()));
    assert_eq!(expected[1][1], SpinValue::Str("20260202".into()));

    let mut eng = setup_copy_cond();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_cond", &[]).unwrap();
    assert_eq!(rows_copy_cond(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}

fn setup_copy_foldif() -> Engine {
    let func = "CREATE FUNCTION f_zero RETURN NUMBER IS BEGIN RETURN 0; END;";
    let getday = "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n NUMBER) RETURN VARCHAR2 IS \
        BEGIN RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;";
    let sql = "CREATE PROCEDURE copy_foldif IS v_x VARCHAR2(20); v_flag NUMBER; BEGIN \
        FOR r IN SELECT a, d FROM source_rows LOOP \
        BEGIN \
        v_x := to_char(to_date(r.d, 'YYYYMMDD') + 1, 'YYYYMMDD'); \
        v_flag := f_zero(); \
        IF v_flag = 1 THEN \
        v_x := f_getday(r.d, 2); \
        END IF; \
        INSERT INTO target_rows(val, d) VALUES (r.a, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, d VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, d VARCHAR)",
        "INSERT INTO source_rows VALUES (10, '20260101'), (20, '20260131')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(func).unwrap();
    eng.register(getday).unwrap();
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_foldif(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, d FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_prune_static_false_if_with_then_joining_body() {
    // 折叠常量 IF（§4.3 A7，语料 v_tradedate_flag 形态最小化）：谓词变量
    // 折叠为常量 0（f_zero RETURN 0），IF v_flag = 1 静态假 → THEN 不可达。
    // THEN 为注册纯函数调用（fnc_com_getday 形态，非直接 SqlFn）且末端跳回
    // 体内汇合点（pc+3）而非循环头：cond_reassign 不收 FuncCall THEN，须先于
    // 不变量路由检查做折叠常量静态剪枝，基值投影逐行求值与 Off 一致。
    let mut baseline = setup_copy_foldif();
    baseline.call("copy_foldif", &[]).unwrap();
    let expected = rows_copy_foldif(&baseline);
    // 谓词恒假 → THEN 剪枝，两行均取基值 +1：20260102 / 20260201。
    assert_eq!(expected[0][1], SpinValue::Str("20260102".into()));
    assert_eq!(expected[1][1], SpinValue::Str("20260201".into()));

    let mut eng = setup_copy_foldif();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_foldif", &[]).unwrap();
    assert_eq!(rows_copy_foldif(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );
}

fn setup_copy_lkref(seeds: &[&str]) -> Engine {
    let sql = "CREATE PROCEDURE copy_lkref IS v_fund VARCHAR2(10); v_ct VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, market_code, fund_key, seat FROM source_rows LOOP \
        BEGIN SELECT f.fund_code INTO v_fund FROM funddim f WHERE f.fkey = r.fund_key; \
        EXCEPTION WHEN OTHERS THEN v_fund := '0'; END; \
        BEGIN SELECT a.commision_type INTO v_ct FROM ( \
        SELECT s.commision_type FROM dim s WHERE s.fund_code = v_fund \
        AND s.market_code = r.market_code AND s.seat_code IN (r.seat, '000000') \
        ORDER BY s.seat_code DESC) a WHERE rownum = 1; \
        EXCEPTION WHEN OTHERS THEN v_ct := '0'; END; \
        INSERT INTO target_rows(val, ct, fund) VALUES (r.a, v_ct, v_fund); \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    let base = [
        "CREATE TABLE source_rows(a BIGINT, market_code VARCHAR, fund_key VARCHAR, seat VARCHAR)",
        "CREATE TABLE target_rows(val BIGINT, ct VARCHAR, fund VARCHAR)",
        "INSERT INTO source_rows VALUES (10, 'M1', 'K1', 'S1'), (20, 'M2', 'K2', 'S2'), (30, 'M3', 'K3', 'S3')",
        "CREATE TABLE funddim(fkey VARCHAR, fund_code VARCHAR)",
        "INSERT INTO funddim VALUES ('K1', 'F1'), ('K2', 'F2')",
        "CREATE TABLE dim(fund_code VARCHAR, market_code VARCHAR, seat_code VARCHAR, commision_type VARCHAR)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ];
    for ddl in base.iter().copied().chain(seeds.iter().copied()) {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng
}

fn rows_copy_lkref(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val, ct, fund FROM target_rows ORDER BY val",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(20)
}

#[test]
fn should_relationalize_top1_filter_referencing_per_row_lookup_var() {
    // 设计 §4.4 B1（语料 v_commision_fee_type 的 s.fund_code = v_fund_code）：
    // 派生 Top-1 的静态过滤引用另一逐行相关查找的结果变量 v_fund（循环可变，
    // 非常量帧值）。该等值须以 v_fund 的批写投影 CASE 逐行代入（含入口并列
    // 守卫自包含注入 v_fund 派生表/连接），而非作单值帧绑定携带。
    // F9 干扰行若漏掉 fund 过滤会与 F1 同排序键误判并列退回；正确代入时
    // 行 10 仅 A → Converted。真并列（F1/M1/S1 增 T）→ 入口守卫拒绝。
    // happy：F1/M1/S1→A（S2/B 不在 seat IN-list；F9/M1/S1/Z 被 fund 过滤排除）；
    // F2/M2/S2→C；K3 无 funddim 行→v_fund='0'，M3 无匹配→v_ct='0'。
    let happy = [
        "INSERT INTO dim VALUES ('F1', 'M1', 'S1', 'A'), ('F1', 'M1', 'S2', 'B'), ('F2', 'M2', 'S2', 'C'), ('F9', 'M1', 'S1', 'Z')",
    ];
    let mut baseline = setup_copy_lkref(&happy);
    baseline.call("copy_lkref", &[]).unwrap();
    let expected = rows_copy_lkref(&baseline);
    assert_eq!(expected[0][1], SpinValue::Str("A".into()));
    assert_eq!(expected[0][2], SpinValue::Str("F1".into()));
    assert_eq!(expected[1][1], SpinValue::Str("C".into()));
    assert_eq!(expected[2][1], SpinValue::Str("0".into()));
    assert_eq!(expected[2][2], SpinValue::Str("0".into()));
    let mut eng = setup_copy_lkref(&happy);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_lkref", &[]).unwrap();
    assert_eq!(rows_copy_lkref(&eng), expected);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "report: {:?}",
        eng.set_oriented_reports()[0]
    );

    // 真并列：F1/M1/S1 增 'T'（同排序键 seat S1）→ 入口守卫拒绝，行输出同 Off。
    let tie = [
        "INSERT INTO dim VALUES ('F1', 'M1', 'S1', 'A'), ('F1', 'M1', 'S1', 'T'), ('F2', 'M2', 'S2', 'C')",
    ];
    let mut baseline = setup_copy_lkref(&tie);
    baseline.call("copy_lkref", &[]).unwrap();
    let expected = rows_copy_lkref(&baseline);
    let mut eng = setup_copy_lkref(&tie);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_lkref", &[]).unwrap();
    assert_eq!(rows_copy_lkref(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::AmbiguousTop1Tie
        }
    );
}
