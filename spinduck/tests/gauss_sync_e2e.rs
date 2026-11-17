//! sync 回写端到端（openGauss 容器实测）：守卫族 + 三模式冲突处理。
//! 门控：feature gauss + 环境变量 SPINDUCK_OG_HOST（缺省跳过——跳过不等于通过）。
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

/// 每测试独立 DuckDB 文件 + 独立 GaussDB 表名（并行测试互不干扰）。
struct Fixture {
    dir: tempfile::TempDir,
    duck: DuckExecutor,
    table: &'static str,
}

fn fixture(table: &'static str) -> Option<Fixture> {
    let dir = tempfile::tempdir().ok()?;
    let db = dir.path().join("local.duckdb");
    let duck = DuckExecutor::open(&db.to_string_lossy()).ok()?;
    Some(Fixture { dir, duck, table })
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_app") else {
        return;
    };
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_upk") else {
        return;
    };
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_nopk") else {
        return;
    };
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_uq") else {
        return;
    };
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_dup") else {
        return;
    };
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_sl") else {
        return;
    };
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
    let Some(mut og) = og() else { return };
    let Some(fx) = fixture("sd_e2e_conf") else {
        return;
    };
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
    assert!(
        gauss_count(&mut og, "sd_e2e_conf") == 3,
        "PK 冲突必须整体回滚：{err}"
    );
}
