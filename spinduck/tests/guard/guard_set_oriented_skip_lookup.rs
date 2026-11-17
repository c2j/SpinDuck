//! 跳行 trap 维表查找（§5 步骤 4 的 sd_sys_purchase 形态最小化）：
//! `SELECT INTO` 包在 `WHEN OTHERS THEN NULL`（处理器 = Nop + Jump 回循环头）
//! 且与 INSERT 同块——查找落空抛 NO_DATA_FOUND → 跳行（整行过滤），
//! 命中则取值写列。区别于常量兜底组（miss → 落默认值保留行）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const PROC: &str = "CREATE PROCEDURE copy_rows IS v_d INTEGER; BEGIN \
    FOR r IN SELECT id, payload FROM source_rows LOOP \
    BEGIN \
    SELECT t.n INTO v_d FROM dim_days t WHERE t.sid = r.payload; \
    INSERT INTO target_rows(id, payload) VALUES (r.id, v_d); \
    EXCEPTION WHEN OTHERS THEN NULL; END; \
    END LOOP; END;";

fn engine() -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
        "CREATE TABLE target_rows(id BIGINT, payload INTEGER)",
        "CREATE TABLE dim_days(sid VARCHAR, n INTEGER)",
        "INSERT INTO dim_days VALUES ('a', 5), ('b', 7)",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn seed(eng: &Engine, rows: &[(i64, &str)]) {
    for (id, p) in rows {
        eng.ex
            .execute(
                &format!("INSERT INTO source_rows VALUES ({id}, '{p}')"),
                &BindMap::new(),
            )
            .unwrap();
    }
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT id, payload FROM target_rows ORDER BY id NULLS LAST, payload",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

#[test]
fn should_filter_rows_whose_skip_trap_lookup_misses() {
    // Off 基线：'a'→5、'c' 落空跳行、'b'→7、'a'→5。
    let mut baseline = engine();
    baseline.register(PROC).unwrap();
    seed(&baseline, &[(1, "a"), (2, "c"), (3, "b"), (4, "a")]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(
        want,
        vec![
            vec![SpinValue::Int(1), SpinValue::Int(5)],
            vec![SpinValue::Int(3), SpinValue::Int(7)],
            vec![SpinValue::Int(4), SpinValue::Int(5)],
        ]
    );

    let mut eng = engine();
    eng.register(PROC).unwrap();
    seed(&eng, &[(1, "a"), (2, "c"), (3, "b"), (4, "a")]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_preserve_hit_null_and_skip_duplicate_key_in_skip_trap_lookup() {
    // 命中 NULL 保留（不得 COALESCE）；同键 2 行 → TOO_MANY_ROWS 跳行；
    // 落空 → NO_DATA_FOUND 跳行。区别于常量兜底（后者保留行落默认值）。
    let mut baseline = engine();
    baseline.register(PROC).unwrap();
    baseline
        .ex
        .execute("DELETE FROM dim_days", &BindMap::new())
        .unwrap();
    baseline
        .ex
        .execute(
            "INSERT INTO dim_days VALUES ('a', 5), ('b', NULL), ('c', 7), ('c', 9)",
            &BindMap::new(),
        )
        .unwrap();
    seed(&baseline, &[(1, "a"), (2, "b"), (3, "c"), (4, "z")]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(
        want,
        vec![
            vec![SpinValue::Int(1), SpinValue::Int(5)],
            vec![SpinValue::Int(2), SpinValue::Null],
        ]
    );

    let mut eng = engine();
    eng.register(PROC).unwrap();
    eng.ex
        .execute("DELETE FROM dim_days", &BindMap::new())
        .unwrap();
    eng.ex
        .execute(
            "INSERT INTO dim_days VALUES ('a', 5), ('b', NULL), ('c', 7), ('c', 9)",
            &BindMap::new(),
        )
        .unwrap();
    seed(&eng, &[(1, "a"), (2, "b"), (3, "c"), (4, "z")]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_reject_skip_trap_lookup_filter_in_strict_mode() {
    // Strict 的 reset_session 回滚语义与行过滤不等价：跳行 trap 查找
    // 仅在 BatchAtomic 授权，Strict 必须保守退回（数据仍与 Off 一致）。
    let mut baseline = engine();
    baseline.register(PROC).unwrap();
    seed(&baseline, &[(1, "a"), (2, "c"), (3, "b")]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);

    let mut eng = engine();
    eng.register(PROC).unwrap();
    seed(&eng, &[(1, "a"), (2, "c"), (3, "b")]);
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(eng.set_oriented_reports()[0].status, Status::Skipped { .. }),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_filter_rows_when_skip_trap_handler_logs_sqlerrm_via_noop_stub() {
    // 语料 prc_trd_deal_purchase 的跳行 trap 形态：处理器不是 NULL，而是
    // NoOp 日志桩 inst_log('取' || r.payload || '出错' || SQLERRM) + GOTO
    // 回循环头。SQLERRM 编译为隐式错误帧变量 __sqlerrm（VARCHAR），NoOp
    // 证明须承认其 Str/Null 才能消除日志桩；落空仍整行过滤，数据与 Off 一致。
    let stub = "CREATE PROCEDURE inst_log(p_msg VARCHAR2) AS BEGIN NULL; END;";
    let proc = "CREATE PROCEDURE copy_rows IS \
        CURSOR c IS SELECT id, payload FROM source_rows; \
        r c%ROWTYPE; v_n INTEGER; BEGIN \
        OPEN c; LOOP \
        <<error_row>> \
        FETCH c INTO r; EXIT WHEN c%NOTFOUND; \
        BEGIN \
        SELECT t.n INTO v_n FROM dim_days t WHERE t.sid = r.payload; \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_n); \
        EXCEPTION WHEN OTHERS THEN \
        inst_log('取' || r.payload || '出错' || SQLERRM); \
        GOTO error_row; \
        END; \
        END LOOP; CLOSE c; END;";
    let setup = || {
        let mut eng = engine();
        eng.register(stub).unwrap();
        eng.register(proc).unwrap();
        eng
    };
    let mut baseline = setup();
    seed(&baseline, &[(1, "a"), (2, "c"), (3, "b")]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(
        want,
        vec![
            vec![SpinValue::Int(1), SpinValue::Int(5)],
            vec![SpinValue::Int(3), SpinValue::Int(7)],
        ]
    );

    let mut eng = setup();
    seed(&eng, &[(1, "a"), (2, "c"), (3, "b")]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_filter_by_between_date_range_with_param_bind() {
    // §5 步骤 4 的 sd_sys_purchase 形态：单键相关 + 过程形参 p_d 落在维表
    // 日期区间（p_d BETWEEN t.beg AND t.endd）。形参绑定须随批写 SQL 携带；
    // 区间外 → 0 行 NO_DATA_FOUND 跳行。
    let proc = "CREATE PROCEDURE copy_rows(p_d VARCHAR2) IS v_n INTEGER; BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN \
        SELECT t.n INTO v_n FROM dim_win t WHERE t.sid = r.payload AND p_d BETWEEN t.beg AND t.endd; \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_n); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for sql in [
            "CREATE TABLE source_rows(id BIGINT, payload VARCHAR)",
            "CREATE TABLE target_rows(id BIGINT, payload INTEGER)",
            "CREATE TABLE dim_win(sid VARCHAR, beg VARCHAR, endd VARCHAR, n INTEGER)",
            "INSERT INTO dim_win VALUES ('a', '20250101', '20251231', 5), ('b', '20240101', '20241231', 7)",
            "INSERT INTO source_rows VALUES (1, 'a'), (2, 'b')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng.register(proc).unwrap();
        eng
    };
    let args = [SpinValue::Str("20250715".into())];
    let mut baseline = setup();
    baseline.call("copy_rows", &args).unwrap();
    let want = rows(&baseline);
    assert_eq!(want, vec![vec![SpinValue::Int(1), SpinValue::Int(5)]]);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &args).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_filter_by_multi_key_correlation_in_skip_trap_lookup() {
    // §5 步骤 4 的 sd_partner 形态（多等值键）：维表按 (sid, mkt) 双键
    // 相关，GROUP BY 双键派生表 LEFT JOIN；任一键落空 → 0 行跳行。
    let proc = "CREATE PROCEDURE copy_rows IS v_n INTEGER; BEGIN \
        FOR r IN SELECT id, payload, mkt FROM source_rows LOOP \
        BEGIN \
        SELECT t.n INTO v_n FROM dim2 t WHERE t.sid = r.payload AND t.mkt = r.mkt; \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_n); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for sql in [
            "CREATE TABLE source_rows(id BIGINT, payload VARCHAR, mkt VARCHAR)",
            "CREATE TABLE target_rows(id BIGINT, payload INTEGER)",
            "CREATE TABLE dim2(sid VARCHAR, mkt VARCHAR, n INTEGER)",
            "INSERT INTO dim2 VALUES ('a', 'SH', 5), ('a', 'SZ', 7), ('b', 'SH', 9)",
            "INSERT INTO source_rows VALUES (1, 'a', 'SH'), (2, 'a', 'SZ'), (3, 'b', 'SZ'), (4, 'b', 'SH')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng.register(proc).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(
        want,
        vec![
            vec![SpinValue::Int(1), SpinValue::Int(5)],
            vec![SpinValue::Int(2), SpinValue::Int(7)],
            vec![SpinValue::Int(4), SpinValue::Int(9)],
        ]
    );

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_coalesce_hit_null_via_nvl_projection_in_skip_trap_lookup() {
    // §5 步骤 4 的 v_purchase_days 形态：投影 nvl(t.n, 1)。命中 NULL →
    // 取常量 1 保留行（源 SQL 显式 coalesce，区别于引擎擅自 COALESCE）；
    // 落空 → NO_DATA_FOUND 整行过滤。hit 判定仍按行计数（sd_cnt）。
    let proc = "CREATE PROCEDURE copy_rows IS v_n INTEGER; BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN \
        SELECT nvl(t.n, 1) INTO v_n FROM dim_days t WHERE t.sid = r.payload; \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_n); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine();
        eng.ex
            .execute("DELETE FROM dim_days", &BindMap::new())
            .unwrap();
        eng.ex
            .execute(
                "INSERT INTO dim_days VALUES ('a', 5), ('b', NULL)",
                &BindMap::new(),
            )
            .unwrap();
        eng.register(proc).unwrap();
        eng
    };
    let mut baseline = setup();
    seed(&baseline, &[(1, "a"), (2, "b"), (3, "z")]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(
        want,
        vec![
            vec![SpinValue::Int(1), SpinValue::Int(5)],
            vec![SpinValue::Int(2), SpinValue::Int(1)],
        ]
    );

    let mut eng = setup();
    seed(&eng, &[(1, "a"), (2, "b"), (3, "z")]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}
