//! sync 回写执行层：事务骨架（BEGIN/COMMIT/ROLLBACK）、DuckDB `COPY TO` CSV
//! 中转快路径与逐行回落。纯逻辑/守卫 SQL 构造见 `sync.rs`，本模块只消费它们。

use crate::error::EngineError;
use crate::exec::{BindMap, RowStream, SqlExecutor};
use crate::io::gauss::{csv_field, GaussConn};
use crate::io::sync::{
    assert_staging_keys, build_upsert_statements, has_unique_covering, schema_violations,
    slice_where, staging_slice_violations, validate_keys_subset, SyncMode, SyncOutcome, TargetCol,
    UpsertStatements, BIZ_SLICE_VIOLATION, ENV_NO_UNIQUE_KEY, ENV_SCHEMA_DRIFT, TMP_TABLE,
};
use crate::value::SpinValue;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// 单次回写执行上下文：读取 SELECT + 快路径工作目录（None = 逐行回落）。
struct RunCtx {
    select_sql: String,
    workdir: Option<PathBuf>,
    fast: bool,
}

/// 单次回写的执行计划：COPY 目标 + 事务前后置 SQL（设计 §4.2/§4.3）。
struct WritePlan {
    /// 显式事务骨架（slice/upsert 需要；append 单语句天然原子）。
    tx: bool,
    /// COPY FROM STDIN 的目标（目标表或 upsert staging 临时表）。
    copy_target: String,
    /// COPY 前执行（同事务）。
    pre_sql: Vec<String>,
    /// COPY 后执行（同事务；upsert 的 UPDATE/INSERT 行数计入结果）。
    post_sql: Vec<String>,
}

impl WritePlan {
    fn new(table: &str, cols: &[String], mode: &SyncMode) -> Self {
        match mode {
            SyncMode::Append { .. } => Self {
                tx: false,
                copy_target: table.to_string(),
                pre_sql: Vec::new(),
                post_sql: Vec::new(),
            },
            SyncMode::Slice { predicates } => Self {
                tx: true,
                copy_target: table.to_string(),
                pre_sql: vec![format!(
                    "DELETE FROM {table} WHERE {}",
                    slice_where(predicates)
                )],
                post_sql: Vec::new(),
            },
            SyncMode::Upsert { keys } => {
                let collist = cols.join(", ");
                let UpsertStatements { update, insert } =
                    build_upsert_statements(table, cols, keys);
                Self {
                    tx: true,
                    copy_target: TMP_TABLE.to_string(),
                    pre_sql: vec![format!(
                        "CREATE TEMP TABLE {TMP_TABLE} AS SELECT {collist} FROM {table} LIMIT 0"
                    )],
                    post_sql: vec![update, insert, format!("DROP TABLE {TMP_TABLE}")],
                }
            }
        }
    }
}

impl GaussConn {
    /// 回写入口（三模式；守卫全部前置于触碰目标数据）。
    /// 快路径失败自动回落逐行通道（错误并入最终消息，不静默）。
    pub fn sync_write(
        &mut self,
        duck: &dyn SqlExecutor,
        table: &str,
        cols: &[String],
        mode: &SyncMode,
        workdir: &Path,
    ) -> Result<SyncOutcome, EngineError> {
        self.guard_target_schema(table, cols)?;
        let select_sql = match mode {
            SyncMode::Append { .. } => crate::io::sync::read_select(table, cols, None),
            SyncMode::Slice { predicates } => {
                let n = staging_slice_violations(duck, table, predicates)?;
                if n > 0 {
                    return Err(EngineError::env(
                        BIZ_SLICE_VIOLATION,
                        format!("staging 有 {n} 行不满足 --slice 谓词（NULL 同判越界）"),
                    ));
                }
                format!(
                    "SELECT {} FROM {} WHERE {}",
                    cols.join(", "),
                    table,
                    slice_where(predicates)
                )
            }
            SyncMode::Upsert { keys } => {
                validate_keys_subset(keys, cols)?;
                assert_staging_keys(duck, table, keys)?;
                self.guard_unique_index(table, keys)?;
                crate::io::sync::read_select(table, cols, None)
            }
        };
        let fast_ctx = RunCtx {
            select_sql: select_sql.clone(),
            workdir: Some(workdir.to_path_buf()),
            fast: true,
        };
        let rows = match self.run(duck, table, cols, mode, fast_ctx) {
            Ok(n) => n,
            Err(fast_err) => {
                let row_ctx = RunCtx {
                    select_sql,
                    workdir: None,
                    fast: false,
                };
                self.run(duck, table, cols, mode, row_ctx).map_err(|e| {
                    EngineError::env(20, format!("快路径失败（{fast_err}），逐行回落亦失败：{e}"))
                })?
            }
        };
        Ok(SyncOutcome {
            rows,
            mode_label: mode.label(),
        })
    }

    /// G1：目标 schema 漂移守卫（information_schema 元数据，零行读取）。
    fn guard_target_schema(&mut self, table: &str, cols: &[String]) -> Result<(), EngineError> {
        let t = table.replace('\'', "''");
        let sql = format!(
            "SELECT column_name, is_nullable, coalesce(column_default, '') \
             FROM information_schema.columns WHERE lower(table_name) = lower('{t}')"
        );
        let (_, rows) = self.fetch_rows(&sql)?;
        let target: Vec<TargetCol> = rows
            .iter()
            .filter_map(|r| {
                match (
                    r.first().and_then(|o| o.as_deref()),
                    r.get(1).and_then(|o| o.as_deref()),
                    r.get(2).and_then(|o| o.as_deref()),
                ) {
                    (Some(n), Some(nul), Some(def)) => Some(TargetCol {
                        name: n.to_string(),
                        not_null: nul.eq_ignore_ascii_case("NO"),
                        has_default: !def.is_empty(),
                    }),
                    _ => None,
                }
            })
            .collect();
        if target.is_empty() {
            return Err(EngineError::env(
                ENV_SCHEMA_DRIFT,
                format!("目标表 {table} 在 information_schema 中无列记录（不存在或无权限）"),
            ));
        }
        let v = schema_violations(&target, cols);
        if !v.is_empty() {
            return Err(EngineError::env(
                ENV_SCHEMA_DRIFT,
                format!("目标表 {table} schema 漂移：{}", v.join("；")),
            ));
        }
        Ok(())
    }

    /// G4：目标 key 须有唯一约束/PK 覆盖（pg_constraint，fail-closed）。
    fn guard_unique_index(&mut self, table: &str, keys: &[String]) -> Result<(), EngineError> {
        let bare = table.split('.').next_back().unwrap_or(table);
        let t = bare.replace('\'', "''");
        let sql = format!(
            "SELECT coalesce(string_agg(a.attname, ',' ORDER BY a.attnum), '') \
             FROM pg_constraint con \
             JOIN pg_class rel ON rel.oid = con.conrelid \
             CROSS JOIN unnest(con.conkey) k \
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k \
             WHERE con.contype IN ('p', 'u') AND lower(rel.relname) = lower('{t}') \
             GROUP BY con.oid"
        );
        let (_, rows) = self.fetch_rows(&sql)?;
        let constraints: Vec<Vec<String>> = rows
            .iter()
            .filter_map(|r| {
                r.first().and_then(|o| o.as_deref()).map(|s| {
                    s.split(',')
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .collect()
                })
            })
            .collect();
        if !has_unique_covering(&constraints, keys) {
            return Err(EngineError::env(
                ENV_NO_UNIQUE_KEY,
                format!(
                    "目标表 {table} 的 upsert key {keys:?} 无唯一约束/PK 覆盖——\
                     先建唯一索引再 upsert（并发双写 fail-closed）"
                ),
            ));
        }
        Ok(())
    }

    /// 执行一次回写计划：快路径（CSV 中转）或逐行回落，事务骨架统一在此。
    fn run(
        &mut self,
        duck: &dyn SqlExecutor,
        table: &str,
        cols: &[String],
        mode: &SyncMode,
        ctx: RunCtx,
    ) -> Result<usize, EngineError> {
        let plan = WritePlan::new(table, cols, mode);
        if plan.tx {
            self.exec("BEGIN")?;
        }
        let result = if ctx.fast {
            self.run_fast(
                duck,
                &ctx.select_sql,
                &plan,
                cols,
                ctx.workdir.as_deref(),
                mode,
            )
        } else {
            self.run_rows(duck, table, &ctx.select_sql, &plan, cols, mode)
        };
        match result {
            Ok(n) => {
                if plan.tx {
                    self.exec("COMMIT")?;
                }
                Ok(n)
            }
            Err(e) => {
                if plan.tx {
                    let _ = self.exec("ROLLBACK");
                }
                Err(e)
            }
        }
    }

    /// 快路径：DuckDB 内核导出 CSV → GaussDB copy_in 流式灌入（零逐行编码）。
    /// 成功即删临时 CSV，失败保留供诊断（与物化 §3.1 同纪律）。
    fn run_fast(
        &mut self,
        duck: &dyn SqlExecutor,
        select_sql: &str,
        plan: &WritePlan,
        cols: &[String],
        workdir: Option<&Path>,
        mode: &SyncMode,
    ) -> Result<usize, EngineError> {
        let workdir = workdir.ok_or_else(|| EngineError::env(20, "快路径缺工作目录"))?;
        let csv = self.export_csv(duck, select_sql, workdir, mode)?;
        for sql in &plan.pre_sql {
            self.exec(sql)?;
        }
        let copied = self.copy_file_to_target(&csv, &plan.copy_target, cols)?;
        let mut post_rows = 0u64;
        for sql in &plan.post_sql {
            post_rows += self.exec(sql)?;
        }
        std::fs::remove_file(&csv)
            .map_err(|e| EngineError::env(20, format!("清理临时 CSV {}: {e}", csv.display())))?;
        let rows = match mode {
            SyncMode::Upsert { .. } => post_rows as usize,
            _ => copied as usize,
        };
        Ok(rows)
    }

    /// DuckDB 内核 COPY TO：导出列序/转义/NULL 口径由内核保证（设计 §4.1）。
    fn export_csv(
        &self,
        duck: &dyn SqlExecutor,
        select_sql: &str,
        workdir: &Path,
        mode: &SyncMode,
    ) -> Result<PathBuf, EngineError> {
        std::fs::create_dir_all(workdir).map_err(|e| {
            EngineError::env(20, format!("回写临时目录 {}: {e}", workdir.display()))
        })?;
        let tag = match mode {
            SyncMode::Append { .. } => "append",
            SyncMode::Slice { .. } => "slice",
            SyncMode::Upsert { .. } => "upsert",
        };
        let csv = workdir.join(format!(".spinduck-sync-{tag}-{}.csv", std::process::id()));
        let sql = format!(
            "COPY ({select_sql}) TO '{}' (FORMAT CSV, HEADER false, DELIMITER ',')",
            csv.display().to_string().replace('\'', "''")
        );
        duck.execute(&sql, &BindMap::new())?;
        Ok(csv)
    }

    /// CSV 文件 → GaussDB `COPY FROM STDIN`（单事务流式，字节直灌）。
    fn copy_file_to_target(
        &mut self,
        csv: &Path,
        target: &str,
        cols: &[String],
    ) -> Result<u64, EngineError> {
        let copy_sql = format!(
            "COPY {target} ({}) FROM STDIN (FORMAT csv)",
            cols.join(", ")
        );
        let mut w = self
            .client
            .copy_in(&copy_sql)
            .map_err(crate::io::gauss::db_err)?;
        let mut f = std::fs::File::open(csv)
            .map_err(|e| EngineError::env(20, format!("打开 {}: {e}", csv.display())))?;
        std::io::copy(&mut f, &mut w)
            .map_err(|e| EngineError::env(20, format!("gaussdb copy_in: {e}")))?;
        w.finish().map_err(crate::io::gauss::db_err)
    }

    /// 逐行回落：append 复用 V26 `sync_append`；slice/upsert 走同事务行循环。
    fn run_rows(
        &mut self,
        duck: &dyn SqlExecutor,
        table: &str,
        select_sql: &str,
        plan: &WritePlan,
        cols: &[String],
        mode: &SyncMode,
    ) -> Result<usize, EngineError> {
        if let SyncMode::Append { .. } = mode {
            return self.sync_append(duck, table, cols);
        }
        let mut s = duck.query(select_sql, &BindMap::new())?;
        for sql in &plan.pre_sql {
            self.exec(sql)?;
        }
        let written = self.write_rows_via_copy(&mut s, &plan.copy_target, cols)?;
        let mut post_rows = 0u64;
        for sql in &plan.post_sql {
            post_rows += self.exec(sql)?;
        }
        let rows = match mode {
            SyncMode::Upsert { .. } => post_rows as usize,
            _ => written,
        };
        Ok(rows)
    }

    /// 逐行 COPY 通道（V26 口径：批拉 1000 行、文本承载、行数交叉校验）。
    fn write_rows_via_copy(
        &mut self,
        s: &mut RowStream,
        target: &str,
        cols: &[String],
    ) -> Result<usize, EngineError> {
        let copy_sql = format!(
            "COPY {target} ({}) FROM STDIN (FORMAT csv)",
            cols.join(", ")
        );
        let mut w = self
            .client
            .copy_in(&copy_sql)
            .map_err(crate::io::gauss::db_err)?;
        let mut n = 0usize;
        loop {
            let rows = s.fetch_many(crate::io::gauss::SYNC_BATCH);
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                let line = r
                    .iter()
                    .map(|v| match v {
                        SpinValue::Null => csv_field(None),
                        other => csv_field(Some(&other.as_str_loose())),
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                w.write_all(line.as_bytes())
                    .and_then(|_| w.write_all(b"\n"))
                    .map_err(|e| EngineError::env(20, format!("gaussdb copy: {e}")))?;
                n += 1;
            }
        }
        let copied = w.finish().map_err(crate::io::gauss::db_err)? as usize;
        if copied != n {
            return Err(EngineError::env(
                20,
                format!("gaussdb: COPY 行数不一致：服务端 {copied}，本地 {n}"),
            ));
        }
        Ok(n)
    }
}
