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
    /// 收尾 SQL（同事务，行数不计入结果；upsert 的 DROP staging 表）。
    finalize_sql: Option<String>,
}

impl WritePlan {
    fn new(table: &str, cols: &[String], mode: &SyncMode) -> Self {
        match mode {
            SyncMode::Append { .. } => Self {
                tx: false,
                copy_target: table.to_string(),
                pre_sql: Vec::new(),
                post_sql: Vec::new(),
                finalize_sql: None,
            },
            SyncMode::Slice { predicates } => Self {
                tx: true,
                copy_target: table.to_string(),
                pre_sql: vec![format!(
                    "DELETE FROM {table} WHERE {}",
                    slice_where(predicates)
                )],
                post_sql: Vec::new(),
                finalize_sql: None,
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
                    post_sql: vec![update, insert],
                    finalize_sql: Some(format!("DROP TABLE {TMP_TABLE}")),
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
        if let SyncMode::Slice { predicates } = mode {
            let n = staging_slice_violations(duck, table, predicates)?;
            if n > 0 {
                return Err(EngineError::business(
                    BIZ_SLICE_VIOLATION,
                    format!("staging 有 {n} 行不满足 --slice 谓词（NULL 同判越界）"),
                ));
            }
        }
        if let SyncMode::Upsert { keys } = mode {
            validate_keys_subset(keys, cols)?;
            assert_staging_keys(duck, table, keys)?;
            self.guard_unique_index(table, keys)?;
        }
        let select_sql = crate::io::sync::mode_select_sql(table, cols, mode);
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
        let (schema, bare) = crate::io::sync::split_schema(table);
        let mut cond = format!("lower(table_name) = lower('{}')", bare.replace('\'', "''"));
        if let Some(sc) = schema {
            // schema 限定名必须同时约束 table_schema，防跨 schema 同名表串列
            cond.push_str(&format!(
                " AND lower(table_schema) = lower('{}')",
                sc.replace('\'', "''")
            ));
        }
        let sql = format!(
            "SELECT column_name, is_nullable, coalesce(column_default, '') \
             FROM information_schema.columns WHERE {cond}"
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
             CROSS JOIN unnest(string_to_array(con.conkey::text, ' ')::int[]) k \
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
            self.run_rows(duck, &ctx.select_sql, &plan, cols, mode)
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
        // V26 同款纪律：服务端 copied 必须与本地行数相等（快路径不豁免）
        let count_sql = format!("SELECT count(*) FROM ({select_sql})");
        let local_n = crate::io::sync::scalar_count(&mut duck.query(&count_sql, &BindMap::new())?)?;
        let csv = self.export_csv(duck, select_sql, workdir, mode)?;
        for sql in &plan.pre_sql {
            self.exec(sql)?;
        }
        let copied = self.copy_file_to_target(&csv, &plan.copy_target, cols)?;
        let mut post_rows = 0u64;
        for sql in &plan.post_sql {
            post_rows += self.exec(sql)?;
        }
        if let Some(fin) = &plan.finalize_sql {
            self.exec(fin)?;
        }
        std::fs::remove_file(&csv)
            .map_err(|e| EngineError::env(20, format!("清理临时 CSV {}: {e}", csv.display())))?;
        match mode {
            SyncMode::Upsert { .. } => {
                if post_rows as i64 != copied as i64 {
                    return Err(EngineError::env(
                        20,
                        format!(
                            "upsert 行数不一致：staging {copied} 行，UPDATE+INSERT {post_rows} 行"
                        ),
                    ));
                }
            }
            _ => {
                if copied as i64 != local_n {
                    return Err(EngineError::env(
                        20,
                        format!("快路径行数不一致：本地 {local_n} 行，服务端 {copied} 行"),
                    ));
                }
            }
        }
        Ok(copied as usize)
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
        select_sql: &str,
        plan: &WritePlan,
        cols: &[String],
        mode: &SyncMode,
    ) -> Result<usize, EngineError> {
        // 回落同样以 select_sql 为准（append 带 --where 时不得退化为全表）
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_plan_slice_as_delete_then_copy_in_tx() {
        let preds = crate::io::sync::resolve_slice("biz_date=20261111").unwrap();
        let plan = WritePlan::new(
            "t_out",
            &["a".to_string()],
            &SyncMode::Slice { predicates: preds },
        );
        assert!(plan.tx, "slice 必须显式事务（DELETE+COPY 原子）");
        assert_eq!(plan.copy_target, "t_out");
        assert_eq!(
            plan.pre_sql,
            vec!["DELETE FROM t_out WHERE biz_date = '20261111'"]
        );
        assert!(plan.post_sql.is_empty());
        assert!(plan.finalize_sql.is_none());
    }

    #[test]
    fn should_plan_upsert_with_staging_tmp_and_finalize_drop() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let plan = WritePlan::new(
            "t_out",
            &cols,
            &SyncMode::Upsert {
                keys: vec!["a".into()],
            },
        );
        assert!(plan.tx);
        assert_eq!(plan.copy_target, TMP_TABLE);
        assert_eq!(
            plan.pre_sql,
            vec!["CREATE TEMP TABLE sd_sync_tmp AS SELECT a, b FROM t_out LIMIT 0"]
        );
        assert_eq!(
            plan.post_sql.len(),
            2,
            "post 仅 UPDATE+INSERT，DROP 不得计入行数"
        );
        assert!(plan.post_sql[0].starts_with("UPDATE t_out SET b = tmp.b"));
        assert!(plan.post_sql[1].starts_with("INSERT INTO t_out"));
        assert_eq!(plan.finalize_sql.as_deref(), Some("DROP TABLE sd_sync_tmp"));
    }

    #[test]
    fn should_plan_append_without_tx() {
        let plan = WritePlan::new(
            "t_out",
            &["a".to_string()],
            &SyncMode::Append { where_clause: None },
        );
        assert!(!plan.tx, "append 单语句天然原子，不加 BEGIN");
        assert!(plan.pre_sql.is_empty() && plan.post_sql.is_empty() && plan.finalize_sql.is_none());
    }
}
