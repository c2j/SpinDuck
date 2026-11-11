//! GaussDB 连接器与物化/回写（§13，V1 数据源锁定 GaussDB）。
//!
//! 通道：PG 线协议（`gaussdb::sync`，openGauss 私有口令认证 sha256/md5-sha256/SM3
//! 均支持；纯 Rust `postgres` 驱动不认 sha256，故 v1 曾要求容器开 md5，见
//! validation V24）。类型保真：物化/回写仍以文本值承载，类型转换由 DuckDB 列类型
//! 完成（v1 口径不变）。

use crate::error::EngineError;
use crate::exec::BindMap;
use crate::value::SpinValue;
use gaussdb::sync::{Client, NoTls};
use std::io::Write;

/// 回写批大小（单次从 DuckDB 取的行数）。
const SYNC_BATCH: usize = 1000;

/// 结果集：列名顺序 + 每行文本值（NULL → None）。
pub type GaussRows = (Vec<String>, Vec<Vec<Option<String>>>);

mod materialize;

pub use materialize::{materialize_parallel, MaterializeMode, MaterializeOutcome};

/// GaussDB 连接（同步，无 TLS；v1 单连接）。
pub struct GaussConn {
    client: Client,
}

fn env_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::env(20, format!("gaussdb: {e}"))
}

/// 数据库错误保真呈现：SQLSTATE + 服务端消息（`gaussdb::sync::Error` 的 Display 只有
/// "db error"，定位 DDL/DML 失败必须看 DbError 字段）。
fn db_err(e: gaussdb::sync::Error) -> EngineError {
    match e.as_db_error() {
        Some(d) => EngineError::env(
            20,
            format!(
                "gaussdb: {} {} ({})",
                d.code().code(),
                d.message(),
                d.severity()
            ),
        ),
        None => EngineError::env(20, format!("gaussdb: {e}")),
    }
}

impl GaussConn {
    /// 由 URL 连接（postgres://user:pass@host:port/db；密码需 URL 编码）。
    pub fn connect(url: &str) -> Result<Self, EngineError> {
        Ok(Self {
            client: Client::connect(url, NoTls).map_err(env_err)?,
        })
    }

    /// 由参数连接（密码免 URL 编码）。
    pub fn connect_parts(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        dbname: &str,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            client: Client::configure()
                .host(host)
                .port(port)
                .user(user)
                .password(password)
                .dbname(dbname)
                .connect(NoTls)
                .map_err(env_err)?,
        })
    }

    /// 执行 DDL/DML（不返回行）。
    pub fn exec(&mut self, sql: &str) -> Result<u64, EngineError> {
        self.client.execute(sql, &[]).map_err(db_err)
    }

    /// 执行脚本（多语句 / PL 块）：走简单查询协议，忽略结果集。
    ///
    /// 与 [`exec`](Self::exec) 的区别：简单查询不支持参数绑定，但可执行多语句
    /// 与 PL 块（`DO $$..$$`、`CREATE PROCEDURE .. AS $$..$$`），用于部署夹具。
    pub fn exec_script(&mut self, sql: &str) -> Result<(), EngineError> {
        self.client.simple_query(sql).map_err(db_err)?;
        Ok(())
    }

    /// 读取行（文本形态；NULL → None）。
    pub fn fetch_rows(&mut self, sql: &str) -> Result<GaussRows, EngineError> {
        let rows = self.client.simple_query(sql).map_err(db_err)?;
        let mut schema = Vec::new();
        let mut out = Vec::new();
        for msg in &rows {
            // `SimpleQueryMessage` 只在驱动层导出（sync 门面未列便捷别名），
            // 走 `sync::driver` 以免为枚举本身多引一条 crate 依赖。
            if let gaussdb::sync::driver::SimpleQueryMessage::Row(row) = msg {
                if schema.is_empty() {
                    schema = row.columns().iter().map(|c| c.name().to_string()).collect();
                }
                out.push(
                    (0..row.len())
                        .map(|i| row.get(i).map(|s| s.to_string()))
                        .collect(),
                );
            }
        }
        Ok((schema, out))
    }

    /// 物化前置守卫（V26.11）：本地 DuckDB 必须已有目标表——schema 所有权归
    /// `deploy`（计划 `ddl_tables` 唯一真相），物化只灌数据。守卫先于 GaussDB
    /// 拉取执行，`--where "1=0"` 的零行路径不再静默"成功"。
    ///
    /// 名字比较两侧 lower（DuckDB 标识符缺省小写，CLI 实参大小写不敏感）；
    /// 表名走绑定传参，不拼字符串。缺表 = 本地前置未就绪，归 `ENV-0015`。
    fn ensure_local_table(
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
    ) -> Result<(), EngineError> {
        let mut binds = BindMap::new();
        binds.insert("t".into(), SpinValue::Str(table.into()));
        let mut s = duck.query(
            "SELECT count(*) FROM duckdb_tables() WHERE lower(table_name) = lower($t)",
            &binds,
        )?;
        let n = match s.fetch_many(1).first() {
            Some(row) => match row.first() {
                Some(SpinValue::Int(c)) => *c,
                other => {
                    return Err(EngineError::env(
                        15,
                        format!("本地表存在性检查返回异常值 {other:?}（表 {table}）"),
                    ))
                }
            },
            None => {
                return Err(EngineError::env(
                    15,
                    format!("本地表存在性检查无结果（表 {table}）"),
                ))
            }
        };
        if n == 0 {
            return Err(EngineError::env(
                15,
                format!(
                    "本地表 {table} 不存在：materialize 只灌数据不建表，\
                 先执行 deploy <计划.toml> --db <本地库> 建好 schema 再物化"
                ),
            ));
        }
        Ok(())
    }

    /// 回写（§13.2）：DuckDB 表 → GaussDB 追加。返回回写行数。
    ///
    /// V26：逐行 execute 改为驱动 `copy_in`（COPY FROM STDIN，单事务原子追加；
    /// 98k 实测 1.2s，见 validation V26.4）。行数以 `finish()` 返回值为准，
    /// 与本地计数交叉校验。
    pub fn sync_append(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        cols: &[String],
    ) -> Result<usize, EngineError> {
        let collist = cols.join(", ");
        let mut s = duck.query(&format!("SELECT {collist} FROM {table}"), &BindMap::new())?;
        let mut n = 0usize;
        // 分批拉取直到耗尽（一次 fetch 有上限，>1 万行时必须循环）
        let mut writer = self
            .client
            .copy_in(&format!("COPY {table} ({collist}) FROM STDIN (FORMAT csv)"))
            .map_err(db_err)?;
        loop {
            let rows = s.fetch_many(SYNC_BATCH);
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                let line = r
                    .iter()
                    .map(|v| match v {
                        SpinValue::Null => csv_field(None),
                        other => {
                            let t = other.as_str_loose();
                            csv_field(Some(&t))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                writer
                    .write_all(line.as_bytes())
                    .and_then(|_| writer.write_all(b"\n"))
                    .map_err(|e| EngineError::env(20, format!("gaussdb copy: {e}")))?;
                n += 1;
            }
        }
        let copied = writer.finish().map_err(db_err)? as usize;
        if copied != n {
            return Err(EngineError::env(
                20,
                format!("gaussdb: COPY 行数不一致：服务端 {copied}，本地 {n} ({table})"),
            ));
        }
        Ok(n)
    }
}

/// CSV 字段编码（FORMAT csv）：NULL → 空字段；空串 → `""`（与 NULL 区分）；
/// 含 `,` `"` `\n` `\r` 的值加引号并在引号内翻倍。
fn csv_field(v: Option<&str>) -> String {
    match v {
        None => String::new(),
        Some("") => "\"\"".to_string(),
        Some(s) if s.contains([',', '"', '\n', '\r']) => format!("\"{}\"", s.replace('"', "\"\"")),
        Some(s) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ExecOutcome, RowStream, SqlExecutor};
    use std::cell::RefCell;

    /// 假执行器：记录收到的 SQL，对 duckdb_tables 探针回放配置的 count 值。
    /// 只服务 [`GaussConn::ensure_local_table`] 的行为断言（§4.1：单测不触真实内核）。
    struct FakeDuck {
        count: RefCell<i64>,
        seen_sql: RefCell<Vec<String>>,
        seen_t: RefCell<Option<String>>,
    }

    impl FakeDuck {
        fn with_count(n: i64) -> Self {
            Self {
                count: RefCell::new(n),
                seen_sql: RefCell::new(Vec::new()),
                seen_t: RefCell::new(None),
            }
        }
    }

    impl SqlExecutor for FakeDuck {
        fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
            self.seen_sql.borrow_mut().push(sql.into());
            *self.seen_t.borrow_mut() = binds.get("t").and_then(|v| match v {
                SpinValue::Str(s) => Some(s.clone()),
                _ => None,
            });
            Ok(RowStream::new(
                vec!["count_star()".into()],
                vec![vec![SpinValue::Int(*self.count.borrow())]],
            ))
        }

        fn execute(&self, _sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
            unreachable!("守卫只发查询")
        }
    }

    #[test]
    fn should_err_env_0015_with_deploy_hint_when_table_missing() {
        let duck = FakeDuck::with_count(0);
        let err = GaussConn::ensure_local_table(&duck, "mid_yjqs_detail").unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("[ENV-0015]"), "{text}");
        assert!(text.contains("deploy"), "必须指路 deploy：{text}");
    }

    #[test]
    fn should_pass_when_table_present() {
        let duck = FakeDuck::with_count(1);
        assert!(GaussConn::ensure_local_table(&duck, "mid_yjqs_detail").is_ok());
    }

    #[test]
    fn should_pass_case_insensitively_and_bind_table_name() {
        let duck = FakeDuck::with_count(1);
        assert!(GaussConn::ensure_local_table(&duck, "MID_YJQS_DETAIL").is_ok());
        // 表名必须经绑定传参（$t），不进 SQL 字面量
        assert!(
            !duck
                .seen_sql
                .borrow()
                .iter()
                .any(|s| s.contains("MID_YJQS")),
            "表名不得拼进 SQL 文本"
        );
        assert_eq!(
            duck.seen_t.borrow().as_deref(),
            Some("MID_YJQS_DETAIL"),
            "表名应经 $t 绑定传入"
        );
    }
}
