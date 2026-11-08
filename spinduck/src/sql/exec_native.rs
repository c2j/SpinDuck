//! DuckDB 原生执行器（feature = `native`）：SqlExecutor 接缝的真实实现。
//! 唯一允许触碰 duckdb crate 的模块（AGENTS.md §1）。

use crate::error::EngineError;
use crate::exec::{BindMap, ExecOutcome, Row, RowStream, SqlExecutor};
use crate::value::SpinValue;
use duckdb::types::{Value, ValueRef};
use duckdb::Connection;
use std::collections::HashMap;

/// DuckDB 连接封装（单连接 = 一个会话，§10.2）。
pub struct DuckExecutor {
    conn: Connection,
}

/// prepared 语句缓存容量：语料工作集（≥50 条不同语句）远大于内核默认 16，
/// LRU 淘汰会把热语句挤出缓存反噬吞吐（V24 计量：单工作集 ~10 条热语句）。
const PREPARED_CACHE_CAPACITY: usize = 512;

/// DDL 判定：CREATE/DROP/ALTER（含 TEMP TABLE OR REPLACE）不得进语句缓存——
/// 本版内核（1.5.5）缓存的 CREATE plan 二次执行会留下 0 列残表
/// （V24 守护测试 should_recreate_temp_table_across_prepared_cache）；
/// 且 DDL 替换了其他缓存语句引用的表，必须连带清空缓存迫使全部重绑定。
fn is_ddl(sql: &str) -> bool {
    let t = sql.trim_start();
    let u = &t[..t.len().min(6)].to_ascii_uppercase();
    u == "CREATE" || u.starts_with("DROP") || u.starts_with("ALTER")
}

fn kernel_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::kernel(10, format!("{e}"))
}

impl DuckExecutor {
    /// 内存库（开发/测试默认）。
    pub fn open_in_memory() -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory().map_err(kernel_err)?;
        conn.set_prepared_statement_cache_capacity(PREPARED_CACHE_CAPACITY);
        Ok(Self { conn })
    }
    /// 文件库（持久化运行态，§11.1 data/）。
    pub fn open(path: &str) -> Result<Self, EngineError> {
        let conn = Connection::open(path).map_err(kernel_err)?;
        conn.set_prepared_statement_cache_capacity(PREPARED_CACHE_CAPACITY);
        Ok(Self { conn })
    }
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// SpinValue → duckdb Value（NULL 以 Varchar 空类型承载，绑定语义等价）。
    fn to_duck(v: &SpinValue) -> Value {
        match v {
            SpinValue::Null => Value::Null,
            SpinValue::Bool(b) => Value::Boolean(*b),
            // 小整数按 INTEGER 绑定：DuckDB 对整型参数类型敏感
            // （如 round(DECIMAL, BIGINT) 无候选而 round(DECIMAL, INTEGER) 有）
            SpinValue::Int(i) if i32::try_from(*i).is_ok() => Value::Int(*i as i32),
            SpinValue::Int(i) => Value::BigInt(*i),
            SpinValue::Decimal(unscaled, precision, scale) => {
                // Decimal::new(width, scale, unscaled)（duckdb 1.10505 API）
                let width = (*precision).min(38);
                match duckdb::types::Decimal::new(width, *scale, *unscaled) {
                    Ok(d) => Value::Decimal(d),
                    Err(_) => Value::Double(*unscaled as f64 / 10f64.powi(*scale as i32)),
                }
            }
            SpinValue::Str(s) => Value::Text(s.clone()),
            SpinValue::Blob(b) => Value::Blob(b.clone()),
            SpinValue::Record(_) => {
                // RECORD 不可作为标量参数绑定（§3.3 coerce 规则）
                Value::Text("<record>".into())
            }
        }
    }

    /// duckdb ValueRef → SpinValue（结果 schema 顺序取回）。
    fn from_ref(vr: ValueRef<'_>) -> SpinValue {
        match vr {
            ValueRef::Null => SpinValue::Null,
            ValueRef::Boolean(b) => SpinValue::Bool(b),
            ValueRef::TinyInt(i) => SpinValue::Int(i as i64),
            ValueRef::SmallInt(i) => SpinValue::Int(i as i64),
            ValueRef::Int(i) => SpinValue::Int(i as i64),
            ValueRef::BigInt(i) => SpinValue::Int(i),
            ValueRef::HugeInt(i) => SpinValue::Int(i as i64),
            ValueRef::UTinyInt(i) => SpinValue::Int(i as i64),
            ValueRef::USmallInt(i) => SpinValue::Int(i as i64),
            ValueRef::UInt(i) => SpinValue::Int(i as i64),
            ValueRef::UBigInt(i) => SpinValue::Int(i as i64),
            ValueRef::Float(f) => SpinValue::Decimal((f * 1_000_000.0).round() as i128, 38, 6),
            ValueRef::Double(f) => SpinValue::Decimal((f * 1_000_000.0).round() as i128, 38, 6),
            ValueRef::Decimal(d) => SpinValue::Decimal(d.value(), 38, d.scale()), // width 不回传，按 38 承载
            ValueRef::Text(t) => SpinValue::Str(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => SpinValue::Blob(b.to_vec()),
            other => {
                // Timestamp/Date 等：按显示串落 Str（coerce 在赋值侧宽松转换）
                SpinValue::Str(format!("{other:?}"))
            }
        }
    }

    fn named_binds(binds: &BindMap) -> HashMap<String, Value> {
        binds
            .iter()
            .map(|(k, v)| (k.trim_start_matches('$').to_string(), Self::to_duck(v)))
            .collect()
    }
}

impl SqlExecutor for DuckExecutor {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(kernel_err)?;
        let named = Self::named_binds(binds);
        // duckdb 1.10505：列元数据在执行后才有效，故先 query 再取 schema
        let mut rows = stmt.query(&named).map_err(kernel_err)?;
        // 行元数据经 Rows 的 Deref 访问（borrow 与行迭代共存）；
        // Statement 已被消费/复位时 as_ref 为 None → KERNEL 域错误（不 panic）
        let st_ref = rows
            .as_ref()
            .ok_or_else(|| EngineError::kernel(10, "rows metadata unavailable"))?;
        let ncols = st_ref.column_count();
        let schema: Vec<String> = (0..ncols)
            .map(|i| {
                rows.as_ref()
                    .and_then(|st| st.column_name(i).ok().map(String::from))
                    .unwrap_or_default()
            })
            .collect();
        let mut out: Vec<Row> = Vec::new();
        while let Some(row) = rows.next().map_err(kernel_err)? {
            let mut r = Row::with_capacity(ncols);
            for i in 0..ncols {
                let vr = row.get_ref(i).map_err(kernel_err)?;
                r.push(Self::from_ref(vr));
            }
            out.push(r);
        }
        Ok(RowStream::new(schema, out))
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        if is_ddl(sql) {
            let mut stmt = self.conn.prepare(sql).map_err(kernel_err)?;
            let named = Self::named_binds(binds);
            let n = stmt.execute(&named).map_err(kernel_err)?;
            self.conn.flush_prepared_statement_cache();
            return Ok(ExecOutcome { rows_affected: n });
        }
        let mut stmt = self.conn.prepare_cached(sql).map_err(kernel_err)?;
        let named = Self::named_binds(binds);
        let n = stmt.execute(&named).map_err(kernel_err)?;
        Ok(ExecOutcome { rows_affected: n })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::BindMap;
    use std::collections::BTreeMap;

    #[test]
    fn should_roundtrip_select_with_named_binds() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        ex.execute(
            "CREATE TABLE t(a VARCHAR, b DECIMAL(15,2))",
            &BindMap::new(),
        )
        .unwrap();
        let mut b = BTreeMap::new();
        b.insert("$v_x".to_string(), SpinValue::Str("hello".into()));
        ex.execute("INSERT INTO t VALUES ($v_x, 12.34)", &b)
            .unwrap();
        let mut s = ex.query("SELECT a, b FROM t WHERE a = $v_x", &b).unwrap();
        let rows = s.fetch_many(2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], SpinValue::Str("hello".into()));
        assert_eq!(rows[0][1], SpinValue::Decimal(1234, 38, 2));
    }

    #[test]
    fn should_report_affected_rows_and_kernel_error_domain() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE t(a INT PRIMARY KEY)", &BindMap::new())
            .unwrap();
        let n = ex
            .execute("INSERT INTO t VALUES (1)", &BindMap::new())
            .unwrap();
        assert_eq!(n.rows_affected, 1);
        let err = ex
            .execute("SELECT * FROM missing_table", &BindMap::new())
            .unwrap_err();
        assert_eq!(err.domain, crate::error::FaultDomain::Kernel);
    }

    #[test]
    fn should_keep_session_usable_after_failing_statement() {
        // 回归守护（语料 WHEN OTHERS 记日志形态）：一条语句在内核侧失败后，
        // 同一会话必须可继续执行——曾持续报
        // "TransactionContext Error: Current transaction is aborted"，
        // 使处理器体内的 INSERT/SELECT 全部连锁失败。
        let ex = DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE t(a VARCHAR)", &BindMap::new())
            .unwrap();
        ex.execute("INSERT INTO t VALUES ('abc')", &BindMap::new())
            .unwrap();
        let err = match ex.query("SELECT CAST(a AS DECIMAL(38,6)) FROM t", &BindMap::new()) {
            Err(e) => e,
            Ok(_) => panic!("期望 'abc' → DECIMAL 转换失败"),
        };
        assert_eq!(err.domain, crate::error::FaultDomain::Kernel);
        let n = ex
            .execute("INSERT INTO t VALUES ('ok')", &BindMap::new())
            .unwrap_or_else(|e| panic!("失败语句后同会话不可用: {}", e.message));
        assert_eq!(n.rows_affected, 1);
        let mut s = ex
            .query("SELECT COUNT(*) FROM t", &BindMap::new())
            .unwrap_or_else(|e| panic!("失败语句后同会话不可查询: {}", e.message));
        assert_eq!(s.fetch_many(1)[0][0].as_str_loose(), "2");
    }

    #[test]
    fn should_rebind_prepared_shape_after_table_recreate() {
        // 守护测试（prepared 缓存前提）：同文本语句在表被 DROP/重建（列类型变化）
        // 后再次执行，必须按新 schema 重新绑定，而不是复用旧 plan 的列形状。
        let ex = DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE t(a BIGINT)", &BindMap::new())
            .unwrap();
        ex.execute("INSERT INTO t VALUES (42)", &BindMap::new())
            .unwrap();
        let mut s = ex.query("SELECT a FROM t", &BindMap::new()).unwrap();
        assert_eq!(s.fetch_many(1)[0][0], SpinValue::Int(42));
        ex.execute("DROP TABLE t", &BindMap::new()).unwrap();
        ex.execute("CREATE TABLE t(a VARCHAR)", &BindMap::new())
            .unwrap();
        ex.execute("INSERT INTO t VALUES ('xyz')", &BindMap::new())
            .unwrap();
        let mut s = ex
            .query("SELECT a FROM t", &BindMap::new())
            .unwrap_or_else(|e| panic!("表重建后同文本语句必须可重新绑定: {}", e.message));
        assert_eq!(s.fetch_many(1)[0][0], SpinValue::Str("xyz".into()));
    }

    #[test]
    fn should_recreate_temp_table_across_prepared_cache() {
        // 回归守护（V24）：缓存的 CREATE OR REPLACE TEMP TABLE plan 二次执行
        // 会留下 0 列残表（KERNEL-0010 Binder Error），sqlfn 物化路径
        // （materialize_fn_sites）必踩。DDL 绕过缓存 + DDL 后清空缓存。
        let ex = DuckExecutor::open_in_memory().unwrap();
        for round in 0..2 {
            ex.execute(
                "CREATE OR REPLACE TEMP TABLE sd_fn0 (sd_v VARCHAR, sd_a0 VARCHAR)",
                &BindMap::new(),
            )
            .unwrap_or_else(|e| panic!("round {round} create: {}", e.message));
            let ins = format!("INSERT INTO sd_fn0 VALUES ('v{round}','a{round}')");
            ex.execute(&ins, &BindMap::new())
                .unwrap_or_else(|e| panic!("round {round} insert: {}", e.message));
            let mut s = ex
                .query("SELECT count(*) FROM sd_fn0", &BindMap::new())
                .unwrap_or_else(|e| panic!("round {round} query: {}", e.message));
            assert_eq!(s.fetch_many(1)[0][0].as_str_loose(), "1");
        }
    }
}
