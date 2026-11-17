//! sync 回写端到端（openGauss 容器实测）：守卫族 + 三模式冲突处理。
//! 门控：feature gauss + 环境变量 SPINDUCK_OG_HOST；缺省或容器连不上即 panic
//! （`require_og`，跳过不等于通过——同 gauss_e2e 契约）。
//!
//! 容器环境同 `gauss_e2e.rs`：
//!   docker compose -f golden/gaussdb-docker/docker-compose.yml up -d
//!   SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
//!   SPINDUCK_OG_PASSWORD=… cargo test --features gauss --test gauss_sync_e2e

#![cfg(feature = "gauss")]

use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::io::{sync::SyncMode, GaussConn};
use spinduck_core::sql::DuckExecutor;

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
            &std::env::var("SPINDUCK_OG_PASSWORD")
                .expect("SPINDUCK_OG_PASSWORD 必须显式提供（口令不入库）"),
            &std::env::var("SPINDUCK_OG_DB").unwrap_or_else(|_| "postgres".into()),
        )
        .expect("connect openGauss"),
    )
}

/// §4.1：黄金层禁止"跳过即绿"——缺容器必须显式变红（同 gauss_e2e 口径）。
fn require_og() -> GaussConn {
    og().unwrap_or_else(|| {
        panic!(
            "E2E 用例需要 A 模式容器（golden/gaussdb-docker）：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d\n\
             SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD=… cargo test --features gauss"
        )
    })
}

/// 每测试独立 DuckDB 文件 + 独立 GaussDB 表名（并行测试互不干扰）。
struct Fixture {
    dir: tempfile::TempDir,
    duck: DuckExecutor,
    table: &'static str,
}

fn fixture(table: &'static str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("local.duckdb");
    let duck = DuckExecutor::open(&db.to_string_lossy()).expect("open duckdb");
    Fixture { dir, duck, table }
}

fn cols() -> Vec<String> {
    vec!["id".into(), "v".into(), "w".into()]
}

fn gauss_count(og: &mut GaussConn, table: &str) -> i64 {
    let (_, rows) = og
        .fetch_rows(&format!("SELECT count(*) FROM {table}"))
        .unwrap();
    rows.first()
        .and_then(|r| r.first())
        .and_then(|c| c.as_deref())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(-1)
}

fn gauss_cell(og: &mut GaussConn, sql: &str) -> String {
    let (_, rows) = og.fetch_rows(sql).unwrap();
    rows.first()
        .and_then(|r| r.first())
        .and_then(|c| c.clone())
        .unwrap_or_default()
}

/// G1 回归守护：`coalesce(column_default,'')` 在 Oracle A 模式下 '' 即 NULL，
/// 会让无默认值列整行被丢 → ENV-0021 恒红。append 模式也必经 G1。
#[test]
fn should_sync_append_when_target_has_no_column_defaults() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_app");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_app; \
         CREATE TABLE sd_e2e_app(id int PRIMARY KEY, v int, w text);",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_app AS \
             SELECT i AS id, i AS v, 'x' AS w FROM range(1,4) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let oc = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Append { where_clause: None },
            fx.dir.path(),
        )
        .unwrap();
    assert_eq!(oc.rows, 3);
    assert_eq!(gauss_count(&mut og, "sd_e2e_app"), 3);
}

/// upsert 混合更新/插入：PK 表上已存在键走 UPDATE，新键走 INSERT。
#[test]
fn should_upsert_mixed_update_and_insert_on_pk_table() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_upk");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_upk; \
         CREATE TABLE sd_e2e_upk(id int PRIMARY KEY, v int, w text); \
         INSERT INTO sd_e2e_upk SELECT i, i*10, 'old' FROM generate_series(1,10) i;",
    )
    .unwrap();
    // 本地：id 1..5 更新（v=999）+ id 11..15 插入
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_upk AS \
             SELECT i AS id, 999 AS v, 'new' AS w FROM range(1,6) t(i) \
             UNION ALL SELECT i, i, 'keep' FROM range(11,16) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let oc = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .unwrap();
    assert_eq!(oc.rows, 10, "UPDATE 5 + INSERT 5 = staging 10");
    assert_eq!(
        gauss_cell(&mut og, "SELECT v FROM sd_e2e_upk WHERE id=3"),
        "999"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_upk WHERE id=3"),
        "new"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_upk WHERE id=13"),
        "keep"
    );
    // 未触碰行原样保留
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_upk WHERE id=8"),
        "old"
    );
    assert_eq!(gauss_count(&mut og, "sd_e2e_upk"), 15);
}

/// G4：目标表无任何唯一约束/索引覆盖 key → ENV-0022 fail-closed，目标零写入。
#[test]
fn should_reject_upsert_when_target_lacks_unique_covering_index() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_nopk");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_nopk; \
         CREATE TABLE sd_e2e_nopk(id int, v int, w text); \
         INSERT INTO sd_e2e_nopk SELECT i, i, 'n' FROM generate_series(1,5) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_nopk AS SELECT i AS id, i AS v, 'm' AS w FROM range(1,4) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let err = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("ENV-0022"), "{err}");
    assert_eq!(gauss_count(&mut og, "sd_e2e_nopk"), 5, "守卫失败不得写入");
}

/// G4 口径：裸 `CREATE UNIQUE INDEX`（无 pg_constraint 记录）也必须算覆盖。
#[test]
fn should_accept_upsert_with_bare_unique_index() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_uq");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_uq; \
         CREATE TABLE sd_e2e_uq(id int, v int, w text); \
         CREATE UNIQUE INDEX sd_e2e_uq_id ON sd_e2e_uq(id); \
         INSERT INTO sd_e2e_uq SELECT i, i*10, 'old' FROM generate_series(1,10) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_uq AS \
             SELECT i AS id, 555 AS v, 'new' AS w FROM range(1,6) t(i) \
             UNION ALL SELECT i, i, 'keep' FROM range(11,16) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let oc = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .unwrap();
    assert_eq!(oc.rows, 10);
    assert_eq!(gauss_count(&mut og, "sd_e2e_uq"), 15);
}

/// G3：staging key 重复 → BIZ-0103（先于 G4 判定）。
#[test]
fn should_reject_upsert_when_staging_keys_not_unique() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_dup");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_dup; \
         CREATE TABLE sd_e2e_dup(id int PRIMARY KEY, v int, w text);",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_dup AS \
             SELECT 1 AS id, 1 AS v, 'a' AS w \
             UNION ALL SELECT 1 AS id, 2 AS v, 'b' AS w",
            &BindMap::new(),
        )
        .unwrap();
    let err = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("BIZ-0103"), "{err}");
    assert_eq!(gauss_count(&mut og, "sd_e2e_dup"), 0);
}

/// slice：同事务 DELETE+COPY，重跑幂等（行数与内容不变）。
#[test]
fn should_slice_delete_and_copy_idempotent_on_rerun() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_sl");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_sl; \
         CREATE TABLE sd_e2e_sl(id int, v int, w text); \
         INSERT INTO sd_e2e_sl SELECT i, i, CASE WHEN i<=5 THEN 'mark1' ELSE 'other' END \
           FROM generate_series(1,10) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_sl AS \
             SELECT i AS id, 100+i AS v, 'mark1' AS w FROM range(1,6) t(i) \
             UNION ALL SELECT 50, 50, 'mark1'",
            &BindMap::new(),
        )
        .unwrap();
    let mode = || SyncMode::Slice {
        predicates: vec![spinduck_core::io::sync::SlicePredicate {
            col: "w".into(),
            value: "mark1".into(),
        }],
    };
    let oc1 = og
        .sync_write(&fx.duck, fx.table, &cols(), &mode(), fx.dir.path())
        .unwrap();
    assert_eq!(oc1.rows, 6);
    let oc2 = og
        .sync_write(&fx.duck, fx.table, &cols(), &mode(), fx.dir.path())
        .unwrap();
    assert_eq!(oc2.rows, 6, "重跑仍写 6 行（先删后灌）");
    // 终态：5 行 other + 6 行 mark1 = 11；mark1 行 v 已被覆盖
    assert_eq!(gauss_count(&mut og, "sd_e2e_sl"), 11);
    assert_eq!(
        gauss_cell(&mut og, "SELECT v FROM sd_e2e_sl WHERE id=1"),
        "101"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT count(*) FROM sd_e2e_sl WHERE w='mark1'"),
        "6"
    );
}

/// append 撞 PK：COPY 单语句原子失败，目标零部分写入。
#[test]
fn should_append_fail_atomically_on_pk_conflict() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_conf");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_conf; \
         CREATE TABLE sd_e2e_conf(id int PRIMARY KEY, v int, w text); \
         INSERT INTO sd_e2e_conf SELECT i, i, 'k' FROM generate_series(1,3) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_conf AS \
             SELECT i AS id, i AS v, 'd' AS w FROM range(1,3) t(i) \
             UNION ALL SELECT i, i, 'd' FROM range(100,103) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let err = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Append { where_clause: None },
            fx.dir.path(),
        )
        .unwrap_err();
    let msg = err.to_string();
    // 必须是写入期约束冲突，而非守卫期失败（ENV-0021/0022 时行数同样是 3，
    // 不检查错误域就锁不住"COPY 撞 PK 原子回滚"这一行为）。
    assert!(
        !msg.contains("ENV-0021") && !msg.contains("ENV-0022"),
        "守卫不应先失败：{msg}"
    );
    assert_eq!(
        gauss_count(&mut og, "sd_e2e_conf"),
        3,
        "PK 冲突必须整体回滚：{msg}"
    );
    // 冲突行（id 1,2）未被覆盖、新增行（id 100..102）未留下
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_conf WHERE id=1"),
        "k"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT count(*) FROM sd_e2e_conf WHERE id >= 100"),
        "0"
    );
}

fn cpk_cols() -> Vec<String> {
    vec!["id".into(), "grp".into(), "v".into(), "w".into()]
}

/// G4 方向回归：复合主键 (id,grp) 只传部分键 id → 1 对 N 风险，必须 ENV-0022。
#[test]
fn should_reject_upsert_with_partial_composite_key() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_cpk");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_cpk; \
         CREATE TABLE sd_e2e_cpk(id int, grp int, v int, w text, \
           PRIMARY KEY(id, grp)); \
         INSERT INTO sd_e2e_cpk SELECT i, i%2, i, 'o' FROM generate_series(1,4) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_cpk AS \
             SELECT i AS id, 0 AS grp, 9 AS v, 'n' AS w FROM range(1,3) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let err = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cpk_cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("ENV-0022"), "{err}");
    assert_eq!(gauss_count(&mut og, "sd_e2e_cpk"), 4, "守卫失败不得写入");
}

/// G4 方向回归：复合主键全键 (id,grp) → 放行，混合 UPDATE+INSERT 正确。
#[test]
fn should_upsert_with_full_composite_key() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_ckf");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_ckf; \
         CREATE TABLE sd_e2e_ckf(id int, grp int, v int, w text, \
           PRIMARY KEY(id, grp)); \
         INSERT INTO sd_e2e_ckf VALUES (1,1,10,'old'),(1,2,20,'old2');",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_ckf AS \
             SELECT 1 AS id, 1 AS grp, 999 AS v, 'new' AS w \
             UNION ALL SELECT 2 AS id, 1 AS grp, 5 AS v, 'ins' AS w",
            &BindMap::new(),
        )
        .unwrap();
    let oc = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cpk_cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into(), "grp".into()],
            },
            fx.dir.path(),
        )
        .unwrap();
    assert_eq!(oc.rows, 2);
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_ckf WHERE id=1 AND grp=1"),
        "new"
    );
    assert_eq!(gauss_count(&mut og, "sd_e2e_ckf"), 3);
}

/// 裸表名必须按 search_path 解析：同名表分处两 schema 时，G1 只看可见的那张。
#[test]
fn should_follow_search_path_for_bare_table_names() {
    let mut og = require_og();
    let fx = fixture("t");
    og.exec_script(
        "DROP SCHEMA IF EXISTS sd_xs1 CASCADE; DROP SCHEMA IF EXISTS sd_xs2 CASCADE; \
         CREATE SCHEMA sd_xs1; CREATE SCHEMA sd_xs2; \
         CREATE TABLE sd_xs1.t(id int PRIMARY KEY, v int, w text); \
         INSERT INTO sd_xs1.t SELECT i, i, 'a' FROM generate_series(1,2) i; \
         CREATE TABLE sd_xs2.t(id int, v int); \
         INSERT INTO sd_xs2.t SELECT i, i FROM generate_series(1,7) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE t AS SELECT 10 AS id, 1 AS v, 'x' AS w",
            &BindMap::new(),
        )
        .unwrap();
    // search_path 指向 sd_xs1：裸名 t = sd_xs1.t（列齐全）→ append 成功
    og.exec_script("SET search_path TO sd_xs1").unwrap();
    let oc = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Append { where_clause: None },
            fx.dir.path(),
        )
        .unwrap();
    assert_eq!(oc.rows, 1);
    assert_eq!(gauss_count(&mut og, "sd_xs1.t"), 3);
    assert_eq!(
        gauss_count(&mut og, "sd_xs2.t"),
        7,
        "异名 schema 表不得被误写"
    );
    // search_path 指向 sd_xs2：裸名 t = sd_xs2.t（无 w 列）→ G1 必须 ENV-0021，
    // 证明守卫跟随 search_path 消解而非合并两 schema 的列清单。
    og.exec_script("SET search_path TO sd_xs2").unwrap();
    let err = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Append { where_clause: None },
            fx.dir.path(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("ENV-0021"), "{err}");
    assert_eq!(gauss_count(&mut og, "sd_xs2.t"), 7, "守卫失败不得写入");
}

/// upsert 撞次级唯一索引：UPDATE 违反 UNIQUE(w) → 整事务回滚，目标原样。
#[test]
fn should_rollback_upsert_when_secondary_unique_index_violated() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_sec");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_sec; \
         CREATE TABLE sd_e2e_sec(id int PRIMARY KEY, v int, w text, \
           CONSTRAINT sd_e2e_sec_w UNIQUE(w)); \
         INSERT INTO sd_e2e_sec VALUES (1,10,'a'),(2,20,'b');",
    )
    .unwrap();
    // staging 把 id=1 的 w 改成 'b'（撞 id=2）+ 新增 id=3
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_sec AS \
             SELECT 1 AS id, 11 AS v, 'b' AS w \
             UNION ALL SELECT 3 AS id, 30 AS v, 'c' AS w",
            &BindMap::new(),
        )
        .unwrap();
    assert!(
        og.sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .is_err(),
        "次级唯一索引冲突必须报错"
    );
    assert_eq!(gauss_count(&mut og, "sd_e2e_sec"), 2, "冲突必须整体回滚");
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_sec WHERE id=1"),
        "a",
        "UPDATE 不得留下半更新"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT count(*) FROM sd_e2e_sec WHERE id=3"),
        "0",
        "INSERT 不得在 UPDATE 失败后单独生效"
    );
}

/// upsert 重跑幂等：第二次全部命中走 UPDATE，终态与首次一致。
#[test]
fn should_upsert_rerun_is_idempotent() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_idem");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_idem; \
         CREATE TABLE sd_e2e_idem(id int PRIMARY KEY, v int, w text); \
         INSERT INTO sd_e2e_idem SELECT i, i*10, 'old' FROM generate_series(1,10) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_idem AS \
             SELECT i AS id, 999 AS v, 'new' AS w FROM range(1,6) t(i) \
             UNION ALL SELECT i, i, 'keep' FROM range(11,16) t(i)",
            &BindMap::new(),
        )
        .unwrap();
    let mode = || SyncMode::Upsert {
        keys: vec!["id".into()],
    };
    let oc1 = og
        .sync_write(&fx.duck, fx.table, &cols(), &mode(), fx.dir.path())
        .unwrap();
    assert_eq!(oc1.rows, 10);
    let oc2 = og
        .sync_write(&fx.duck, fx.table, &cols(), &mode(), fx.dir.path())
        .unwrap();
    assert_eq!(oc2.rows, 10, "重跑仍 UPDATE+INSERT=10（全命中走 UPDATE）");
    assert_eq!(gauss_count(&mut og, "sd_e2e_idem"), 15, "重跑不得重复插入");
    assert_eq!(
        gauss_cell(&mut og, "SELECT w FROM sd_e2e_idem WHERE id=3"),
        "new"
    );
}

/// G3 e2e：staging upsert key 含 NULL → BIZ-0103，目标零写入。
#[test]
fn should_reject_upsert_when_staging_key_is_null() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_nkey");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_nkey; \
         CREATE TABLE sd_e2e_nkey(id int PRIMARY KEY, v int, w text); \
         INSERT INTO sd_e2e_nkey SELECT i, i, 'k' FROM generate_series(1,3) i;",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_nkey AS \
             SELECT NULL::INTEGER AS id, 1 AS v, 'n' AS w \
             UNION ALL SELECT 5 AS id, 2 AS v, 'n' AS w",
            &BindMap::new(),
        )
        .unwrap();
    let err = og
        .sync_write(
            &fx.duck,
            fx.table,
            &cols(),
            &SyncMode::Upsert {
                keys: vec!["id".into()],
            },
            fx.dir.path(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("BIZ-0103"), "{err}");
    assert_eq!(gauss_count(&mut og, "sd_e2e_nkey"), 3, "守卫失败不得写入");
}

/// 宽类型保真：DECIMAL/DATE/TIMESTAMP 经 CSV 中转不失真；
/// A 模式 '' 即 NULL——文本列空串落库为 NULL（方言语义，非 sync 缺陷）。
#[test]
fn should_preserve_wide_types_on_append() {
    let mut og = require_og();
    let fx = fixture("sd_e2e_wide");
    og.exec_script(
        "DROP TABLE IF EXISTS sd_e2e_wide; \
         CREATE TABLE sd_e2e_wide(k int PRIMARY KEY, d decimal(10,2), \
           dt date, ts timestamp, txt text);",
    )
    .unwrap();
    fx.duck
        .execute(
            "CREATE TABLE sd_e2e_wide AS \
             SELECT 1 AS k, 1.50::DECIMAL(10,2) AS d, \
               DATE '2024-01-02' AS dt, TIMESTAMP '2024-01-02 03:04:05' AS ts, \
               'hello' AS txt \
             UNION ALL SELECT 2, 2.25::DECIMAL(10,2), \
               DATE '2024-12-31', TIMESTAMP '2024-12-31 23:59:58', ''",
            &BindMap::new(),
        )
        .unwrap();
    let wcols: Vec<String> = ["k", "d", "dt", "ts", "txt"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let oc = og
        .sync_write(
            &fx.duck,
            fx.table,
            &wcols,
            &SyncMode::Append { where_clause: None },
            fx.dir.path(),
        )
        .unwrap();
    assert_eq!(oc.rows, 2);
    // A 模式 DATE 含时间分量（Oracle 语义），::text 呈现为 'YYYY-MM-DD 00:00:00'
    assert_eq!(
        gauss_cell(
            &mut og,
            "SELECT d::text||'|'||dt::text||'|'||ts::text FROM sd_e2e_wide WHERE k=1"
        ),
        "1.50|2024-01-02 00:00:00|2024-01-02 03:04:05"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT d::text FROM sd_e2e_wide WHERE k=2"),
        "2.25"
    );
    // A 模式：'' 即 NULL——空串文本列落库必须为 NULL（方言语义锁定）
    assert_eq!(
        gauss_cell(
            &mut og,
            "SELECT txt IS NULL::text FROM sd_e2e_wide WHERE k=2"
        ),
        "true"
    );
    assert_eq!(
        gauss_cell(&mut og, "SELECT txt FROM sd_e2e_wide WHERE k=1"),
        "hello"
    );
}
