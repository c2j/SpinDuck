//! 集合化 inline 语义守护。

use super::*;

#[test]
fn should_inline_pure_function_call_into_batch_insert() {
    use spinduck_core::ir::set_oriented::Status;
    // 纯计算函数（单 RETURN，无读写/异常）内联为批写投影。Concat 须保持
    // PL 语义：单侧 NULL 视作空串（'X'），双侧 NULL 才 NULL——原生 `||`
    // 单侧 NULL 即 NULL，会篡改第三行，守护必须逐字比对 Off。
    let func = "CREATE FUNCTION f_concat(p_a VARCHAR2, p_b VARCHAR2) RETURN VARCHAR2 IS \
        BEGIN RETURN p_a || p_b; END;";
    let proc_sql = "CREATE PROCEDURE copy_rows IS v_x VARCHAR2(40); BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        v_x := f_concat(r.payload, 'X'); \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(func).unwrap();
    baseline.register(proc_sql).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    // 第三行 payload NULL → concat(NULL,'X')='X'（单侧 NULL 视作空串）。
    assert_eq!(want[2][1], SpinValue::Str("X".into()));

    let mut eng = engine();
    eng.register(func).unwrap();
    eng.register(proc_sql).unwrap();
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

fn date_arithmetic_engine(dates: &[&str]) -> Engine {
    let func = "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n INTEGER) RETURN VARCHAR2 IS \
        BEGIN RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;";
    let proc_sql = "CREATE PROCEDURE copy_rows IS v_x VARCHAR2(40); BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN v_x := f_getday(r.payload, 1); \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let seed = |eng: &Engine, dates: &[&str]| {
        eng.ex
            .execute("DELETE FROM source_rows", &BindMap::new())
            .unwrap();
        for (i, d) in dates.iter().enumerate() {
            eng.ex
                .execute(
                    &format!("INSERT INTO source_rows VALUES ({}, '{d}')", i + 1),
                    &BindMap::new(),
                )
                .unwrap();
        }
    };
    let mut eng = engine();
    eng.register(func).unwrap();
    eng.register(proc_sql).unwrap();
    seed(&eng, dates);
    eng
}

#[test]
fn should_guard_inlined_date_arithmetic_at_entry_and_fall_back_on_bad_row() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    // 日期算术纯函数内联（§4.3 A7 + §4.5-2）：to_char(to_date(d)+n) 的
    // strptime 对坏日期抛错。入口无抛错守卫以 TRY_ 全定义预筛全部可达行：
    // 全好日期 → Converted；含坏日期行 → 守卫失败 → 无损退回原区域（trap 跳行）。
    // 全好日期：内联转换，日期 +1 天（2024 闰年 2/28 → 2/29）。
    let mut baseline = date_arithmetic_engine(&["20240101", "20240228"]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(want[0][1], SpinValue::Str("20240102".into()));
    assert_eq!(want[1][1], SpinValue::Str("20240229".into()));
    let mut eng = date_arithmetic_engine(&["20240101", "20240228"]);
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

    // 含坏日期行：入口守卫预筛失败 → 退回原区域。Off 语义下坏行赋值抛错
    // 被 trap 捕获（过程 Ok 返回）但整个事务回滚 → 0 行；退回侧须逐字一致。
    let mut baseline = date_arithmetic_engine(&["20240101", "notadate"]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(want.len(), 0);
    let mut eng = date_arithmetic_engine(&["20240101", "notadate"]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnprovenQueryTotalness
            }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_inline_nested_pure_function_call_into_batch_insert() {
    use spinduck_core::ir::set_oriented::Status;
    // 嵌套纯调用（§4.3 A7）：f_outer 体调用纯函数 f_getday，须递归展开内联
    // 为批写投影，日期 +1 天结果与 Off 逐字一致。
    let getday = "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n INTEGER) RETURN VARCHAR2 IS \
        BEGIN RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;";
    let outer = "CREATE FUNCTION f_outer(p_d VARCHAR2) RETURN VARCHAR2 IS \
        BEGIN RETURN f_getday(p_d, 1); END;";
    let proc_sql = "CREATE PROCEDURE copy_rows IS v_x VARCHAR2(40); BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN v_x := f_outer(r.payload); \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let seed = |eng: &Engine| {
        eng.ex
            .execute("DELETE FROM source_rows", &BindMap::new())
            .unwrap();
        eng.ex
            .execute(
                "INSERT INTO source_rows VALUES (1, '20240101'), (2, '20240228')",
                &BindMap::new(),
            )
            .unwrap();
    };
    let mut baseline = engine();
    baseline.register(getday).unwrap();
    baseline.register(outer).unwrap();
    baseline.register(proc_sql).unwrap();
    seed(&baseline);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(want[0][1], SpinValue::Str("20240102".into()));
    assert_eq!(want[1][1], SpinValue::Str("20240229".into()));
    let mut eng = engine();
    eng.register(getday).unwrap();
    eng.register(outer).unwrap();
    eng.register(proc_sql).unwrap();
    seed(&eng);
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
fn should_inline_function_internal_lookup_into_batch_insert_with_fallback() {
    use spinduck_core::ir::set_oriented::Status;
    // 函数内查找（§4.3 A7 LookupExpr）：f_lk 体内 trap 包裹 SELECT INTO +
    // WHEN OTHERS 兜底 '0'，须关系化为计数门控相关子查询。0/>1 行→兜底，
    // 恰 1 行→值（含 NULL 保留），与 Off 逐字一致。
    let lk = "CREATE FUNCTION f_lk(p_sid VARCHAR2) RETURN VARCHAR2 IS v_days VARCHAR2(8); \
        BEGIN BEGIN SELECT t.purchase_days INTO v_days FROM par_sys_purchase t \
        WHERE t.security_id = p_sid; EXCEPTION WHEN OTHERS THEN v_days := '0'; END; \
        RETURN v_days; END;";
    let proc_sql = "CREATE PROCEDURE copy_rows IS v_x VARCHAR2(40); BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN v_x := f_lk(r.payload); \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = |eng: &Engine| {
        for sql in [
            "CREATE TABLE par_sys_purchase(security_id VARCHAR, purchase_days VARCHAR)",
            "INSERT INTO par_sys_purchase VALUES ('s1', '5'), ('s2', '7'), ('s2', '9'), ('s3', NULL)",
            "DELETE FROM source_rows",
            "INSERT INTO source_rows VALUES (1, 's0'), (2, 's1'), (3, 's2'), (4, 's3')",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
    };
    let mut baseline = engine();
    baseline.register(lk).unwrap();
    baseline.register(proc_sql).unwrap();
    setup(&baseline);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(want[0][1], SpinValue::Str("0".into()));
    assert_eq!(want[1][1], SpinValue::Str("5".into()));
    assert_eq!(want[2][1], SpinValue::Str("0".into()));
    assert_eq!(want[3][1], SpinValue::Null);
    let mut eng = engine();
    eng.register(lk).unwrap();
    eng.register(proc_sql).unwrap();
    setup(&eng);
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

fn date_lookup_engine(sids: &[&str]) -> Engine {
    let days = "CREATE FUNCTION f_days(p_sid VARCHAR2) RETURN INTEGER IS v_d INTEGER; \
        BEGIN BEGIN SELECT t.purchase_days INTO v_d FROM par_sys_purchase t \
        WHERE t.security_id = p_sid; EXCEPTION WHEN OTHERS THEN v_d := 0; END; \
        RETURN v_d; END;";
    let getday = "CREATE FUNCTION f_getday(p_d VARCHAR2, p_n INTEGER) RETURN VARCHAR2 IS \
        BEGIN RETURN to_char(to_date(p_d, 'YYYYMMDD') + p_n, 'YYYYMMDD'); END;";
    let proc_sql = "CREATE PROCEDURE copy_rows IS v_n INTEGER; v_x VARCHAR2(40); BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        BEGIN v_n := f_days(r.payload); \
        v_x := f_getday('20240101', v_n); \
        INSERT INTO target_rows(id, payload) VALUES (r.id, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let setup = |eng: &Engine, sids: &[&str]| {
        for sql in [
            "CREATE TABLE par_sys_purchase(security_id VARCHAR, purchase_days INTEGER)",
            "INSERT INTO par_sys_purchase VALUES ('s1', 5), ('s2', 7), ('s2', 9), ('s3', NULL)",
            "DELETE FROM source_rows",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        for (i, s) in sids.iter().enumerate() {
            eng.ex
                .execute(
                    &format!("INSERT INTO source_rows VALUES ({}, '{s}')", i + 1),
                    &BindMap::new(),
                )
                .unwrap();
        }
    };
    let mut eng = engine();
    eng.register(days).unwrap();
    eng.register(getday).unwrap();
    eng.register(proc_sql).unwrap();
    setup(&eng, sids);
    eng
}

#[test]
fn should_inline_date_plus_lookup_days_into_batch_insert() {
    use spinduck_core::ir::set_oriented::{SkipReason, Status};
    // 日期 ± 查找天数（§4.3 A7 + §4.4 B1）：v_n := f_days(sid)（trap 包裹
    // SELECT INTO + WHEN OTHERS 兜底 0），v_x := to_char(to_date(d)+v_n)。
    // f_days 内联为 Lookup 叶子，须提升为 GROUP BY 派生表 LEFT JOIN（相关
    // 聚合标量子查询深嵌套会触发内核规划栈溢出），合成变量替换为计数门控
    // CASE。本守护在默认栈深下运行，既证等价又证栈溢出已消除。
    // 0/>1 行→兜底 0，恰 1 行→天数。
    // 全非 NULL 行：s1→5 天、s2 重复→兜底 0、s0 缺失→兜底 0 → Converted。
    let mut baseline = date_lookup_engine(&["s1", "s2", "s0"]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(want[0][1], SpinValue::Str("20240106".into()));
    assert_eq!(want[1][1], SpinValue::Str("20240101".into()));
    assert_eq!(want[2][1], SpinValue::Str("20240101".into()));
    let mut eng = date_lookup_engine(&["s1", "s2", "s0"]);
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

    // 含 NULL 命中行（s3）：日期算术结果 NULL → 入口守卫保守预筛失败 →
    // 无损退回原区域，Off 侧 NULL 行逐字保留。
    let mut baseline = date_lookup_engine(&["s1", "s3"]);
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);
    assert_eq!(want[1][1], SpinValue::Null);
    let mut eng = date_lookup_engine(&["s1", "s3"]);
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    eng.call("copy_rows", &[]).unwrap();
    assert_eq!(rows(&eng), want);
    assert!(
        matches!(
            eng.set_oriented_reports()[0].status,
            Status::Skipped {
                reason: SkipReason::UnprovenQueryTotalness
            }
        ),
        "{:?}",
        eng.set_oriented_reports()[0]
    );
}

#[test]
fn should_not_substitute_bind_placeholders_inside_string_literals() {
    use spinduck_core::ir::set_oriented::Status;
    // 字符串字面量中的 `$v_r__payload` 与 Field 绑定占位符同名：纯文本
    // replace 会把字面量内部的占位符文本一并替换，篡改常量值（§4.6 值等价）。
    let func = "CREATE FUNCTION f_tag(p_a VARCHAR2) RETURN VARCHAR2 IS \
        BEGIN RETURN '$v_r__payload' || p_a; END;";
    let proc_sql = "CREATE PROCEDURE copy_rows IS v_x VARCHAR2(40); BEGIN \
        FOR r IN SELECT id, payload FROM source_rows LOOP \
        v_x := f_tag(r.payload); \
        BEGIN INSERT INTO target_rows(id, payload) VALUES (r.id, v_x); \
        EXCEPTION WHEN OTHERS THEN NULL; END; \
        END LOOP; END;";
    let mut baseline = engine();
    baseline.register(func).unwrap();
    baseline.register(proc_sql).unwrap();
    baseline.call("copy_rows", &[]).unwrap();
    let want = rows(&baseline);

    let mut eng = engine();
    eng.register(func).unwrap();
    eng.register(proc_sql).unwrap();
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
