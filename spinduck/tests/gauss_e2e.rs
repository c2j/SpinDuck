//! M4.5 端到端：openGauss 容器实测（物化 → CALL 过程 → 回写 → 校验）。
//! 门控：feature gauss + 环境变量 SPINDUCK_OG_HOST（缺省跳过）。
//!
//! 容器与认证准备（validation.md V6 补充）：
//!   docker run -d --name spinduck-og --privileged \
//!     -e GS_PASSWORD=Enmo@1234 -p 15432:5432 opengauss/opengauss:5.0.0
//!   # password_encryption_type=0 + hba md5 + 密码重存（PG 协议 md5 兼容）
//!   SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=15432 \
//!   SPINDUCK_OG_USER=gaussdb SPINDUCK_OG_PASSWORD=Enmo@5678 \
//!   cargo test --features gauss gauss_e2e

use spinduck_core::engine::Engine;
use spinduck_core::error::EngineError;
use spinduck_core::exec::{BindMap, Row, RowStream, SqlExecutor};
use spinduck_core::io::GaussConn;
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
            &std::env::var("SPINDUCK_OG_PASSWORD").unwrap_or_else(|_| "Enmo@5678".into()),
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
#[cfg(feature = "gauss")]
fn gauss_e2e_materialize_process_sync_back() {
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
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
    let mat = og.materialize(&ex, "sd_dat_in", None).unwrap();
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
#[cfg(feature = "gauss")]
fn gauss_e2e_materialize_with_where_clause() {
    let Some(mut og) = og() else {
        eprintln!("SKIP: SPINDUCK_OG_HOST not set");
        return;
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
        .materialize(&ex, "sd_incr", Some("bcrq = '20240105'"))
        .unwrap();
    assert_eq!(n, 1, "按分区键增量物化");
    let mut s = ex
        .query("SELECT count(*) FROM sd_incr", &BindMap::new())
        .unwrap();
    assert_eq!(s.fetch_many(1).len(), 1);
}
