//! DuckDB 原生执行器（feature = `native`）：SqlExecutor 接缝的真实实现。
//! 唯一允许触碰 duckdb crate 的模块（AGENTS.md §1）。

use crate::error::EngineError;
use crate::exec::{is_ddl, BindMap, ExecOutcome, Row, RowStream, SqlExecutor};
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

    /// 由既有连接构造（V27 §3.6 多表并行：同库多连接句柄分发给工作线程）。
    pub fn from_connection(conn: Connection) -> Self {
        conn.set_prepared_statement_cache_capacity(PREPARED_CACHE_CAPACITY);
        Self { conn }
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
            // DDL 本体绕过语句缓存（契约①的根源：缓存的 CREATE plan 二次执行
            // 产生 0 列残表）；被替换表的其他缓存语句交由内核 execute 期
            // rebind 兜底（V24 评审决断 1：不做全缓存 flush——sqlfn 物化路径
            // 每行 CREATE 会把 512 容量反复清空，互斥计量不可接受）。
            let mut stmt = self.conn.prepare(sql).map_err(kernel_err)?;
            let named = Self::named_binds(binds);
            let n = stmt.execute(&named).map_err(kernel_err)?;
            return Ok(ExecOutcome { rows_affected: n });
        }
        let mut stmt = self.conn.prepare_cached(sql).map_err(kernel_err)?;
        let named = Self::named_binds(binds);
        let n = stmt.execute(&named).map_err(kernel_err)?;
        Ok(ExecOutcome { rows_affected: n })
    }
}

#[cfg(test)]
mod appender_probes {
    use super::*;

    fn execute(ex: &DuckExecutor, sql: &str) -> usize {
        ex.execute(sql, &BindMap::new()).unwrap().rows_affected
    }

    fn rows(ex: &DuckExecutor, sql: &str) -> Vec<Row> {
        ex.query(sql, &BindMap::new()).unwrap().fetch_many(10)
    }

    fn constraint_outcome(
        definition: &str,
        values: [Option<i32>; 2],
        staged: bool,
    ) -> (String, String) {
        let ex = DuckExecutor::open_in_memory().unwrap();
        execute(&ex, "CREATE TABLE parent(id INTEGER PRIMARY KEY)");
        execute(&ex, "INSERT INTO parent VALUES (1), (10), (20), (30)");
        execute(&ex, &format!("CREATE TABLE t({definition})"));
        execute(
            &ex,
            "CREATE TEMP TABLE stage AS SELECT id FROM t WHERE FALSE",
        );
        execute(&ex, "INSERT INTO t VALUES (1)");
        execute(&ex, "BEGIN TRANSACTION");
        execute(&ex, "INSERT INTO t VALUES (10), (20)");
        let sql = if staged {
            let mut app = ex.conn.appender("stage").unwrap();
            for value in values {
                app.append_row([value]).unwrap();
            }
            app.flush().unwrap();
            "INSERT INTO t SELECT id FROM stage".to_string()
        } else {
            let tuples: Vec<_> = values
                .iter()
                .map(|v| match v {
                    Some(n) => format!("({n})"),
                    None => "(NULL)".to_string(),
                })
                .collect();
            format!("INSERT INTO t VALUES {}", tuples.join(","))
        };
        let err = ex.execute(&sql, &BindMap::new()).unwrap_err();
        assert_eq!(err.code.0, "KERNEL-0010");
        let replay = ex
            .execute("INSERT INTO t VALUES (30)", &BindMap::new())
            .unwrap_err();
        assert_eq!(replay.code.0, "KERNEL-0010");
        assert!(
            replay.message.contains("transaction is aborted"),
            "{}",
            replay.message
        );
        execute(&ex, "ROLLBACK");
        assert_eq!(
            rows(&ex, "SELECT id FROM t ORDER BY id"),
            vec![vec![SpinValue::Int(1)]]
        );
        assert_eq!(rows(&ex, "SELECT id FROM stage"), Vec::<Row>::new());
        (err.message, replay.message)
    }

    #[test]
    fn should_match_sql_constraint_failure_and_rollback_through_staging() {
        for (definition, values, message) in [
            (
                "id INTEGER PRIMARY KEY",
                [Some(30), Some(1)],
                "Duplicate key",
            ),
            (
                "id INTEGER NOT NULL",
                [Some(30), None],
                "NOT NULL constraint failed",
            ),
            (
                "id INTEGER CHECK(id > 0)",
                [Some(30), Some(-1)],
                "CHECK constraint failed",
            ),
            (
                "id INTEGER REFERENCES parent(id)",
                [Some(30), Some(99)],
                "foreign key constraint",
            ),
        ] {
            let direct = constraint_outcome(definition, values, false);
            let staged = constraint_outcome(definition, values, true);
            assert!(direct.0.contains(message), "{}", direct.0);
            assert_eq!(direct, staged, "{definition}");
        }
    }

    #[test]
    fn should_match_staged_column_defaults_on_commit_and_rollback() {
        for finish in ["COMMIT", "ROLLBACK"] {
            for staged in [false, true] {
                let ex = DuckExecutor::open_in_memory().unwrap();
                let binds = BindMap::new();
                ex.execute("CREATE SEQUENCE ids START 100", &binds).unwrap();
                ex.execute("CREATE TABLE t(id BIGINT DEFAULT nextval('ids'), amount DECIMAL(18,2), label VARCHAR DEFAULT 'default', flag BOOLEAN DEFAULT TRUE)", &binds).unwrap();
                ex.execute("BEGIN TRANSACTION", &binds).unwrap();
                let sql = if staged {
                    ex.execute(
                        "CREATE TEMP TABLE stage AS SELECT amount, label FROM t WHERE FALSE",
                        &binds,
                    )
                    .unwrap();
                    let mut app = ex.conn.appender("stage").unwrap();
                    for (amount, label) in [
                        (12345, Value::Null),
                        (-12345, Value::Text("O'Reilly".into())),
                    ] {
                        let values = [
                            DuckExecutor::to_duck(&SpinValue::Decimal(amount, 18, 3)),
                            label,
                        ];
                        app.append_row(duckdb::appender_params_from_iter(values))
                            .unwrap();
                    }
                    app.flush().unwrap();
                    "INSERT INTO t(amount, label) SELECT amount, label FROM stage"
                } else {
                    "INSERT INTO t(amount, label) VALUES (12.345, NULL), (-12.345, 'O''Reilly')"
                };
                assert_eq!(ex.execute(sql, &binds).unwrap().rows_affected, 2);
                let expected = vec![
                    vec![
                        SpinValue::Int(100),
                        SpinValue::Decimal(1235, 38, 2),
                        SpinValue::Null,
                        SpinValue::Bool(true),
                    ],
                    vec![
                        SpinValue::Int(101),
                        SpinValue::Decimal(-1235, 38, 2),
                        SpinValue::Str("O'Reilly".into()),
                        SpinValue::Bool(true),
                    ],
                ];
                assert_eq!(rows(&ex, "SELECT * FROM t ORDER BY id"), expected);
                execute(&ex, finish);
                let after = rows(&ex, "SELECT * FROM t ORDER BY id");
                assert_eq!(after, if finish == "COMMIT" { expected } else { vec![] });
                assert_eq!(
                    rows(&ex, "SELECT currval('ids')"),
                    vec![vec![SpinValue::Int(101)]]
                );
            }
        }
    }

    #[test]
    fn should_expose_changed_transaction_state_on_staged_conversion_error() {
        let mut states = Vec::new();
        for staged in [false, true] {
            let ex = DuckExecutor::open_in_memory().unwrap();
            let binds = BindMap::new();
            ex.execute("CREATE TABLE t(id INTEGER)", &binds).unwrap();
            ex.execute(
                "CREATE TEMP TABLE stage AS SELECT id FROM t WHERE FALSE",
                &binds,
            )
            .unwrap();
            ex.execute("INSERT INTO t VALUES (1)", &binds).unwrap();
            ex.execute("BEGIN TRANSACTION", &binds).unwrap();
            ex.execute("INSERT INTO t VALUES (10)", &binds).unwrap();
            let err = if staged {
                let mut app = ex.conn.appender("stage").unwrap();
                app.append_row(["20"]).unwrap();
                kernel_err(app.append_row(["invalid"]).unwrap_err())
            } else {
                ex.execute("INSERT INTO t VALUES ('20'), ('invalid')", &binds)
                    .unwrap_err()
            };
            assert_eq!(err.code.0, "KERNEL-0010");
            assert!(err.message.contains("Could not convert"), "{}", err.message);
            states.push(
                ex.execute("INSERT INTO t VALUES (30)", &binds)
                    .map(|outcome| outcome.rows_affected)
                    .map_err(|e| (e.code.0, e.message)),
            );
            ex.execute("ROLLBACK", &binds).unwrap();
            assert_eq!(
                ex.query("SELECT id FROM t", &binds).unwrap().fetch_many(10),
                vec![vec![SpinValue::Int(1)]]
            );
        }
        assert_eq!(
            states[0],
            Err((
                "KERNEL-0010".to_string(),
                "TransactionContext Error: Current transaction is aborted (please ROLLBACK)"
                    .to_string()
            ))
        );
        assert_eq!(states[1], Ok(1));
    }

    #[test]
    fn should_defer_source_typed_staging_conversion_failure_to_target_sql() {
        let mut failures = Vec::new();
        for staged in [false, true] {
            let ex = DuckExecutor::open_in_memory().unwrap();
            execute(&ex, "CREATE TABLE t(id INTEGER)");
            execute(&ex, "CREATE TEMP TABLE stage(id VARCHAR)");
            execute(&ex, "INSERT INTO t VALUES (1)");
            execute(&ex, "BEGIN TRANSACTION");
            execute(&ex, "INSERT INTO t VALUES (10)");
            let sql = if staged {
                let mut app = ex.conn.appender("stage").unwrap();
                app.append_rows([["20"], ["invalid"]]).unwrap();
                app.flush().unwrap();
                "INSERT INTO t SELECT id FROM stage"
            } else {
                "INSERT INTO t VALUES ('20'), ('invalid')"
            };
            let err = ex.execute(sql, &BindMap::new()).unwrap_err();
            assert_eq!(err.code.0, "KERNEL-0010");
            assert!(
                err.message
                    .contains("Could not convert string 'invalid' to INT32"),
                "{}",
                err.message
            );
            let replay = ex
                .execute("INSERT INTO t VALUES (30)", &BindMap::new())
                .unwrap_err();
            assert_eq!(replay.code.0, "KERNEL-0010");
            assert!(
                replay.message.contains("transaction is aborted"),
                "{}",
                replay.message
            );
            failures.push(replay.message);
            execute(&ex, "ROLLBACK");
            assert_eq!(rows(&ex, "SELECT id FROM t"), vec![vec![SpinValue::Int(1)]]);
            assert_eq!(rows(&ex, "SELECT id FROM stage"), Vec::<Row>::new());
        }
        assert_eq!(failures[0], failures[1]);
    }

    #[test]
    fn should_preserve_sequence_consumption_on_staged_conversion_failure() {
        let mut next_ids = Vec::new();
        for staged in [false, true] {
            let ex = DuckExecutor::open_in_memory().unwrap();
            execute(&ex, "CREATE SEQUENCE ids START 100");
            execute(
                &ex,
                "CREATE TABLE t(id BIGINT DEFAULT nextval('ids'), amount INTEGER)",
            );
            execute(&ex, "CREATE TEMP TABLE stage(amount VARCHAR)");
            execute(&ex, "BEGIN TRANSACTION");
            let sql = if staged {
                let mut app = ex.conn.appender("stage").unwrap();
                app.append_rows([["20"], ["invalid"]]).unwrap();
                app.flush().unwrap();
                "INSERT INTO t(amount) SELECT amount FROM stage"
            } else {
                "INSERT INTO t(amount) VALUES ('20'), ('invalid')"
            };
            let err = ex.execute(sql, &BindMap::new()).unwrap_err();
            assert_eq!(err.code.0, "KERNEL-0010");
            assert!(
                err.message
                    .contains("Could not convert string 'invalid' to INT32"),
                "{}",
                err.message
            );
            execute(&ex, "ROLLBACK");
            assert_eq!(rows(&ex, "SELECT * FROM t"), Vec::<Row>::new());
            next_ids.push(rows(&ex, "SELECT nextval('ids')"));
        }
        assert_eq!(next_ids, vec![vec![vec![SpinValue::Int(100)]]; 2]);
    }

    #[test]
    fn should_expose_literal_string_error_bypass_with_source_typed_staging() {
        let mut outcomes = Vec::new();
        for staged in [false, true] {
            let ex = DuckExecutor::open_in_memory().unwrap();
            execute(&ex, "CREATE TABLE t(label VARCHAR)");
            execute(&ex, "CREATE TEMP TABLE stage(label VARCHAR)");
            execute(&ex, "BEGIN TRANSACTION");
            let sql = if staged {
                let mut app = ex.conn.appender("stage").unwrap();
                app.append_row(["a\0b"]).unwrap();
                app.flush().unwrap();
                "INSERT INTO t SELECT label FROM stage"
            } else {
                "INSERT INTO t VALUES ('a\0b')"
            };
            outcomes.push(
                ex.execute(sql, &BindMap::new())
                    .map(|outcome| outcome.rows_affected)
                    .map_err(|e| (e.code.0, e.message)),
            );
            let expected = if staged {
                vec![vec![SpinValue::Str("a\0b".into())]]
            } else {
                vec![]
            };
            assert_eq!(rows(&ex, "SELECT * FROM t"), expected);
            execute(&ex, "ROLLBACK");
            assert_eq!(rows(&ex, "SELECT * FROM t"), Vec::<Row>::new());
        }
        assert_eq!(
            outcomes[0],
            Err((
                "KERNEL-0010".to_string(),
                "nul byte found in provided data at position: 24".to_string()
            ))
        );
        assert_eq!(outcomes[1], Ok(1));
    }

    #[test]
    fn should_reject_savepoint_for_appender_block_recovery() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let binds = BindMap::new();
        ex.execute("BEGIN TRANSACTION", &binds).unwrap();
        let err = ex.execute("SAVEPOINT append_block", &binds).unwrap_err();
        assert_eq!(err.code.0, "KERNEL-0010");
        assert!(err.message.contains("syntax error"), "{}", err.message);
        assert!(err.message.contains("SAVEPOINT"), "{}", err.message);
        ex.execute("ROLLBACK", &binds).unwrap();
    }

    #[test]
    fn should_require_full_rollback_after_appender_constraint_failure() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let binds = BindMap::new();
        ex.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", &binds)
            .unwrap();
        ex.execute("INSERT INTO t VALUES (1)", &binds).unwrap();
        ex.execute("BEGIN TRANSACTION", &binds).unwrap();
        ex.execute("INSERT INTO t VALUES (10)", &binds).unwrap();
        {
            let mut appender = ex.conn.appender("t").unwrap();
            appender.append_row([20]).unwrap();
            appender.flush().unwrap();
        }
        assert_eq!(
            ex.query("SELECT id FROM t ORDER BY id", &binds)
                .unwrap()
                .fetch_many(10),
            vec![
                vec![SpinValue::Int(1)],
                vec![SpinValue::Int(10)],
                vec![SpinValue::Int(20)]
            ]
        );
        {
            let mut appender = ex.conn.appender("t").unwrap();
            appender.append_rows([[30], [1]]).unwrap();
            let err = kernel_err(appender.flush().unwrap_err());
            assert_eq!(err.code.0, "KERNEL-0010");
            assert!(err.message.contains("Duplicate key"), "{}", err.message);
        }
        let replay_error = ex.execute("INSERT INTO t VALUES (30)", &binds).unwrap_err();
        assert_eq!(replay_error.code.0, "KERNEL-0010");
        assert!(
            replay_error.message.contains("transaction is aborted"),
            "{}",
            replay_error.message
        );
        ex.execute("ROLLBACK", &binds).unwrap();
        assert_eq!(
            ex.query("SELECT id FROM t ORDER BY id", &binds)
                .unwrap()
                .fetch_many(10),
            vec![vec![SpinValue::Int(1)]]
        );
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
}
