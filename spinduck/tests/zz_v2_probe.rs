#![cfg(feature = "native")]
//! 临时探针：发现语料过程 V2 转换的下一个 SkipReason（用后即删）。

use spinduck_core::engine::Engine;
use spinduck_core::exec::BindMap;
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::project::ProjectConfig;
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::DuckExecutor;
use spinduck_core::value::SpinValue;
use spinduck_core::verify::deploy_project;
use std::path::Path;

#[test]
fn zz_probe_v2_corpus() {
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
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let args = [SpinValue::Str("20250715".into()), SpinValue::Null];
    let r = eng.call("pkg_split_trade_step3_sh.prc_trd_deal_purchase", &args);
    match &r {
        Ok(x) => eprintln!("PROBE_CALL_OK ret={:?} outs={:?}", x.return_value, x.outs),
        Err(e) => eprintln!("PROBE_CALL_ERR code={} msg={}", e.code, e.message),
    }
    for rep in eng.set_oriented_reports() {
        eprintln!(
            "PROBE_REPORT routine={} status={:?}",
            rep.routine, rep.status
        );
    }
    let rows = eng
        .ex
        .query(
            "SELECT security_id, partner_code, purchase_quantity, purchase_money, purchase_fee, purchase_yj, purchase_days, purchase_kind FROM dat_trd_repurchase ORDER BY security_id",
            &BindMap::new(),
        )
        .unwrap()
        .fetch_many(100);
    eprintln!("PROBE_ROWS={:?}", rows);
    for t in ["mid_yjqs_detail", "dat_trd_repurchase"] {
        let ddl = eng
            .ex
            .query(
                &format!("SELECT sql FROM duckdb_tables() WHERE lower(table_name) = '{t}'"),
                &BindMap::new(),
            )
            .unwrap()
            .fetch_many(1);
        eprintln!("PROBE_DDL {t} = {ddl:?}");
    }
}
