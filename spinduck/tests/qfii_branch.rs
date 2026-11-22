#![cfg(feature = "native")]
//! byfund QFII 分支业务成功验收：有效 qfqscjhbrbb_temp 输入必须真实落表
//! dat_fund_cjqs、源行标记 split_err_msg='成功'/split_flag='0'，且
//! Off 与 BatchAtomic 两模式结果逐行全等。此前嵌套 DECLARE 变量
//! （v_fund_code/v_security_id/v_seat_id）未绑定导致 INSERT 引用裸
//! 标识符，QFII 输出为零且源行未标成功——本测试锁定该回归。
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::project::ProjectConfig;
use spinduck_core::verify::deploy_project;
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};
use std::collections::BTreeMap;
use std::path::Path;

const SEED: &str = "\
INSERT INTO dic_all_kind(operation_kind, kind_id, kind_name) VALUES ('split_data_flag','qfii_dealer_date','20200101');\
INSERT INTO dic_stock_kind_sub(market_code, stock_kind, begin_code, end_code) VALUES ('001','01','600000','699999');\
INSERT INTO par_fund_partner(fund_code, partner_code, market_code, sub_partner_code, inure_begin_date, inure_end_date) VALUES ('F0000001','G0000000001','001','000000','20200101','99991231');\
INSERT INTO par_sys_securities(main_market_code, main_stock_code, stock_kind, stock_status, security_id, inure_begin_date, inure_end_date) VALUES ('001','600000','01','00','0000000000000000718','20200101','99991231');\
INSERT INTO par_fund_seat(fund_code, seat_code, market_code, seat_id, inure_begin_date, inure_end_date, valid_flag) VALUES ('F0000001','S01','001','SEAT0001','20200101','99991231','1');\
INSERT INTO qfqscjhbrbb_temp(gddm, gddmzm, gsdm, bcrq, bs, zqdm, cjsl, cjje, yjlx, yhs, jsf, qsf, zgf, ghf, yj, fxjj, seq_no) VALUES ('G0000000001','000000','S01','20250715','B','600000',100,1000.00,0.50,0,0,0,0,0,0,0,1);";

fn deploy(mode: SetOrientedMode) -> Engine {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("golden/case_corpus_purchase");
    let project = ProjectConfig::load(&root).unwrap();
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap();
    let mut eng = Engine::new(Box::new(DuckExecutor::open_in_memory().unwrap()), rules);
    deploy_project(&mut eng, &project).unwrap();
    // deploy_project 会带入项目默认数据集（purchase 种子）：清空与
    // byfund/QFII 链路相关的表，避免 purchase 种子行干扰本场景。
    for t in [
        "mid_yjqs_detail",
        "par_fund_info",
        "par_fund_partner",
        "par_fund_seat",
        "par_oper_venture",
        "dic_all_kind",
        "qfqscjhbrbb_temp",
        "dic_stock_kind_sub",
        "par_sys_securities",
        "dat_fund_cjqs",
    ] {
        eng.ex
            .execute(&format!("DELETE FROM {t}"), &BindMap::new())
            .unwrap();
    }
    for stmt in SEED.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        eng.ex.execute(stmt, &BindMap::new()).unwrap();
    }
    eng.set_oriented = mode;
    eng
}

fn snapshot(eng: &Engine) -> BTreeMap<String, Vec<String>> {
    let names = eng
        .ex
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'main' ORDER BY 1",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10_000);
    let mut out = BTreeMap::new();
    for row in names {
        let SpinValue::Str(name) = &row[0] else {
            continue;
        };
        if name.starts_with("sd_") {
            continue;
        }
        let mut rows: Vec<String> = eng
            .ex
            .query(&format!("SELECT * FROM \"{name}\""), &BindMap::new())
            .unwrap()
            .fetch_many(2_000_000)
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        rows.sort();
        out.insert(name.clone(), rows);
    }
    out
}

fn args() -> [SpinValue; 2] {
    [SpinValue::Str("20250715".into()), SpinValue::Null]
}

/// QFII 有效输入的业务断言：dat_fund_cjqs 恰一行且键列正确、
/// 源行标记成功；两模式快照与 OUT 全等。
fn assert_qfii_success(eng: &Engine) {
    let rows = eng
        .ex
        .query(
            "SELECT fund_code, xwdm, security_id, bs, cjsl, cjje, stock_kind, gddm, gddmzm, scdm \
             FROM dat_fund_cjqs",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10);
    assert_eq!(
        rows.len(),
        1,
        "dat_fund_cjqs 应恰有 QFII 汇总一行: {rows:?}"
    );
    let r = &rows[0];
    assert_eq!(r[0], SpinValue::Str("F0000001".into()), "fund_code");
    assert_eq!(r[1], SpinValue::Str("S01".into()), "xwdm");
    assert_eq!(
        r[2],
        SpinValue::Str("0000000000000000718".into()),
        "security_id 须来自 par_sys_securities 绑定"
    );
    assert_eq!(r[3], SpinValue::Str("1".into()), "bs: B→1");
    assert_eq!(r[6], SpinValue::Str("01".into()), "stock_kind");
    assert_eq!(r[7], SpinValue::Str("G0000000001".into()), "gddm");
    assert_eq!(r[8], SpinValue::Str("000000".into()), "gddmzm");
    assert_eq!(r[9], SpinValue::Str("001".into()), "scdm");

    let src = eng
        .ex
        .query(
            "SELECT split_err_msg, split_flag FROM qfqscjhbrbb_temp",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(10);
    assert_eq!(src.len(), 1);
    assert_eq!(src[0][0], SpinValue::Str("成功".into()), "源行须标记成功");
    assert_eq!(src[0][1], SpinValue::Str("0".into()), "split_flag=0");
}

#[test]
fn qfii_branch_persists_and_marks_source() {
    let mut off = deploy(SetOrientedMode::Off);
    let expected = off
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();
    assert_qfii_success(&off);
    let expected_state = snapshot(&off);

    let mut eng = deploy(SetOrientedMode::BatchAtomic);
    let actual = eng
        .call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args())
        .unwrap();
    assert_qfii_success(&eng);
    assert_eq!(actual.outs, expected.outs, "OUT 形参须与 Off 一致");
    assert_eq!(
        snapshot(&eng),
        expected_state,
        "全部用户表须与 Off 逐行全等"
    );
}
