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
use std::path::Path;

/// 回写批大小（单次从 DuckDB 取的行数）。
pub(crate) const SYNC_BATCH: usize = 1000;

/// 结果集：列名顺序 + 每行文本值（NULL → None）。
pub type GaussRows = (Vec<String>, Vec<Vec<Option<String>>>);

/// 物化实际生效的通道（观测与测试断言用；CLI stdout 契约不变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializeMode {
    /// COPY TO STDOUT CSV → DuckDB COPY FROM（V27 快路径，默认）。
    CopyStream,
    /// 500 行批量多值 INSERT（V26 路径；快路径失败自动回落）。
    InsertBatch,
}

/// 一次物化的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterializeOutcome {
    /// 物化行数（本地表行数差值，含回落路径）。
    pub rows: usize,
    /// 实际生效的通道。
    pub mode: MaterializeMode,
}

/// GaussDB 连接（同步，无 TLS；v1 单连接）。
pub struct GaussConn {
    pub(crate) client: Client,
}

fn env_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::env(20, format!("gaussdb: {e}"))
}

/// 数据库错误保真呈现：SQLSTATE + 服务端消息（`gaussdb::sync::Error` 的 Display 只有
/// "db error"，定位 DDL/DML 失败必须看 DbError 字段）。
pub(crate) fn db_err(e: gaussdb::sync::Error) -> EngineError {
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

    /// 物化（§13.1）：GaussDB 表（可带 WHERE）→ 本地 DuckDB 表。
    /// 要求 DuckDB 侧表已由 deploy 通道建好（类型一致，§13.1 类型保真）；
    /// 缺表在拉取前由 [`Self::ensure_local_table`] 显式红（V26.11，消灭零行假绿）。
    ///
    /// V27：默认走 COPY STREAM 快路径（D14：两侧显式 `NULL '\N'`，空串不塌缩），
    /// 任一失败自动回落 V26 的 500 行批量 INSERT（错误并入最终错误消息，不静默）。
    /// `workdir` 为临时 CSV 的落盘目录（成功即删，失败保留供诊断）。
    pub fn materialize(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        where_clause: Option<&str>,
        workdir: &Path,
    ) -> Result<MaterializeOutcome, EngineError> {
        Self::ensure_local_table(duck, table)?;
        let select_sql = match where_clause {
            Some(w) => format!("SELECT * FROM {table} WHERE {w}"),
            None => format!("SELECT * FROM {table}"),
        };
        match self.materialize_copystream(duck, table, &select_sql, workdir) {
            Ok(out) => Ok(out),
            Err(fast_err) => self
                .materialize_insert_batch(duck, table, &select_sql)
                .map_err(|insert_err| {
                    EngineError::env(
                        20,
                        format!("快路径失败（{fast_err}），INSERT 回落亦失败：{insert_err}"),
                    )
                })
                .map(|rows| MaterializeOutcome {
                    rows,
                    mode: MaterializeMode::InsertBatch,
                }),
        }
    }

    /// V27 快路径：COPY TO STDOUT CSV（流式落盘，不整表驻留内存）→
    /// DuckDB `COPY ... FROM`。任意失败交由调用方回落，本方法不吞错。
    fn materialize_copystream(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        select_sql: &str,
        workdir: &Path,
    ) -> Result<MaterializeOutcome, EngineError> {
        let before = Self::local_count(duck, table)?;
        // 列名经 prepare 取语句元数据（0 行查询在简单协议下无 Row 消息、
        // fetch_rows 的 schema 会是空的），COPY 的列清单按名对齐，
        // 与 INSERT 路径同一口径
        let cols = self.source_columns(select_sql)?.join(", ");
        std::fs::create_dir_all(workdir).map_err(|e| {
            EngineError::env(20, format!("物化临时目录 {}: {e}", workdir.display()))
        })?;
        let csv = workdir.join(format!(".spinduck-mat-{table}-{}.csv", std::process::id()));
        self.copy_to_csv(select_sql, &csv)?;
        let load = format!(
            "COPY {table} ({cols}) FROM '{}' (FORMAT CSV, HEADER true, \
             DELIMITER ',', QUOTE '\"', ESCAPE '\"', NULLSTR '\\N')",
            csv.display().to_string().replace('\'', "''")
        );
        let result = duck.execute(&load, &BindMap::new());
        if let Err(e) = result {
            // 失败保留 CSV 供诊断（设计文档 §3.1），错误带路径
            return Err(EngineError::env(
                20,
                format!("DuckDB COPY FROM {} 失败（CSV 已保留）：{e}", csv.display()),
            ));
        }
        let rows = (Self::local_count(duck, table)? - before) as usize;
        std::fs::remove_file(&csv)
            .map_err(|e| EngineError::env(20, format!("清理临时 CSV {}: {e}", csv.display())))?;
        Ok(MaterializeOutcome {
            rows,
            mode: MaterializeMode::CopyStream,
        })
    }

    /// V26 批量 INSERT 路径（快路径回落；500 行/语句批量多值 INSERT，
    /// 字面量形态与逐行逐字一致，类型转换仍由 DuckDB 列类型完成）。
    fn materialize_insert_batch(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        select_sql: &str,
    ) -> Result<usize, EngineError> {
        let (schema, rows) = self.fetch_rows(select_sql)?;
        let cols = schema.join(", ");
        let mut n = 0;
        const MAT_BATCH: usize = 500;
        for chunk in rows.chunks(MAT_BATCH) {
            let mut stmt = format!("INSERT INTO {table} ({cols}) VALUES ");
            for (i, r) in chunk.iter().enumerate() {
                if i > 0 {
                    stmt.push(',');
                }
                stmt.push('(');
                let vals: Vec<String> = r
                    .iter()
                    .map(|c| match c {
                        Some(s) => format!("'{}'", s.replace('\'', "''")),
                        None => "NULL".into(),
                    })
                    .collect();
                stmt.push_str(&vals.join(", "));
                stmt.push(')');
            }
            duck.execute(&stmt, &BindMap::new())?;
            n += chunk.len();
        }
        Ok(n)
    }

    /// 本地表当前行数（物化行数差值统计的基准）。
    fn local_count(duck: &dyn crate::exec::SqlExecutor, table: &str) -> Result<i64, EngineError> {
        let mut s = duck.query(&format!("SELECT count(*) FROM {table}"), &BindMap::new())?;
        match s.fetch_many(1).first().and_then(|r| r.first()) {
            Some(SpinValue::Int(c)) => Ok(*c),
            other => Err(EngineError::env(
                15,
                format!("本地行数检查返回异常值 {other:?}（表 {table}）"),
            )),
        }
    }

    /// COPY TO STDOUT（D14：显式 `NULL '\N'`——空串保持 `""`，NULL 保持 `\N`，
    /// 两侧同一标记，空串不得塌缩成 NULL）流式写入 `csv_path`。
    fn copy_to_csv(&mut self, select_sql: &str, csv_path: &Path) -> Result<(), EngineError> {
        let copy_sql =
            format!("COPY ({select_sql}) TO STDOUT WITH CSV HEADER DELIMITER ',' NULL '\\N'");
        let mut reader = self.client.copy_out(&copy_sql).map_err(db_err)?;
        let file = std::fs::File::create(csv_path)
            .map_err(|e| EngineError::env(20, format!("创建 {}: {e}", csv_path.display())))?;
        let mut writer = std::io::BufWriter::new(file);
        std::io::copy(&mut reader, &mut writer)
            .map_err(|e| EngineError::env(20, format!("gaussdb copy_out: {e}")))?;
        writer
            .flush()
            .map_err(|e| EngineError::env(20, format!("gaussdb copy_out flush: {e}")))?;
        Ok(())
    }

    /// 源端列名（按 SELECT 输出序）：prepare 的语句元数据在零行时同样完整。
    fn source_columns(&mut self, select_sql: &str) -> Result<Vec<String>, EngineError> {
        let stmt = self.client.prepare(select_sql).map_err(db_err)?;
        Ok(stmt
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect())
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

/// 多表并行物化（V27 §3.6）：`jobs` 个工作线程消费表队列，每线程经
/// `open_source` 取**独立** GaussDB 连接（`copy_out` 需 `&mut client`），
/// 并持有传入的第 i 个本地执行器（同库多连接，MVCC 处理并发写不同表）。
///
/// 单表失败不中断其余表（快照批次语义）；结果按输入表序返回，失败以
/// `Err` 体现，由调用方决定呈现与退出码。表数少于 `jobs` 时按表数收敛。
/// 线程聚合的逐表结果：(输入序, 表名, 物化结果/错误)。
type TableResults = Vec<(usize, String, Result<MaterializeOutcome, EngineError>)>;

pub fn materialize_parallel<T, F>(
    jobs: usize,
    tables: Vec<String>,
    workdir: &Path,
    open_source: F,
    duck_executors: Vec<T>,
) -> Vec<(String, Result<MaterializeOutcome, EngineError>)>
where
    T: crate::exec::SqlExecutor + Send + 'static,
    F: Fn() -> Result<GaussConn, EngineError> + Sync,
{
    debug_assert_eq!(tables.len(), duck_executors.len(), "表与执行器须一一对应");
    let jobs = jobs.max(1).min(tables.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    // 执行器按索引所有权移交（take）：线程只触碰自己的 T，约束收敛到 Send 即可
    let pool: std::sync::Mutex<Vec<Option<T>>> =
        std::sync::Mutex::new(duck_executors.into_iter().map(Some).collect());
    let results: std::sync::Mutex<TableResults> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= tables.len() {
                    break;
                }
                let Some(duck) = pool.lock().unwrap_or_else(|e| e.into_inner())[i].take() else {
                    break;
                };
                let res = open_source()
                    .and_then(|mut og| og.materialize(&duck, &tables[i], None, workdir));
                // 锁中毒只在持锁线程 panic 时发生——结果聚合继续（该线程的结果已丢，
                // 由对应表的缺失暴露），不放大成整体失败
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((i, tables[i].clone(), res));
            });
        }
    });
    let mut out = results
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect::<Vec<_>>();
    out.sort_by_key(|(i, _, _)| *i);
    out.into_iter()
        .map(|(_, table, res)| (table, res))
        .collect()
}

/// CSV 字段编码（FORMAT csv）：NULL → 空字段；空串 → `""`（与 NULL 区分）；
/// 含 `,` `"` `\n` `\r` 的值加引号并在引号内翻倍。
pub(crate) fn csv_field(v: Option<&str>) -> String {
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
    /// 只服务 [`ensure_local_table`] 的行为断言（§4.1：单测不触真实内核）。
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
