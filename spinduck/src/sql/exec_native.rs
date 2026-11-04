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

fn kernel_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::kernel(10, format!("{e}"))
}

impl DuckExecutor {
    /// 内存库（开发/测试默认）。
    pub fn open_in_memory() -> Result<Self, EngineError> {
        Ok(Self {
            conn: Connection::open_in_memory().map_err(kernel_err)?,
        })
    }
    /// 文件库（持久化运行态，§11.1 data/）。
    pub fn open(path: &str) -> Result<Self, EngineError> {
        Ok(Self {
            conn: Connection::open(path).map_err(kernel_err)?,
        })
    }
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// SpinValue → duckdb Value（NULL 以 Varchar 空类型承载，绑定语义等价）。
    fn to_duck(v: &SpinValue) -> Value {
        match v {
            SpinValue::Null => Value::Null,
            SpinValue::Bool(b) => Value::Boolean(*b),
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
        let mut stmt = self.conn.prepare(sql).map_err(kernel_err)?;
        let named = Self::named_binds(binds);
        // duckdb 1.10505：列元数据在执行后才有效，故先 query 再取 schema
        let mut rows = stmt.query(&named).map_err(kernel_err)?;
        // 行元数据经 Rows 的 Deref 访问（borrow 与行迭代共存）
        let ncols = rows.as_ref().unwrap().column_count();
        let schema: Vec<String> = (0..ncols)
            .map(|i| {
                rows.as_ref()
                    .unwrap()
                    .column_name(i)
                    .cloned()
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
        let mut stmt = self.conn.prepare(sql).map_err(kernel_err)?;
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
}
