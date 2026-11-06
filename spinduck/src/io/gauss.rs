//! GaussDB 连接器与物化/回写（§13，V1 数据源锁定 GaussDB）。
//!
//! 通道：PG 线协议（`postgres` crate，md5 认证；openGauss 容器需
//! `password_encryption_type=0` + hba md5 + 密码重存，见 validation.md）。
//! 类型保真：物化/回写均以文本值承载，类型转换由 DuckDB 列类型完成（v1）。

use crate::error::EngineError;
use crate::exec::BindMap;
use crate::value::SpinValue;
use postgres::{Client, NoTls};

/// 回写批大小（单次从 DuckDB 取的行数）。
const SYNC_BATCH: usize = 1000;

/// 结果集：列名顺序 + 每行文本值（NULL → None）。
pub type GaussRows = (Vec<String>, Vec<Vec<Option<String>>>);

/// GaussDB 连接（同步，无 TLS；v1 单连接）。
pub struct GaussConn {
    client: Client,
}

fn env_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::env(20, format!("gaussdb: {e}"))
}

/// 数据库错误保真呈现：SQLSTATE + 服务端消息（`postgres::Error` 的 Display 只有
/// "db error"，定位 DDL/DML 失败必须看 DbError 字段）。
fn db_err(e: postgres::Error) -> EngineError {
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
            if let postgres::SimpleQueryMessage::Row(row) = msg {
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

    /// 物化（§13.1）：GaussDB 表（可带 WHERE）→ 本地 DuckDB 表（INSERT 文本承载）。
    /// 要求 DuckDB 侧表已由 load 通道建好（类型一致，§13.1 类型保真）。
    pub fn materialize(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        where_clause: Option<&str>,
    ) -> Result<usize, EngineError> {
        let sql = match where_clause {
            Some(w) => format!("SELECT * FROM {table} WHERE {w}"),
            None => format!("SELECT * FROM {table}"),
        };
        let (schema, rows) = self.fetch_rows(&sql)?;
        let cols = schema.join(", ");
        let mut n = 0;
        for r in &rows {
            let vals: Vec<String> = r
                .iter()
                .map(|c| match c {
                    Some(s) => format!("'{}'", s.replace('\'', "''")),
                    None => "NULL".into(),
                })
                .collect();
            duck.execute(
                &format!("INSERT INTO {table} ({cols}) VALUES ({})", vals.join(", ")),
                &BindMap::new(),
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// 回写（§13.2）：DuckDB 表 → GaussDB 追加。返回回写行数。
    pub fn sync_append(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        cols: &[String],
    ) -> Result<usize, EngineError> {
        let collist = cols.join(", ");
        let mut s = duck.query(&format!("SELECT {collist} FROM {table}"), &BindMap::new())?;
        let mut n = 0;
        // 分批拉取直到耗尽（一次 fetch 有上限，>1 万行时必须循环）
        loop {
            let rows = s.fetch_many(SYNC_BATCH);
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                let vals: Vec<String> = r
                    .iter()
                    .map(|v| match v {
                        SpinValue::Null => "NULL".into(),
                        other => format!("'{}'", other.as_str_loose().replace('\'', "''")),
                    })
                    .collect();
                self.client
                    .execute(
                        &format!(
                            "INSERT INTO {table} ({collist}) VALUES ({})",
                            vals.join(", ")
                        ),
                        &[],
                    )
                    .map_err(db_err)?;
                n += 1;
            }
        }
        Ok(n)
    }
}
