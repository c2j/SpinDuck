//! §5.1-10 / 设计 §4：`prc_trd_hz_byfund` 区域 0（`c_get_data`）集合化
//! 与后缀原 PC 解释的端到端守护。缩小夹具镜像真实结构：前缀 DELETE+COMMIT
//! → 静态游标 `c_get_data`（区域 0，BatchAtomic Converted，当场提交）→
//! 动态游标 `c_fxj`（`OPEN c_fxj FOR`，原解释器）→ `qfqscjhbrbb_temp`
//! QFII 段（混合成功/失败行，原解释器，绝对 GOTO/异常处理按原 PC）。
//! 投影由过程 AST 推导，不在夹具中硬编码 §4.2 的 28 列。
#![cfg(feature = "native")]

use spinduck_core::ir::set_oriented::{SetOrientedMode, Status};
use spinduck_core::value::SpinValue;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry};

include!("fixtures/region0.rs");

fn engine_with(oriented: SetOrientedMode) -> Engine {
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    eng.batch_commit = false;
    for sql in DDL
        .iter()
        .chain(SEEDS.iter())
        .chain([INST_LOG, BYFUND].iter())
    {
        if sql.contains("PROCEDURE") {
            eng.register(sql).unwrap();
        } else {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
    }
    eng.set_oriented = oriented;
    eng
}

fn rows(eng: &Engine, sql: &str) -> Vec<Vec<SpinValue>> {
    eng.ex.query(sql, &BindMap::new()).unwrap().fetch_many(100)
}

/// 四张目标表全列排序后逐行全等（含 qfqscjhbrbb_temp 的成功/失败行）。
fn assert_same_state(eng: &Engine, off: &Engine) {
    for (name, order) in [
        ("dat_fund_cjqs", "bcrq, zqdm, gddm, xwdm, bs, amt, tag, req"),
        (
            "qfqscjhbrbb_temp",
            "seq, gddm, zqdm, req, split_err_msg, split_flag",
        ),
        ("inst_log_log", "code, msg"),
        ("fxj_audit", "id, note"),
    ] {
        let sql = format!("SELECT * FROM {name} ORDER BY {order}");
        assert_eq!(rows(eng, &sql), rows(off, &sql), "{name} 须与 Off 逐行全等");
    }
}

#[test]
fn should_convert_region0_of_byfund_and_match_off_on_small_corpus() {
    let mut off = engine_with(SetOrientedMode::Off);
    let expected = off
        .call(
            "prc_trd_hz_byfund_small",
            &[SpinValue::Str("2024-01-01".into()), SpinValue::Null],
        )
        .unwrap();

    let mut eng = engine_with(SetOrientedMode::BatchAtomic);
    let actual = eng
        .call(
            "prc_trd_hz_byfund_small",
            &[SpinValue::Str("2024-01-01".into()), SpinValue::Null],
        )
        .unwrap();

    // 区域 0 报告 Converted；c_fxj / QFII 不是被转换的区域（无第二份报告）。
    let reports = eng.set_oriented_reports();
    assert_eq!(reports.len(), 1, "仅区域 0 应产生报告: {reports:?}");
    assert!(
        matches!(reports[0].status, Status::Converted { .. }),
        "区域 0 必须 Converted: {reports:?}"
    );
    // bcrq 投影必须是 IN 形参 p_i_date 的占位符本身（§4.2 第 7 列），
    // 不是与之值相等的游标字段。
    let Status::Converted { sql, .. } = &reports[0].status else {
        unreachable!("上方已断言 Converted")
    };
    assert!(
        sql.contains("$v_p_i_date"),
        "批写 SQL 须投影 IN 形参占位符: {sql}"
    );

    assert_eq!(
        actual.outs.get("p_o_succeed"),
        expected.outs.get("p_o_succeed"),
        "p_o_succeed 须与 Off 一致"
    );
    assert_same_state(&eng, &off);

    // 夹具自检：qfqscjhbrbb_temp 必须同时含成功行与失败行（失败行未被
    // 改成成功），dat_fund_cjqs 必须含区域 0 与 QFII 成功行的写入。
    let temp = rows(
        &off,
        "SELECT split_flag, COUNT(*) FROM qfqscjhbrbb_temp GROUP BY split_flag ORDER BY split_flag",
    );
    assert!(
        temp.iter().any(|r| r[0] == SpinValue::Str("0".into())),
        "qfqscjhbrbb_temp 必须有成功行: {temp:?}"
    );
    assert!(
        temp.iter().any(|r| r[0] == SpinValue::Str("1".into())),
        "qfqscjhbrbb_temp 必须有失败行: {temp:?}"
    );
    let cjqs = rows(&off, "SELECT COUNT(*) FROM dat_fund_cjqs");
    assert!(cjqs[0][0] != SpinValue::Int(0), "dat_fund_cjqs 不应为空");
}
