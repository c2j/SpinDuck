//! 物化通道（§13.1，V27 COPY STREAM 快路径 + V26 INSERT 回落 + 多表并行）。

use super::{db_err, GaussConn};
use crate::error::EngineError;
use crate::exec::BindMap;
use crate::value::SpinValue;
use std::io::Read;
use std::io::Write;
use std::path::Path;

/// 物化实际生效的通道（观测与测试断言用；CLI stdout 契约不变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializeMode {
    /// COPY TO STDOUT CSV → DuckDB COPY FROM（V27 快路径，默认）。
    CopyStream,
    /// 500 行批量多值 INSERT（V26 路径；快路径失败自动回落）。
    InsertBatch,
}

/// 一次物化的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeOutcome {
    /// 物化行数：快路径为本地表行数差值（且与 CSV 数据行数核对一致）；
    /// 回落路径为 INSERT 推送的行数（chunk 计数）。
    pub rows: usize,
    /// 实际生效的通道。
    pub mode: MaterializeMode,
    /// 快路径失败回落 INSERT 时的降级原因（快路径成功为 `None`；#17 降级可观测：
    /// CLI 据此写 stderr WARN，stdout 契约不变）。
    pub degraded: Option<String>,
}

/// 快路径失败分类（#16）：`loaded = true` 表示 `COPY FROM` **已成功入库**——
/// 此时自动回落 INSERT 会把本地表灌成两倍行数，必须直接上抛，绝不回落。
#[derive(Debug)]
struct FastPathFailure {
    err: EngineError,
    loaded: bool,
}

impl GaussConn {
    /// 物化（§13.1）：GaussDB 表（可带 WHERE）→ 本地 DuckDB 表。
    /// 要求 DuckDB 侧表已由 deploy 通道建好（类型一致，§13.1 类型保真）；
    /// 缺表在拉取前由 [`Self::ensure_local_table`] 显式红（V26.11，消灭零行假绿）。
    ///
    /// V27：默认走 COPY STREAM 快路径（D14：两侧显式 `NULL '\N'`，空串不塌缩）。
    /// 入库前失败自动回落 V26 的 500 行批量 INSERT，降级原因随
    /// [`MaterializeOutcome::degraded`] 带出（#17）；**入库后失败（#16：行数核对、
    /// 交叉核对、收尾）直接上抛，绝不回落**——否则 INSERT 追加会把本地表灌成
    /// 两倍行数。`workdir` 为临时 CSV 的落盘目录（成功即删，失败保留供诊断）。
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
            Err(fast) => Self::apply_fast_failure(fast, || {
                self.materialize_insert_batch(duck, table, &select_sql)
            }),
        }
    }

    /// 快路径失败的处理分岔（#16 守护接缝，`insert_batch` 闭包注入使单测无需
    /// 真连接）：`loaded = true`（COPY 已入库）时直接上抛、**绝不回落 INSERT**——
    /// 否则本地表行数翻倍且 `rows` 报单份数量，错误数据 + 错误计数双重伪装；
    /// `loaded = false`（入库前失败）回落 500 行批量 INSERT，降级原因随
    /// [`MaterializeOutcome::degraded`] 带出（#17 可观测），双失败才拼进消息。
    fn apply_fast_failure<F>(
        fast: FastPathFailure,
        insert_batch: F,
    ) -> Result<MaterializeOutcome, EngineError>
    where
        F: FnOnce() -> Result<usize, EngineError>,
    {
        if fast.loaded {
            return Err(fast.err);
        }
        insert_batch()
            .map_err(|insert_err| {
                EngineError::env(
                    20,
                    format!(
                        "快路径失败（{}），INSERT 回落亦失败：{insert_err}",
                        fast.err
                    ),
                )
            })
            .map(|rows| MaterializeOutcome {
                rows,
                mode: MaterializeMode::InsertBatch,
                degraded: Some(fast.err.to_string()),
            })
    }

    /// V27 快路径：COPY TO STDOUT CSV（流式落盘，不整表驻留内存）→
    /// DuckDB `COPY ... FROM`。入库前的失败交由调用方回落（`loaded = false`），
    /// 入库后的失败带 `loaded = true` 上抛（#16）；本方法不吞错。
    fn materialize_copystream(
        &mut self,
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        select_sql: &str,
        workdir: &Path,
    ) -> Result<MaterializeOutcome, FastPathFailure> {
        let before =
            Self::local_count(duck, table).map_err(|err| FastPathFailure { err, loaded: false })?;
        // 列名经 prepare 取语句元数据（0 行查询在简单协议下无 Row 消息、
        // fetch_rows 的 schema 会是空的），COPY 的列清单按名对齐，
        // 与 INSERT 路径同一口径
        let cols = match self.source_columns(select_sql) {
            Ok(c) => c.join(", "),
            Err(err) => return Err(FastPathFailure { err, loaded: false }),
        };
        std::fs::create_dir_all(workdir).map_err(|e| FastPathFailure {
            err: EngineError::env(20, format!("物化临时目录 {}: {e}", workdir.display())),
            loaded: false,
        })?;
        let csv = workdir.join(format!(".spinduck-mat-{table}-{}.csv", std::process::id()));
        // 卸载期失败发生在入库前（loaded = false），回落 INSERT 安全
        let csv_rows = match self.copy_to_csv(select_sql, &csv) {
            Ok(n) => n,
            Err(err) => return Err(FastPathFailure { err, loaded: false }),
        };
        let load = format!(
            "COPY {table} ({cols}) FROM '{}' (FORMAT CSV, HEADER true, \
             DELIMITER ',', QUOTE '\"', ESCAPE '\"', NULLSTR '\\N')",
            csv.display().to_string().replace('\'', "''")
        );
        if let Err(e) = duck.execute(&load, &BindMap::new()) {
            // 失败保留 CSV 供诊断（设计文档 §3.1），错误带路径；autocommit 下
            // 失败的 COPY 整条回滚、0 行入库，此形态回落 INSERT 安全
            return Err(FastPathFailure {
                err: EngineError::env(
                    20,
                    format!("DuckDB COPY FROM {} 失败（CSV 已保留）：{e}", csv.display()),
                ),
                loaded: false,
            });
        }
        Self::finalize_copystream(duck, table, before, &csv, csv_rows)
    }

    /// 快路径入库后的收尾（#16 接缝，单测可直测）：行数差值、CSV 行数交叉
    /// 核对（设计 §4 第 4 项——部分加载/重复加载的唯一独立哨兵）、清理临时 CSV。
    /// 此阶段数据已在本地表中，任何失败一律 `loaded = true` 上抛，绝不回落；
    /// 唯一例外是 CSV 清理失败——设计 §3.1 本就是「失败保留供诊断」，降级为
    /// 保留文件继续成功，不作为致命错误。
    fn finalize_copystream(
        duck: &dyn crate::exec::SqlExecutor,
        table: &str,
        before: i64,
        csv: &Path,
        csv_rows: usize,
    ) -> Result<MaterializeOutcome, FastPathFailure> {
        let loaded = |err: EngineError| FastPathFailure { err, loaded: true };
        let after = Self::local_count(duck, table).map_err(loaded)?;
        // 差值保持 i64 直呈（评审 nit）：after < before 时负数如实进错误消息，
        // 不做 usize 环绕伪装；核对通过后差值必然非负，再收敛成 usize
        let rows = after - before;
        if rows != csv_rows as i64 {
            return Err(loaded(EngineError::env(
                20,
                format!(
                    "行数交叉核对失败（表 {table}）：CSV 数据行数 {csv_rows}，\
                     本地行数差值 {rows}——部分加载/重复加载嫌疑，CSV 已保留供诊断"
                ),
            )));
        }
        // 清理失败非致命（§3.1 失败诊断现场自包含）：保留文件，不回落不报错
        let _ = std::fs::remove_file(csv);
        Ok(MaterializeOutcome {
            rows: rows as usize,
            mode: MaterializeMode::CopyStream,
            degraded: None,
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
    /// 返回 CSV 数据行数（表头除外）：落盘与计数同一次流式循环完成（评审 P1
    /// 修正——不事后读文件，编码无关、零额外 I/O、零内存驻留）。
    fn copy_to_csv(&mut self, select_sql: &str, csv_path: &Path) -> Result<usize, EngineError> {
        let copy_sql =
            format!("COPY ({select_sql}) TO STDOUT WITH CSV HEADER DELIMITER ',' NULL '\\N'");
        let mut reader = self.client.copy_out(&copy_sql).map_err(db_err)?;
        let file = std::fs::File::create(csv_path)
            .map_err(|e| EngineError::env(20, format!("创建 {}: {e}", csv_path.display())))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut counter = CsvRecordCounter::default();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| EngineError::env(20, format!("gaussdb copy_out: {e}")))?;
            if n == 0 {
                break;
            }
            counter.feed(&buf[..n]);
            writer
                .write_all(&buf[..n])
                .map_err(|e| EngineError::env(20, format!("gaussdb copy_out: {e}")))?;
        }
        writer
            .flush()
            .map_err(|e| EngineError::env(20, format!("gaussdb copy_out flush: {e}")))?;
        counter.finish()
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
}

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
    // 前置条件硬断言（#21-4）：release 下也生效。长度不一致时线程池按索引取
    // 执行器会越界 panic，且 panic 经 thread::scope 抛出主线程，与「单表失败不
    // 中断其余表」相反——公共 API 的契约不能只靠调用方自觉。
    assert_eq!(
        tables.len(),
        duck_executors.len(),
        "materialize_parallel: 表与执行器须一一对应"
    );
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

/// CSV 记录计数状态机（评审 P1 修正）：字节级、编码无关——`"` `,` `\n` `\r`
/// 均为 ASCII 定界符，不可能是 UTF-8 尾字节（≥0x80）或 GBK/Big5 尾字节
/// （≥0x40）的组成部分，故 SQL_ASCII/GBK 等非 UTF-8 源端下扫描同样安全；
/// 随卸载流式喂入，不读文件、不整文件驻内存。
#[derive(Default)]
struct CsvRecordCounter {
    records: usize,
    in_quotes: bool,
    in_record: bool,
    saw_any: bool,
}

impl CsvRecordCounter {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.saw_any = true;
            match b {
                b'"' => {
                    self.in_quotes = !self.in_quotes;
                    self.in_record = true;
                }
                b'\n' if !self.in_quotes => {
                    if self.in_record {
                        self.records += 1;
                    }
                    self.in_record = false;
                }
                b'\r' if !self.in_quotes => {}
                _ => self.in_record = true,
            }
        }
    }

    /// 数据行数（记录数 − 表头；空输入报错，调用方归域 ENV）。
    /// `COPY ... HEADER` 至少产出一行表头，空流属异常（连接被断/服务端未返回），
    /// 报错而非放行 0 行。
    fn finish(self) -> Result<usize, EngineError> {
        let mut records = self.records;
        if self.in_record {
            records += 1;
        }
        if !self.saw_any {
            return Err(EngineError::env(20, "CSV 为空，无法核对行数".to_string()));
        }
        Ok(records.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ExecOutcome, RowStream, SqlExecutor};
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// 快路径收尾用假执行器：`query` 对含 `count(*)` 的语句回放 `count`；
    /// `execute` 记录 SQL 且默认成功。`fail_query` 打开后 count 查询返回
    /// 注入错误（#16：入库后 local_count 失败的形态）。
    struct FakeCopyDuck {
        count: i64,
        fail_query: bool,
        seen_sql: RefCell<Vec<String>>,
    }

    impl FakeCopyDuck {
        fn loaded(count: i64) -> Self {
            Self {
                count,
                fail_query: false,
                seen_sql: RefCell::new(Vec::new()),
            }
        }

        fn broken(count: i64) -> Self {
            Self {
                count,
                fail_query: true,
                seen_sql: RefCell::new(Vec::new()),
            }
        }
    }

    impl SqlExecutor for FakeCopyDuck {
        fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
            if sql.contains("count(*)") && self.fail_query {
                return Err(EngineError::env(20, "注入：入库后行数查询失败".to_string()));
            }
            Ok(RowStream::new(
                vec!["count_star()".into()],
                vec![vec![SpinValue::Int(self.count)]],
            ))
        }

        fn execute(&self, sql: &str, _binds: &BindMap) -> Result<ExecOutcome, EngineError> {
            self.seen_sql.borrow_mut().push(sql.into());
            Ok(ExecOutcome { rows_affected: 0 })
        }
    }

    // ---- #16：入库后失败绝不回落 INSERT（快路径收尾接缝） ----

    /// 临时 CSV 夹具：`rows` 条数据行（自动加表头），落 `env::temp_dir()` 下
    /// 的独立子目录，测试结束自行清理。
    fn csv_fixture(name: &str, data_rows: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let dir =
            std::env::temp_dir().join(format!("spinduck-guard-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时夹具目录");
        let csv = dir.join("mat.csv");
        let mut body = String::from("c1,c2\n");
        for r in data_rows {
            body.push_str(r);
            body.push('\n');
        }
        std::fs::write(&csv, body).expect("写 CSV 夹具");
        (csv, vec![dir])
    }

    #[test]
    fn should_not_fall_back_to_insert_when_copy_already_loaded() {
        // #16 回归守护：COPY 成功入库后的失败（行数查询失败）必须原样上抛，
        // 回落 INSERT 会把本地表灌成两倍行数
        let duck = FakeCopyDuck::broken(5);
        let (csv, dirs) = csv_fixture("loaded-count-fail", &["a,1"]);
        let fast = GaussConn::finalize_copystream(&duck, "t", 0, &csv, 1);
        assert!(fast.is_err(), "入库后失败必须报错，不得吞成成功");
        let fast = fast.unwrap_err();
        assert!(fast.loaded, "行数查询失败发生在入库后，必须标记 loaded");
        assert!(fast.err.to_string().starts_with("[ENV-0020]"), "{fast:?}");
        // 关键断言：该失败形态下绝不触发 INSERT 回落
        let insert_ran = std::cell::RefCell::new(false);
        let out = GaussConn::apply_fast_failure(fast, || {
            insert_ran.replace(true);
            Ok(5)
        });
        assert!(out.is_err(), "loaded 失败不得走 INSERT 回落：{out:?}");
        assert!(!insert_ran.into_inner(), "INSERT 路径不得被执行");
        let _ = dirs.iter().try_for_each(std::fs::remove_dir_all);
    }

    #[test]
    fn should_err_loaded_when_csv_rows_mismatch_loaded_rows() {
        // 设计 §4 第 4 项：CSV 数据行数 vs 本地行数差值交叉核对——部分加载/
        // 重复加载的唯一独立哨兵
        let duck = FakeCopyDuck::loaded(7);
        let (csv, dirs) = csv_fixture("row-mismatch", &["a,1", "b,2"]);
        let fast = GaussConn::finalize_copystream(&duck, "t", 0, &csv, 2);
        let err = fast.expect_err("行数不一致必须报错").err;
        let text = err.to_string();
        assert!(text.contains("行数交叉核对失败"), "{text}");
        assert!(text.contains("部分加载/重复加载嫌疑"), "{text}");
        let _ = dirs.iter().try_for_each(std::fs::remove_dir_all);
    }

    #[test]
    fn should_keep_file_and_succeed_when_csv_cleanup_fails() {
        // 设计 §3.1：清理失败不是致命错误——保留文件、正常返回 CopyStream
        // （Unix 下以只读目录制造 remove_file 失败；Windows 无此口径的等价
        // 制造手段，测试不跑）
        use std::os::unix::fs::PermissionsExt;
        // 行数与 CSV 数据行数对齐（1 行），让失败点落在清理而不是行数核对
        let duck = FakeCopyDuck::loaded(1);
        let (csv, dirs) = csv_fixture("cleanup-fail", &["a,1"]);
        let dir = &dirs[0];
        let mut ro = dir
            .as_path()
            .metadata()
            .expect("读夹具目录权限")
            .permissions();
        ro.set_mode(0o555);
        std::fs::set_permissions(dir, ro).expect("置只读目录");
        let out = GaussConn::finalize_copystream(&duck, "t", 0, &csv, 1);
        let mut rw = dir
            .as_path()
            .metadata()
            .expect("读夹具目录权限")
            .permissions();
        rw.set_mode(0o755);
        std::fs::set_permissions(dir, rw).expect("恢复目录权限");
        let out = out.expect("清理失败必须降级为成功");
        assert_eq!(out.mode, MaterializeMode::CopyStream);
        assert_eq!(out.rows, 1);
        assert!(out.degraded.is_none(), "快路径成功不得带降级原因");
        assert!(csv.exists(), "清理失败必须保留 CSV 供诊断");
        let _ = dirs.iter().try_for_each(std::fs::remove_dir_all);
    }

    #[test]
    fn should_fall_back_with_degraded_reason_before_load_fails() {
        // #17：入库前失败回落 INSERT，降级原因随 degraded 带出，不静默
        let fast = FastPathFailure {
            err: EngineError::env(20, "DuckDB COPY FROM x 失败".to_string()),
            loaded: false,
        };
        let insert_sql = RefCell::new(Vec::new());
        let out = GaussConn::apply_fast_failure(fast, || {
            insert_sql
                .borrow_mut()
                .push("INSERT INTO t (c) VALUES (1)".to_string());
            Ok(3)
        })
        .expect("入库前失败应成功回落");
        assert_eq!(out.mode, MaterializeMode::InsertBatch);
        assert_eq!(out.rows, 3);
        let reason = out.degraded.expect("回落必须携带降级原因");
        // 降级原因 = 快路径失败的原始错误呈现（Display），逐字不加工
        assert!(reason.contains("DuckDB COPY"), "{reason}");
        assert_eq!(insert_sql.borrow().len(), 1, "回落必须真的执行 INSERT");
    }

    // ---- 行数哨兵：字节级计数（编码无关，评审 P1 守护） ----

    fn count_bytes(chunks: &[&[u8]]) -> usize {
        let mut c = CsvRecordCounter::default();
        for chunk in chunks {
            c.feed(chunk);
        }
        c.finish().expect("计数成功")
    }

    #[test]
    fn should_count_csv_records_byte_level_quote_aware() {
        // 引号翻倍与内嵌换行不得多计；跨 chunk 喂入与单块等价
        let body = b"a,b\n\"x,y\n,z\",1\n\"say \"\"hi\"\"\",2\n";
        let split = body.len() / 2;
        assert_eq!(count_bytes(&[body]), 2);
        assert_eq!(count_bytes(&[&body[..split], &body[split..]]), 2);
    }

    #[test]
    fn should_count_non_utf8_csv_bytes_quote_aware() {
        // 评审 P1 守护：SQL_ASCII/GBK 源端原样落字节（0xD6 0xD0 = GBK「中」，
        // 尾字节 ≥0x40），ASCII 定界符不可能落在多字节序列内部——引号感知
        // 计数不受编码影响，含内嵌换行的字段照常不拆
        let body = b"v,amt\n\"\xD6\xD0\xB9\xFA\",1\n\"\xC3\xF7\nx\",2\n";
        assert_eq!(count_bytes(&[body]), 2);
    }

    #[test]
    fn should_err_when_csv_stream_is_empty() {
        let err = CsvRecordCounter::default()
            .finish()
            .expect_err("空流必须报错");
        assert!(err.to_string().contains("ENV-0020"), "{err}");
    }

    // ---- #21-4：parallel 前置条件在 release 下同样生效 ----

    #[test]
    #[should_panic(expected = "一一对应")]
    fn should_panic_when_executor_count_mismatches_tables() {
        // debug_assert 只在 debug 生效；公共 API 的前置条件必须硬断言
        let executors: Vec<FakeCopyDuck> = Vec::new();
        let _ = crate::io::materialize_parallel(
            1,
            vec!["t1".to_string()],
            Path::new("."),
            || unreachable!("断言先于连接发生"),
            executors,
        );
    }
}
