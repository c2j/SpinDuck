//! 受 trap 保护的表达式 INSERT 的列级守卫守护（V2 设计 §4.5 第 3 项情形 B）。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

const GUARDED_DIV: &str = "CREATE PROCEDURE copy_div IS BEGIN \
    FOR r IN SELECT a, b FROM source_rows LOOP \
    BEGIN INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
    EXCEPTION WHEN OTHERS THEN NULL; END; \
    END LOOP; END;";

fn engine(target: &str) -> Engine {
    let eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in [
        "CREATE TABLE source_rows(a BIGINT, b BIGINT)",
        target,
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng
}

fn seed(eng: &Engine, values: &str) {
    eng.ex
        .execute(
            &format!("INSERT INTO source_rows VALUES {values}"),
            &BindMap::new(),
        )
        .unwrap();
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    eng.ex
        .query(
            "SELECT val FROM target_rows ORDER BY rowid",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10)
}

/// 原生 SQL `0/0` 经 TRY_CAST 写进 VARCHAR 的拼法随平台不同（"nan"/"-nan"）；
/// inf / 2.0 / NULL 三行逐字锁定。
fn assert_varchar_div_rows(got: &[Vec<SpinValue>]) {
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0], vec![SpinValue::Str("inf".into())]);
    assert!(
        matches!(&got[1][0], SpinValue::Str(v) if v == "nan" || v == "-nan"),
        "0/0 row must be nan or -nan: {got:?}"
    );
    assert_eq!(got[2], vec![SpinValue::Str("2.0".into())]);
    assert_eq!(got[3], vec![SpinValue::Null]);
}

#[path = "set_oriented_expr_concat.rs"]
mod concat;
#[path = "set_oriented_expr_decimal.rs"]
mod decimal;
#[path = "set_oriented_expr_round.rs"]
mod round;

#[test]
fn should_convert_guarded_native_division_into_decimal_with_column_guard() {
    // Off 基线实测：1/0→inf 与 0/0→nan 在 DECIMAL(15,3) 转换阶段报错被
    // trap 跳行；6/3→2.000 与 NULL/0→NULL 落库。转换必须以列级 TRY_CAST
    // 守卫精确复现该跳行集，且不得套用 PL 除零守卫误删 NULL 行。
    let mut baseline = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
    seed(&baseline, "(1, 0), (6, 3), (NULL, 0), (0, 0)");
    baseline.register(GUARDED_DIV).unwrap();
    baseline.call("copy_div", &[]).unwrap();
    let expected = vec![vec![SpinValue::Decimal(2000, 38, 3)], vec![SpinValue::Null]];
    assert_eq!(rows(&baseline), expected);

    let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
    seed(&eng, "(1, 0), (6, 3), (NULL, 0), (0, 0)");
    eng.register(GUARDED_DIV).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
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
fn should_keep_inf_nan_rows_when_guarded_division_targets_varchar() {
    // TRY_CAST(inf/nan AS VARCHAR) 非空 → 合法行不得被守卫误删。
    // 0/0 行的 nan/-nan 拼法随平台不同：Off 先跑做形状检查，
    // BatchAtomic 必须与同次运行的 Off 逐行一致（含 NaN 符号）。
    let mut off = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&off, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
    off.register(GUARDED_DIV).unwrap();
    off.call("copy_div", &[]).unwrap();
    let expected = rows(&off);
    assert_varchar_div_rows(&expected);

    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(1, 0), (0, 0), (6, 3), (NULL, 0)");
    eng.register(GUARDED_DIV).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
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
fn should_filter_null_rows_for_not_null_target_column() {
    // NOT NULL 列：NULL/0 的合法 NULL 表达式值在 Off 下触发约束报错被
    // trap 跳行；守卫必须以 (e) IS NOT NULL 精确复现。
    let mut baseline = engine("CREATE TABLE target_rows(val DECIMAL(15,3) NOT NULL)");
    seed(&baseline, "(6, 3), (NULL, 1)");
    baseline.register(GUARDED_DIV).unwrap();
    baseline.call("copy_div", &[]).unwrap();
    let expected = vec![vec![SpinValue::Decimal(2000, 38, 3)]];
    assert_eq!(rows(&baseline), expected);

    let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3) NOT NULL)");
    seed(&eng, "(6, 3), (NULL, 1)");
    eng.register(GUARDED_DIV).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
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
fn should_reject_expression_insert_without_skip_trap() {
    // 无 trap：坏行报错直接向上传播，过滤坏行即改语义，必须整段跳过。
    let sql = "CREATE PROCEDURE copy_div IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        INSERT INTO target_rows(val) VALUES (r.a / r.b); \
        END LOOP; END;";
    let mut baseline = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&baseline, "(1, 0)");
    baseline.register(sql).unwrap();
    baseline.call("copy_div", &[]).unwrap();
    let expected = vec![vec![SpinValue::Str("inf".into())]];
    assert_eq!(rows(&baseline), expected);

    let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
    seed(&eng, "(1, 0)");
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedWrite
        }
    );
}

#[test]
fn should_reject_strict_guarded_expression_as_unproven_rollback_scope() {
    // Strict × trap：即使列级守卫可过滤坏行，reset_session 回滚域分歧
    // 仍不可证明 → UnprovenRollbackScope（设计 §4.5 第 1 项）。
    let sql = "CREATE PROCEDURE copy_div IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.a / r.b); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut baseline = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
    seed(&baseline, "(1, 0), (6, 3), (NULL, 0)");
    baseline.register(sql).unwrap();
    baseline.call("copy_div", &[]).unwrap();
    let expected = rows(&baseline);

    let mut eng = engine("CREATE TABLE target_rows(val DECIMAL(15,3))");
    seed(&eng, "(1, 0), (6, 3), (NULL, 0)");
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenRollbackScope
        }
    );
}

#[test]
fn should_preserve_valid_null_rows_for_round_null_div_bad_divisor() {
    // 设计 §6.2 命名守护：NULL / 'bad' 经 round(...,2) 写可空 DECIMAL
    // 合法保留 NULL 行（DuckDB NULL 传播先于 'bad' 转换）；优化器不得
    // 捏造“除数不可转 DOUBLE 即过滤”的拦截规则误删该行。
    let sql = "CREATE PROCEDURE copy_div IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (round(r.a / r.b, 2)); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a BIGINT, b VARCHAR)",
            "CREATE TABLE target_rows(val DECIMAL(15,3))",
            "INSERT INTO source_rows VALUES (NULL, 'bad'), (6, '3'), (1, '0')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_div", &[]).unwrap();
    let expected = vec![vec![SpinValue::Null], vec![SpinValue::Decimal(2000, 38, 3)]];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenWriteCompleteness
        }
    );
}

#[test]
fn should_sink_pl_integer_division_with_pl_zero_guard() {
    // 设计 §4.5 第 3 项情形 A + §4.6：PL `v := r.a / r.b` 下沉为精确整数
    // 除法 `//`（禁浮点/TRUNC），PL 除零守卫 (a) IS NULL OR (b) IS NULL
    // OR (b) <> 0 复现 BIZ-0124 跳行；NULL/0 合法保留。
    let sql = "CREATE PROCEDURE copy_pl IS v NUMBER; BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a / r.b; INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine("CREATE TABLE target_rows(val BIGINT)");
        seed(&eng, "(1, 2), (9007199254740993, 1), (NULL, 0), (7, 0)");
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[]).unwrap();
    let expected = vec![
        vec![SpinValue::Int(0)],
        vec![SpinValue::Int(9007199254740993)],
        vec![SpinValue::Null],
    ];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
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
fn should_reject_strict_pl_division_sinking() {
    // PL 除法下沉仅在 BatchAtomic × trap 授权；Strict 下过滤除零行与
    // reset_session 回滚域不等价，必须整段跳过且行为与 Off 逐字一致。
    let sql = "CREATE PROCEDURE copy_pl IS v NUMBER; BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a / r.b; INSERT INTO target_rows(val) VALUES (v); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine("CREATE TABLE target_rows(val BIGINT)");
        seed(&eng, "(1, 2), (7, 0), (NULL, 0)");
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[]).unwrap();
    let expected = rows(&baseline);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnsupportedControlFlow
        }
    );
}

#[test]
fn should_reject_pl_sinking_on_non_integer_source_column() {
    // `//` 只等价于 SpinValue Int/Int 截断除法；源列为 DOUBLE 时 PL 走
    // 浮点除法，下沉即改语义 → UnprovenWriteCompleteness，行为不变。
    let sql = "CREATE PROCEDURE copy_pl IS v NUMBER; BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a / r.b; INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a DOUBLE, b BIGINT)",
            "CREATE TABLE target_rows(val BIGINT)",
            "INSERT INTO source_rows VALUES (3, 2), (7, 0)",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[]).unwrap();
    let expected = rows(&baseline);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenWriteCompleteness
        }
    );
}

#[test]
fn should_reject_pl_sinking_into_varchar_slot() {
    // Varchar 槽有 coerce_for_slot 字符串强转，值域不可证等价 →
    // UnprovenExpression，不得下沉。
    let sql = "CREATE PROCEDURE copy_pl IS v VARCHAR2(10); BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a / r.b; INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine("CREATE TABLE target_rows(val VARCHAR)");
        seed(&eng, "(7, 2), (NULL, 0)");
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[]).unwrap();
    let expected = rows(&baseline);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_pl", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenExpression
        }
    );
}

#[test]
fn should_reject_pl_sinking_when_sink_var_is_out_param() {
    // 下沉变量是 OUT 形参时出口写回不可消除 → LiveOutState。
    let sql = "CREATE PROCEDURE copy_pl(v OUT NUMBER) IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN v := r.a / r.b; INSERT INTO target_rows(val) VALUES (v); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = engine("CREATE TABLE target_rows(val BIGINT)");
        seed(&eng, "(1, 2), (7, 0)");
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_pl", &[SpinValue::Null]).unwrap();
    let expected = rows(&baseline);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_pl", &[SpinValue::Null]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::LiveOutState
        }
    );
}

#[test]
fn should_reject_varchar_divisor_as_unproven_write_completeness() {
    // VARCHAR 除数的隐式转换可在求值阶段抛错（'bad'），无法证明不抛错
    // → 保守退回，绝不捏造行级过滤。
    let sql = "CREATE PROCEDURE copy_div IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.a / r.b); COMMIT; \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for ddl in [
        "CREATE TABLE source_rows(a BIGINT, b VARCHAR)",
        "CREATE TABLE target_rows(val DECIMAL(15,3))",
        "INSERT INTO source_rows VALUES (6, '3'), (1, 'bad')",
        "CREATE TABLE sys_dummy(dummy VARCHAR)",
        "INSERT INTO sys_dummy VALUES ('X')",
    ] {
        eng.ex.execute(ddl, &BindMap::new()).unwrap();
    }
    eng.register(sql).unwrap();
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_div", &[]).unwrap();
    assert_eq!(rows(&eng), vec![vec![SpinValue::Decimal(2000, 38, 3)]]);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenWriteCompleteness
        }
    );
}

#[test]
fn should_reject_bad_row_filtering_in_strict_mode_when_prior_write_uncommitted() {
    // 设计 §6.2 命名守护：插入 1 → 异常写入 bad → 插入 3。Off/Strict 解释
    // 路径中 bad 行报错经 reset_session 撤销未提交的 1，合计为 3；Strict
    // 若以列级守卫过滤坏行会误保留 1（合计 4），必须拒绝转换。
    let sql = "CREATE PROCEDURE copy_cast IS BEGIN \
        FOR r IN SELECT a, b FROM source_rows LOOP \
        BEGIN INSERT INTO target_rows(val) VALUES (r.b); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = || {
        let mut eng = Engine::new(
            Box::new(DuckExecutor::open_in_memory().unwrap()),
            RuleRegistry::default(),
        );
        for ddl in [
            "CREATE TABLE source_rows(a BIGINT, b VARCHAR)",
            "CREATE TABLE target_rows(val BIGINT)",
            "INSERT INTO source_rows VALUES (1, '1'), (2, 'bad'), (3, '3')",
            "CREATE TABLE sys_dummy(dummy VARCHAR)",
            "INSERT INTO sys_dummy VALUES ('X')",
        ] {
            eng.ex.execute(ddl, &BindMap::new()).unwrap();
        }
        eng.register(sql).unwrap();
        eng
    };
    let mut baseline = setup();
    baseline.call("copy_cast", &[]).unwrap();
    let expected = vec![vec![SpinValue::Int(3)]];
    assert_eq!(rows(&baseline), expected);

    let mut eng = setup();
    eng.set_oriented = SetOrientedMode::Strict;
    eng.call("copy_cast", &[]).unwrap();
    assert_eq!(rows(&eng), expected);
    assert_eq!(
        eng.set_oriented_reports()[0].status,
        Status::Skipped {
            reason: SkipReason::UnprovenRollbackScope
        }
    );
}
