//! 完整回购语料的集合化与逐值类型等价守护。
#![cfg(feature = "native")]

use spinduck_core::exec::{ExecOutcome, RowStream, SqlExecutor};
use spinduck_core::ir::set_oriented::{SetOrientedMode, SkipReason, Status};
use spinduck_core::project::ProjectConfig;
use spinduck_core::verify::deploy_project;
use spinduck_core::{BindMap, DuckExecutor, Engine, EngineError, RuleRegistry, SpinValue};

struct DiagnosedExecutor(DuckExecutor);

impl SqlExecutor for DiagnosedExecutor {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        self.0.query(sql, binds).map_err(|mut error| {
            error.message = format!("{}; SQL={sql}; binds={:?}", error.message, binds.keys());
            error
        })
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.0.execute(sql, binds).map_err(|mut error| {
            error.message = format!("{}; SQL={sql}; binds={:?}", error.message, binds.keys());
            error
        })
    }
}

fn engine() -> Engine {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("golden/case_corpus_purchase");
    let project = ProjectConfig::load(&root).unwrap();
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap();
    let mut eng = Engine::new(
        Box::new(DiagnosedExecutor(DuckExecutor::open_in_memory().unwrap())),
        rules,
    );
    deploy_project(&mut eng, &project).unwrap();
    eng
}

fn rows(eng: &Engine) -> Vec<Vec<SpinValue>> {
    let mut stream = eng
        .ex
        .query(
            "SELECT * FROM dat_trd_repurchase ORDER BY security_id, partner_code, purchase_no",
            &BindMap::new(),
        )
        .unwrap();
    let mut rows = Vec::new();
    loop {
        let chunk = stream.fetch_many(1024);
        if chunk.is_empty() {
            break;
        }
        rows.extend(chunk);
    }
    rows
}

fn check_variant(label: &str, sql: &[&str], count: usize, skipped: Option<SkipReason>) {
    let setup = || {
        let eng = engine();
        for statement in sql {
            eng.ex.execute(statement, &BindMap::new()).unwrap();
        }
        eng
    };
    let routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase";
    let args = [SpinValue::Str("20250715".into()), SpinValue::Null];
    let mut off = setup();
    let expected = off.call(routine, &args).unwrap();
    let expected_rows = rows(&off);
    assert_eq!(expected_rows.len(), count, "Off: {label}");
    assert_eq!(
        expected.outs.get("p_o_succeed"),
        Some(&SpinValue::Str("0".into()))
    );
    let expected_hash = typed_hash(&off, &expected_rows);
    let (offline, offline_sql) = offline_engine();
    for (mut batch, candidate) in [(engine(), None), (offline, Some(offline_sql))] {
        for statement in sql {
            batch.ex.execute(statement, &BindMap::new()).unwrap();
        }
        batch.set_oriented = SetOrientedMode::BatchAtomic;
        let actual = batch.call(routine, &args).unwrap();
        assert_eq!(actual.outs, expected.outs, "OUT: {label}");
        assert_eq!(
            actual.return_value, expected.return_value,
            "return: {label}"
        );
        let actual_rows = rows(&batch);
        assert_eq!(actual_rows, expected_rows, "typed rows: {label}");
        assert_eq!(
            typed_hash(&batch, &actual_rows),
            expected_hash,
            "hash: {label}"
        );
        let status = &batch.set_oriented_reports()[0].status;
        match (skipped, candidate) {
            (Some(reason), _) => assert_eq!(*status, Status::Skipped { reason }, "{label}"),
            (None, Some(sql)) => assert_eq!(*status, Status::Converted { sql }, "{label}"),
            (None, None) => assert!(
                matches!(status, Status::Converted { .. }),
                "{label}: {status:?}"
            ),
        }
    }
    eprintln!("[corpus] {label}: rows={count} sha256={expected_hash}");
}

fn offline_engine() -> (Engine, String) {
    use spinduck_core::catalog::compile_sources;
    use spinduck_core::ir::set_oriented::compile_set_oriented_plans;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("golden/case_corpus_purchase");
    let project = ProjectConfig::load(&root).unwrap();
    let rules = RuleRegistry::default()
        .with_project_rules(&project.rules_files)
        .unwrap();
    let mut sources = project.stub_files().unwrap();
    sources.extend(project.routine_files().unwrap());
    let texts: Vec<_> = sources
        .iter()
        .map(|p| spinduck_core::cli::read_source_strict(p).unwrap())
        .collect();
    let compiled = compile_sources(
        &texts.iter().map(String::as_str).collect::<Vec<_>>(),
        rules.clone(),
    )
    .unwrap();
    let snapshot = offline_schema(&project, &rules);
    let routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase";
    let plans =
        compile_set_oriented_plans(&compiled.catalog, &snapshot, SetOrientedMode::BatchAtomic)
            .unwrap();
    let sql = plans
        .get(routine)
        .unwrap()
        .as_ref()
        .unwrap_or_else(|reason| panic!("{reason:?}: {:?}", snapshot.table(&["mid_yjqs_detail"])))
        .sql()
        .to_string();
    let mut batch = Engine::from_compiled(
        Box::new(DiagnosedExecutor(DuckExecutor::open_in_memory().unwrap())),
        compiled,
    )
    .unwrap();
    batch.set_oriented = SetOrientedMode::BatchAtomic;
    batch.attach_set_oriented_plans(plans);
    spinduck_core::verify::deploy_schema(&mut batch, &project).unwrap();
    spinduck_core::verify::deploy_seeds(&mut batch, &project).unwrap();
    (batch, sql)
}

fn offline_schema(
    project: &ProjectConfig,
    rules: &RuleRegistry,
) -> spinduck_core::ir::set_oriented::metadata::SchemaSnapshot {
    use spinduck_core::ir::set_oriented::metadata::SchemaSnapshot;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("codeweb.toml"),
        "[project]\nname = 'purchase'\n[analysis]\npaths = ['schema.sql']\n",
    )
    .unwrap();
    let ddl = project
        .ddl_files()
        .unwrap()
        .iter()
        .map(|p| spinduck_core::cli::read_source_strict(p).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(tmp.path().join("schema.sql"), ddl).unwrap();
    let export = codeweb::embed::EmbeddedProject::open_exact(tmp.path())
        .unwrap()
        .export_table_schemas()
        .unwrap();
    let snapshot = SchemaSnapshot::from_codeweb(&export, rules);
    assert!(
        snapshot.diagnostics.is_empty(),
        "{:?}",
        snapshot.diagnostics
    );
    snapshot
}

#[test]
fn should_compile_full_purchase_candidate_offline_and_reuse_it_with_typed_hash_equality() {
    let (mut batch, sql) = offline_engine();
    let routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase";
    let mut off = engine();
    let args = [SpinValue::Str("20250715".into()), SpinValue::Null];
    let expected = off.call(routine, &args).unwrap();
    let actual = batch.call(routine, &args).unwrap();
    assert_eq!(actual.outs, expected.outs);
    assert_eq!(actual.return_value, expected.return_value);
    let expected_rows = rows(&off);
    assert_eq!(expected_rows.len(), 2);
    assert_eq!(rows(&batch), expected_rows);
    assert_eq!(
        typed_hash(&batch, &rows(&batch)),
        typed_hash(&off, &expected_rows)
    );
    assert_eq!(
        batch.set_oriented_reports()[0].status,
        Status::Converted { sql }
    );
}

#[test]
fn should_convert_prc_trd_deal_purchase_and_match_golden_hashes() {
    check_variant("base", &[], 2, None);
    check_variant("duplicate lookup", &[
        "INSERT INTO par_sys_purchase SELECT * FROM par_sys_purchase WHERE security_id = '0130000001'",
    ], 1, None);
    check_variant(
        "missing partner",
        &["DELETE FROM par_fund_partner WHERE fund_code = 'F001'"],
        1,
        None,
    );
    check_variant(
        "missing inner seat",
        &["DELETE FROM par_fund_seat WHERE fund_code = 'F001'"],
        1,
        None,
    );
    check_variant(
        "zero divisor",
        &["UPDATE par_sys_purchase SET purchase_days = 0 WHERE security_id = '0130000001'"],
        1,
        None,
    );
    check_variant(
        "nullable lookup",
        &["UPDATE par_sys_purchase SET purchase_days = NULL WHERE security_id = '0130000001'"],
        2,
        Some(SkipReason::UnprovenQueryTotalness),
    );
    check_variant("empty switch", &["DELETE FROM swh_all_kind"], 2, None);
    check_variant(
        "duplicate switch",
        &["INSERT INTO swh_all_kind SELECT * FROM swh_all_kind"],
        2,
        None,
    );
    check_variant(
        "null money rejected",
        &["UPDATE mid_yjqs_detail SET cjje = NULL WHERE gddm = 'A0001'"],
        1,
        None,
    );
    check_variant(
        "quantity overflow rejected",
        &["UPDATE mid_yjqs_detail SET cjsl = 999999999999999.999 WHERE gddm = 'A0001'"],
        1,
        None,
    );
    check_variant("empty cursor", &["DELETE FROM mid_yjqs_detail"], 0, None);
}

#[test]
fn should_match_corpus_case_values_and_top1_tie_fallback_hashes() {
    let seed = "INSERT INTO par_fund_seat_para(fund_code, market_code, seat_code, stock_kind, \
        commision_fee_type, inure_begin_date, inure_end_date) \
        VALUES ('F001','001','X001','0501','4','20200101','20991231')";
    let fractions = "UPDATE mid_yjqs_detail SET yj12 = -3.255, yj06 = 1 WHERE gddm = 'A0001'";
    check_variant("top1 single mixed scale case", &[seed, fractions], 2, None);
    check_variant(
        "top1 equal ties",
        &[
            seed,
            fractions,
            "INSERT INTO par_fund_seat_para SELECT * FROM par_fund_seat_para",
        ],
        2,
        None,
    );
    check_variant("top1 differing ties", &[seed, fractions,
        "INSERT INTO par_fund_seat_para SELECT * REPLACE ('1' AS commision_fee_type) FROM par_fund_seat_para",
    ], 2, Some(SkipReason::AmbiguousTop1Tie));
    check_variant(
        "top1 null value",
        &[
            seed,
            "UPDATE par_fund_seat_para SET commision_fee_type = NULL",
            "UPDATE mid_yjqs_detail SET yj05 = 7.25 WHERE gddm = 'A0001'",
        ],
        2,
        None,
    );
}

#[test]
fn should_gain_end_to_end_on_scaled_purchase_corpus_including_all_guards() {
    let setup = |mut eng: Engine| {
        eng.step_budget = 20_000_000;
        for sql in [
            "CREATE TEMP TABLE corpus_template AS SELECT * FROM mid_yjqs_detail WHERE gddm = 'A0001'",
            "DELETE FROM mid_yjqs_detail",
            "INSERT INTO mid_yjqs_detail SELECT * EXCLUDE (i) REPLACE (CAST(100000 + i AS DECIMAL(16,0)) AS cjbh) \
             FROM corpus_template CROSS JOIN range(1000) AS numbers(i)",
        ] {
            eng.ex.execute(sql, &BindMap::new()).unwrap();
        }
        eng
    };
    let routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase";
    let args = [SpinValue::Str("20250715".into()), SpinValue::Null];
    let mut off = setup(engine());
    let start = std::time::Instant::now();
    let expected = off.call(routine, &args).unwrap();
    let baseline = start.elapsed();
    let expected_rows = rows(&off);
    assert_eq!(expected_rows.len(), 1000);
    assert_eq!(
        expected.outs.get("p_o_succeed"),
        Some(&SpinValue::Str("0".into()))
    );
    let hash = typed_hash(&off, &expected_rows);
    let (offline, offline_sql) = offline_engine();
    for (engine, candidate) in [(engine(), None), (offline, Some(offline_sql))] {
        let mut batch = setup(engine);
        batch.set_oriented = SetOrientedMode::BatchAtomic;
        let start = std::time::Instant::now();
        let actual = batch.call(routine, &args).unwrap();
        let optimized = start.elapsed();
        assert_eq!(actual.outs, expected.outs);
        assert_eq!(actual.return_value, expected.return_value);
        let actual_rows = rows(&batch);
        assert_eq!(actual_rows, expected_rows);
        assert_eq!(typed_hash(&batch, &actual_rows), hash);
        let status = &batch.set_oriented_reports()[0].status;
        let path = if candidate.is_some() {
            "offline"
        } else {
            "runtime"
        };
        match candidate {
            Some(sql) => assert_eq!(*status, Status::Converted { sql }),
            None => assert!(matches!(status, Status::Converted { .. }), "{status:?}"),
        }
        eprintln!("[corpus-perf] {path} 1000 rows: off={baseline:?} batch={optimized:?} speedup={:.2}x sha256={hash}",
            baseline.as_secs_f64() / optimized.as_secs_f64());
        assert!(
            optimized < baseline,
            "{path}: off={baseline:?} batch={optimized:?}"
        );
    }
}

fn typed_value(value: &SpinValue) -> serde_json::Value {
    use serde_json::json;
    match value {
        SpinValue::Null => json!(["null"]),
        SpinValue::Bool(v) => json!(["bool", v]),
        SpinValue::Int(v) => json!(["int", v.to_string()]),
        SpinValue::Decimal(v, precision, scale) => {
            json!(["decimal", v.to_string(), precision, scale])
        }
        SpinValue::Str(v) => json!(["str", v]),
        SpinValue::Blob(v) => json!(["blob", v]),
        SpinValue::Record(fields) => json!([
            "record",
            fields
                .iter()
                .map(|(name, v)| json!([name, typed_value(v)]))
                .collect::<Vec<_>>()
        ]),
    }
}

fn typed_hash(eng: &Engine, rows: &[Vec<SpinValue>]) -> String {
    let mut canonical: Vec<String> = rows
        .iter()
        .map(|row| serde_json::to_string(&row.iter().map(typed_value).collect::<Vec<_>>()).unwrap())
        .collect();
    canonical.sort();
    let binds = BindMap::from([(
        "$data".into(),
        SpinValue::Str(serde_json::to_string(&canonical).unwrap()),
    )]);
    let result = eng
        .ex
        .query("SELECT sha256($data)", &binds)
        .unwrap()
        .fetch_many(1);
    match result.as_slice() {
        [row] => match row.as_slice() {
            [SpinValue::Str(hash)] => hash.clone(),
            other => panic!("unexpected digest row: {other:?}"),
        },
        other => panic!("unexpected digest result: {other:?}"),
    }
}

#[test]
fn should_hash_typed_rows_independent_of_order_without_losing_duplicates() {
    let eng = engine();
    let values = vec![
        vec![SpinValue::Null, SpinValue::Str("NULL\n\"中".into())],
        vec![SpinValue::Int(1), SpinValue::Decimal(100, 38, 2)],
    ];
    let mut reversed = values.clone();
    reversed.reverse();
    assert_eq!(typed_hash(&eng, &values), typed_hash(&eng, &reversed));
    reversed.push(values[0].clone());
    assert_ne!(typed_hash(&eng, &values), typed_hash(&eng, &reversed));
    for different in [SpinValue::Str("1".into()), SpinValue::Decimal(1, 38, 0)] {
        let mut changed = values.clone();
        changed[1][0] = different;
        assert_ne!(typed_hash(&eng, &values), typed_hash(&eng, &changed));
    }
    let digest = eng
        .ex
        .query("SELECT sha256('abc')", &BindMap::new())
        .unwrap()
        .fetch_many(1);
    assert_eq!(
        digest,
        vec![vec![SpinValue::Str(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()
        )]]
    );
}

#[test]
fn should_convert_purchase_corpus_without_changing_typed_rows() {
    let routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase";
    let args = [SpinValue::Str("20250715".into()), SpinValue::Null];
    let mut off = engine();
    let original = off.call(routine, &args).unwrap();
    let expected = rows(&off);
    assert_eq!(expected.len(), 2);
    assert_eq!(expected[0][1], SpinValue::Str("0130000001".into()));
    assert_eq!(expected[1][1], SpinValue::Str("0130000002".into()));
    assert_eq!(
        original.outs.get("p_o_succeed"),
        Some(&SpinValue::Str("0".into()))
    );

    let mut batch = engine();
    batch.set_oriented = SetOrientedMode::BatchAtomic;
    let actual = batch.call(routine, &args).unwrap();
    assert_eq!(actual.outs, original.outs);
    assert_eq!(actual.return_value, original.return_value);
    assert_eq!(rows(&batch), expected);
    assert!(
        matches!(
            batch.set_oriented_reports()[0].status,
            Status::Converted { .. }
        ),
        "{:?}",
        batch.set_oriented_reports()
    );
}
