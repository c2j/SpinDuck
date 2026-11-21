#![cfg(feature = "native")]
use spinduck_core::exec::{ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::ir::set_oriented::SetOrientedMode;
use spinduck_core::project::ProjectConfig;
use spinduck_core::verify::deploy_project;
use spinduck_core::{BindMap, DuckExecutor, Engine, EngineError, RuleRegistry, SpinValue};
use std::path::Path;

struct DE(DuckExecutor);
impl SqlExecutor for DE {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.query(sql, binds)
    }
    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.0.execute(sql, binds)
    }
}

#[test]
fn probe() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("golden/case_corpus_purchase");
    let project = ProjectConfig::load(&root).unwrap();
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap();
    let mut eng = Engine::new(Box::new(DE(DuckExecutor::open_in_memory().unwrap())), rules);
    deploy_project(&mut eng, &project).unwrap();
    // datasets_byfund 种子：清空共享业务表后灌入（与 purchase 种子表名相同）
    for t in [
        "mid_yjqs_detail",
        "par_fund_info",
        "par_fund_partner",
        "par_fund_seat",
        "par_oper_venture",
        "dic_all_kind",
    ] {
        let _ = eng.ex.execute(&format!("DELETE FROM {t}"), &BindMap::new());
    }
    let seed = std::fs::read_to_string(root.join("cases/datasets_byfund/seed.sql")).unwrap();
    for stmt in seed.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if let Err(e) = eng.ex.execute(stmt, &BindMap::new()) {
            eprintln!("SEED_SKIP {} -> {}", &stmt[..stmt.len().min(60)], e.message);
        }
    }
    // dump IR: 找含 v_return_rate_flag 的条件跳转及邻近语句
    for name in eng.catalog.names() {
        if name.contains("hz_byfund") {
            let r = eng.catalog.lookup(&name).unwrap();
            for (i, st) in r.prog.code.iter().enumerate() {
                let d = format!("{:?}", st);
                if d.contains("v_return_rate_flag") || d.contains("v_return_rate_org") {
                    for j in i.saturating_sub(3)..=(i + 4).min(r.prog.code.len() - 1) {
                        eprintln!("IR{} {:?}", j, r.prog.code[j]);
                    }
                    eprintln!("---");
                }
                if let spinduck_core::ir::IrStmt::OpenCursor {
                    source:
                        spinduck_core::ir::CursorSource::Static {
                            sql,
                            binds,
                            fn_sites,
                        },
                    ..
                } = st
                {
                    if !fn_sites.is_empty() {
                        eprintln!("CURSOR_SQL {}", sql);
                        eprintln!("CURSOR_BINDS {:?}", binds);
                        for s in fn_sites {
                            eprintln!(
                                "CSITE table={} name={} args={:?}\n  distinct={}\n  binds={:?}",
                                s.table, s.name, s.args, s.distinct_sql, s.binds
                            );
                        }
                    }
                }
                if let spinduck_core::ir::IrStmt::SqlExec {
                    sql,
                    binds,
                    fn_sites,
                    ..
                } = st
                {
                    if !fn_sites.is_empty() {
                        eprintln!("FNSITE_SQL {}", sql);
                        eprintln!("FNSITE_BINDS {:?}", binds);
                        for s in fn_sites {
                            eprintln!(
                                "SITE table={} name={} args={:?}\n  distinct={}\n  binds={:?}",
                                s.table, s.name, s.args, s.distinct_sql, s.binds
                            );
                        }
                    }
                }
            }
        }
    }
    eng.set_oriented = SetOrientedMode::BatchAtomic;
    let args = [SpinValue::Str("20250715".into()), SpinValue::Null];
    match eng.call("pkg_split_trade_step3_sh.prc_trd_hz_byfund", &args) {
        Ok(o) => eprintln!("CALL_OK outs={:?} ret={:?}", o.outs, o.return_value),
        Err(e) => eprintln!("CALL_ERR {}", e.message),
    }
    for r in eng.set_oriented_reports() {
        eprintln!("REPORT {:?}", r.status);
    }
}
