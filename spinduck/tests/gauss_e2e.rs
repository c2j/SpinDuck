//! M4.5 端到端：openGauss 容器实测（物化 → CALL 过程 → 回写 → 校验）。
//! 门控：feature gauss + 环境变量 SPINDUCK_OG_HOST（缺省跳过——跳过不等于通过）。
//!
//! 容器环境（自包含复现包，见 golden/README.md）：
//!   docker compose -f golden/gaussdb-docker/docker-compose.yml up -d
//!   SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
//!   SPINDUCK_OG_USER=gaussdb SPINDUCK_OG_PASSWORD=… \
//!   cargo test --features gauss --test gauss_e2e

#![cfg(feature = "gauss")]

use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, Row, RowStream, SqlExecutor};
use spinduck_core::io::{GaussConn, MaterializeMode};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::exec_native::DuckExecutor;
use spinduck_core::value::SpinValue;

fn og() -> Option<GaussConn> {
    let host = std::env::var("SPINDUCK_OG_HOST").ok()?;
    Some(
        GaussConn::connect_parts(
            &host,
            std::env::var("SPINDUCK_OG_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            &std::env::var("SPINDUCK_OG_USER").unwrap_or_else(|_| "gaussdb".into()),
            &std::env::var("SPINDUCK_OG_PASSWORD")
                .expect("SPINDUCK_OG_PASSWORD 必须显式提供（口令不入库）"),
            &std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".into()),
        )
        .expect("connect openGauss"),
    )
}

fn to_duck(v: &SpinValue) -> duckdb::types::Value {
    use duckdb::types::Value;
    match v {
        SpinValue::Null => Value::Null,
        SpinValue::Bool(b) => Value::Boolean(*b),
        SpinValue::Int(i) => Value::BigInt(*i),
        SpinValue::Decimal(u, _p, s) => match duckdb::types::Decimal::new(38, *s, *u) {
            Ok(d) => Value::Decimal(d),
            Err(_) => Value::Double(*u as f64 / 10f64.powi(*s as i32)),
        },
        SpinValue::Str(s) => Value::Text(s.clone()),
        SpinValue::Blob(b) => Value::Blob(b.clone()),
        SpinValue::Record(_) => Value::Text("<record>".into()),
    }
}

fn from_ref(vr: duckdb::types::ValueRef<'_>) -> SpinValue {
    use duckdb::types::ValueRef;
    match vr {
        ValueRef::Null => SpinValue::Null,
        ValueRef::Boolean(b) => SpinValue::Bool(b),
        ValueRef::TinyInt(i) => SpinValue::Int(i as i64),
        ValueRef::SmallInt(i) => SpinValue::Int(i as i64),
        ValueRef::Int(i) => SpinValue::Int(i as i64),
        ValueRef::BigInt(i) => SpinValue::Int(i),
        ValueRef::HugeInt(i) => SpinValue::Int(i as i64),
        ValueRef::UInt(i) => SpinValue::Int(i as i64),
        ValueRef::UBigInt(i) => SpinValue::Int(i as i64),
        ValueRef::Float(f) => SpinValue::Decimal((f * 1_000_000.0).round() as i128, 38, 6),
        ValueRef::Double(f) => SpinValue::Decimal((f * 1_000_000.0).round() as i128, 38, 6),
        ValueRef::Decimal(d) => SpinValue::Decimal(d.value(), 38, d.scale()),
        ValueRef::Text(t) => SpinValue::Str(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => SpinValue::Blob(b.to_vec()),
        other => SpinValue::Str(format!("{other:?}")),
    }
}

/// 第二句柄执行器：同一 DuckDB 库（物化/回写与 Engine 共享数据）。
struct HandleExec {
    conn: duckdb::Connection,
}

impl SqlExecutor for HandleExec {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| EngineError::kernel(10, format!("{e}")))?;
        let named: std::collections::HashMap<String, duckdb::types::Value> = binds
            .iter()
            .map(|(k, v)| (k.trim_start_matches('$').to_string(), to_duck(v)))
            .collect();
        let mut rows = stmt
            .query(&named)
            .map_err(|e| EngineError::kernel(10, format!("{e}")))?;
        let ncols = rows.as_ref().map(|s| s.column_count()).unwrap_or(0);
        let schema: Vec<String> = rows
            .as_ref()
            .map(|s| {
                (0..ncols)
                    .map(|i| s.column_name(i).map(|c| c.to_string()).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();
        let mut out: Vec<Row> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| EngineError::kernel(10, format!("{e}")))?
        {
            let mut r = Row::with_capacity(ncols);
            for i in 0..ncols {
                let vr = row
                    .get_ref(i)
                    .map_err(|e| EngineError::kernel(10, format!("{e}")))?;
                r.push(from_ref(vr));
            }
            out.push(r);
        }
        Ok(RowStream::new(schema, out))
    }
    fn execute(
        &self,
        sql: &str,
        binds: &BindMap,
    ) -> Result<spinduck_core::exec::ExecOutcome, EngineError> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| EngineError::kernel(10, format!("{e}")))?;
        let named: std::collections::HashMap<String, duckdb::types::Value> = binds
            .iter()
            .map(|(k, v)| (k.trim_start_matches('$').to_string(), to_duck(v)))
            .collect();
        let n = stmt
            .execute(&named)
            .map_err(|e| EngineError::kernel(10, format!("{e}")))?;
        Ok(spinduck_core::exec::ExecOutcome { rows_affected: n })
    }
}

#[test]
fn gauss_e2e_materialize_process_sync_back() {
    let Some(mut og) = og() else {
        // §4.1：黄金层禁止"跳过即绿"——缺容器必须让测试显式变红，
        // 否则 V13–V15 结论可被无容器环境静默"复验"
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };

    // 0. 源端准备（幂等）
    let _ = og.exec("DROP TABLE IF EXISTS sd_dat_in");
    let _ = og.exec("DROP TABLE IF EXISTS sd_dat_out");
    og.exec("CREATE TABLE sd_dat_in(a VARCHAR(20), n INT)")
        .unwrap();
    og.exec("INSERT INTO sd_dat_in VALUES ('x', 2)").unwrap();
    og.exec("INSERT INTO sd_dat_in VALUES ('y', 3)").unwrap();
    og.exec("CREATE TABLE sd_dat_out(a VARCHAR(20), total INT)")
        .unwrap();

    // 1. 本地 DuckDB：同一库双句柄（conn 被 move 进 Engine 执行器前克隆）
    let ex = DuckExecutor::open_in_memory().unwrap();
    let handle_conn = ex.connection().try_clone().unwrap();
    ex.execute("CREATE TABLE sd_dat_in(a VARCHAR, n INT)", &BindMap::new())
        .unwrap();
    ex.execute(
        "CREATE TABLE sd_dat_out(a VARCHAR, total INT)",
        &BindMap::new(),
    )
    .unwrap();

    // 1b. 物化：GaussDB → DuckDB（按源端全量）
    let mat = og
        .materialize(&ex, "sd_dat_in", None, std::env::temp_dir().as_path())
        .unwrap()
        .rows;
    assert_eq!(mat, 2, "全量物化两行");

    let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
    let handle_ex = HandleExec { conn: handle_conn };

    // 2. 注册过程（聚合输入表 → 写输出表），CALL
    eng.register(
        "CREATE OR REPLACE PROCEDURE prc_agg IS \
         BEGIN \
           INSERT INTO sd_dat_out SELECT 'sum', SUM(n) FROM sd_dat_in; \
         END;",
    )
    .unwrap();
    eng.call("prc_agg", &[]).unwrap();

    // 3. 回写：DuckDB → GaussDB（append）
    let cols = vec!["a".to_string(), "total".to_string()];
    let m = og.sync_append(&handle_ex, "sd_dat_out", &cols).unwrap();
    assert_eq!(m, 1);

    // 4. 校验：GaussDB 侧读回
    let (schema, rows) = og.fetch_rows("SELECT a, total FROM sd_dat_out").unwrap();
    assert_eq!(schema, vec!["a".to_string(), "total".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some("sum"));
    assert_eq!(rows[0][1].as_deref(), Some("5")); // 2+3

    eprintln!("M4.5 E2E OK: 物化 → 处理 → 回写 → 校验 通过");
}

#[test]
fn gauss_e2e_materialize_with_where_clause() {
    let Some(mut og) = og() else {
        // §4.1：黄金层禁止"跳过即绿"——缺容器必须让测试显式变红，
        // 否则 V13–V15 结论可被无容器环境静默"复验"
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };
    let _ = og.exec("DROP TABLE IF EXISTS sd_incr");
    og.exec("CREATE TABLE sd_incr(a VARCHAR(20), bcrq VARCHAR(8), n INT)")
        .unwrap();
    og.exec("INSERT INTO sd_incr VALUES ('x', '20240105', 1)")
        .unwrap();
    og.exec("INSERT INTO sd_incr VALUES ('y', '20240106', 2)")
        .unwrap();

    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(
        "CREATE TABLE sd_incr(a VARCHAR, bcrq VARCHAR, n INT)",
        &BindMap::new(),
    )
    .unwrap();
    let n = og
        .materialize(
            &ex,
            "sd_incr",
            Some("bcrq = '20240105'"),
            std::env::temp_dir().as_path(),
        )
        .unwrap()
        .rows;
    assert_eq!(n, 1, "按分区键增量物化");
    let mut s = ex
        .query("SELECT count(*) FROM sd_incr", &BindMap::new())
        .unwrap();
    assert_eq!(s.fetch_many(1).len(), 1);
}

/// 物化前置守卫（回归守护，V26.11）：本地 DuckDB 缺目标表时必须**在任何
/// GaussDB 拉取之前**显式红，且消息指路 deploy——`--where "1=0"` 这类零行
/// 路径过去会静默"成功"（INSERT 不执行，缺表不暴露），是本次缺陷的形态。
#[test]
fn should_fail_materialize_when_local_table_missing() {
    let Some(mut og) = og() else {
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };
    let _ = og.exec("DROP TABLE IF EXISTS sd_guard");
    og.exec("CREATE TABLE sd_guard(a VARCHAR(20), n INT)")
        .unwrap();
    og.exec("INSERT INTO sd_guard VALUES ('x', 1)").unwrap();

    // 本地库存在但**没有** sd_guard：守卫必须在拉取前红
    let ex = DuckExecutor::open_in_memory().unwrap();
    let err = og
        .materialize(
            &ex,
            "sd_guard",
            Some("n = 1"),
            std::env::temp_dir().as_path(),
        )
        .expect_err("本地缺表必须显式红");
    let text = err.to_string();
    assert!(text.starts_with("[ENV-0015]"), "{text}");
    assert!(text.contains("deploy"), "消息必须指路 deploy：{text}");

    // 零行路径同样红：守卫先于 fetch，`1=0` 不再有假绿
    let err0 = og
        .materialize(&ex, "sd_guard", Some("1=0"), std::env::temp_dir().as_path())
        .expect_err("零行路径也必须红（假绿禁止）");
    assert!(err0.to_string().starts_with("[ENV-0015]"), "{err0}");
}

/// PR-A 回归守护（V27）：COPY STREAM 快路径的单元格保真——NULL、空串、
/// 含逗号/引号/换行的字符串、DECIMAL 精度四类逐值往返（D14 硬约束：
/// 空串不得塌缩成 NULL）。同时锁定 outcome.mode = CopyStream 与行数。
#[test]
fn should_preserve_null_empty_and_special_cells_via_copystream() {
    let Some(mut og) = og() else {
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };
    let _ = og.exec("DROP TABLE IF EXISTS sd_fidelity");
    og.exec("CREATE TABLE sd_fidelity(id INT, v VARCHAR(50), note VARCHAR(80), amt NUMERIC(15,3))")
        .unwrap();
    og.exec(
        "INSERT INTO sd_fidelity VALUES \
         (1, NULL, 'x', 1.500), \
         (2, '', 'comma,quote\"' || chr(10) || 'next-line', 2.250), \
         (3, 'plain', NULL, NULL)",
    )
    .unwrap();

    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(
        "CREATE TABLE sd_fidelity(id INT, v VARCHAR, note VARCHAR, amt DECIMAL(15,3))",
        &BindMap::new(),
    )
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let out = og
        .materialize(&ex, "sd_fidelity", None, tmp.path())
        .expect("快路径物化");
    assert_eq!(
        out.mode,
        MaterializeMode::CopyStream,
        "默认应走 COPY STREAM"
    );
    assert_eq!(out.rows, 3);

    let mut s = ex
        .query(
            "SELECT id, v, note, amt FROM sd_fidelity ORDER BY id",
            &BindMap::new(),
        )
        .unwrap();
    let rows = s.fetch_many(10);
    assert_eq!(rows.len(), 3, "{rows:?}");
    // id=1：NULL 保真
    assert_eq!(rows[0][1], SpinValue::Null, "NULL 不得变空串");
    assert_eq!(
        rows[0][3],
        SpinValue::Decimal(1500, 38, 3),
        "DECIMAL(15,3) 类型化返回：1.500 = 未缩放 1500/scale 3"
    );
    assert_eq!(
        rows[1][3],
        SpinValue::Decimal(2250, 38, 3),
        "2.250 精度保真"
    );
    // id=2：openGauss A 兼容模式下内核层 '' ≡ NULL（datcompatibility=A，Oracle 语义），
    // 源端 CSV 即写 \N——空串保真（D14）只对 PG 类源端成立，A 模式无空串可保；
    // 此处锁定的是"不因传输层塌缩产生额外差异"（与 INSERT 路径同形）
    assert_eq!(
        rows[1][1],
        SpinValue::Null,
        "A 模式源端 '' 即 NULL，传输层不得改变该语义"
    );
    assert_eq!(
        rows[1][2],
        SpinValue::Str("comma,quote\"\nnext-line".into())
    );
    // id=3：NULL 数值
    assert_eq!(rows[2][2], SpinValue::Null);
    assert_eq!(rows[2][3], SpinValue::Null);
}

/// PR-A 降级守护（V27）：快路径失败（临时目录不可创建）必须自动回落
/// INSERT 路径且数据正确——降级可观测的 TraceSink 事件随 JSONL 契约评审
/// 单独落地，本用例锁定行为面。
#[test]
fn should_fall_back_to_insert_batch_when_fast_path_fails() {
    let Some(mut og) = og() else {
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };
    let _ = og.exec("DROP TABLE IF EXISTS sd_fallback");
    og.exec("CREATE TABLE sd_fallback(a VARCHAR(20), n INT)")
        .unwrap();
    og.exec("INSERT INTO sd_fallback VALUES ('x', 1), ('y', 2)")
        .unwrap();

    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(
        "CREATE TABLE sd_fallback(a VARCHAR, n INT)",
        &BindMap::new(),
    )
    .unwrap();
    // 不可创建的"目录"：一个已存在的普通文件
    let blocker = tempfile::tempdir().unwrap();
    let workdir = blocker.path().join("not-a-dir");
    std::fs::write(&workdir, b"file").unwrap();
    let out = og
        .materialize(&ex, "sd_fallback", None, &workdir)
        .expect("降级后应成功");
    assert_eq!(out.mode, MaterializeMode::InsertBatch, "必须回落 INSERT");
    assert_eq!(out.rows, 2);
    let mut s = ex
        .query("SELECT count(*) FROM sd_fallback", &BindMap::new())
        .unwrap();
    assert_eq!(s.fetch_many(1)[0][0], SpinValue::Int(2));
}

/// PR-A2 回归守护（V27 §3.6）：多表并行——结果按输入序、单表失败不中断
/// 其余表（快照批次语义）、同库多连接并发写（MVCC）、快路径默认生效。
#[test]
fn should_materialize_multiple_tables_in_parallel_with_failure_isolation() {
    let Some(_guard) = og() else {
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        );
    };
    let mut og = og().unwrap();
    for (t, rows) in [
        ("sd_mt_a", "INSERT INTO sd_mt_a VALUES ('x',1),('y',2)"),
        (
            "sd_mt_b",
            "INSERT INTO sd_mt_b VALUES ('p',10),('q',20),('r',30)",
        ),
    ] {
        og.exec(&format!("DROP TABLE IF EXISTS {t}")).unwrap();
        og.exec(&format!("CREATE TABLE {t}(a VARCHAR(20), n INT)"))
            .unwrap();
        og.exec(rows).unwrap();
    }

    // 同一落盘库 + 每表一个连接句柄（CLI 多表形态的同一装配方式）
    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("mt.duckdb");
    let ex = DuckExecutor::open(db_file.to_str().unwrap()).expect("落盘库");
    for t in ["sd_mt_a", "sd_mt_b"] {
        ex.execute(
            &format!("CREATE TABLE {t}(a VARCHAR, n INT)"),
            &BindMap::new(),
        )
        .unwrap();
    }
    let executors = (0..3)
        .map(|_| DuckExecutor::from_connection(ex.connection().try_clone().unwrap()))
        .collect::<Vec<_>>();

    let workdir = db_file.parent().unwrap().to_path_buf();
    let host = std::env::var("SPINDUCK_OG_HOST").unwrap();
    let port: u16 = std::env::var("SPINDUCK_OG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let password = std::env::var("SPINDUCK_OG_PASSWORD").unwrap();
    let outcomes = spinduck_core::io::materialize_parallel(
        2,
        vec![
            "sd_mt_a".to_string(),
            "sd_mt_b".to_string(),
            "sd_mt_c".to_string(), // 源端不存在：失败隔离样本
        ],
        &workdir,
        || {
            GaussConn::connect_parts(&host, port, "gaussdb", &password, "postgres")
                .map_err(|e| spinduck_core::error::EngineError::env(20, format!("connect: {e}")))
        },
        executors,
    );

    // 输入序 + 成功两张 + 缺表一张 ENV-0015（不中断其余）
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].0, "sd_mt_a");
    let ok_a = outcomes[0].1.as_ref().expect("sd_mt_a 应成功");
    assert_eq!(ok_a.rows, 2);
    assert_eq!(ok_a.mode, MaterializeMode::CopyStream);
    let ok_b = outcomes[1].1.as_ref().expect("sd_mt_b 应成功");
    assert_eq!(ok_b.rows, 3);
    let err_c = outcomes[2].1.as_ref().unwrap_err().to_string();
    assert!(err_c.starts_with("[ENV-0015]"), "{err_c}");
}
