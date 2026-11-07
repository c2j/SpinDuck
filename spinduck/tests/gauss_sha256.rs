//! sha256 口令认证端到端探针（§13 数据通路 / V24）。
//!
//! 与 [`gauss_e2e`](gauss_e2e.rs) 的分工：那套跑 golden 容器（5433，为兼容旧驱动
//! 特意开了 md5），本探针只跑**默认 sha256**（`password_encryption_type = 2`）的
//! 实例（5434），用来锁定"纯 Rust 客户端能否直连 GaussDB/openGauss 默认口令形态"。
//!
//! 门禁口径（AGENTS §4.1 内核集成层禁止"跳过即绿"，与 gauss_e2e 同规格）：
//! - **feature 级**：不打开 `gauss-sha256` 时本文件收集 0 个测试（`0 passed`），
//!   如实显示"未纳入本次运行"——这是唯一的"不跑"入口；
//! - **env 级**：打开 feature 即视为要求执行，缺 `SPINDUCK_OG_SHA256_PW_FILE`/
//!   `SPINDUCK_OG_SHA256_HOST`、口令文件读不到、或实例连不上都**显式变红**。
//!
//! 复现（compose 自带 `gaussdb-sha256` 服务，口令由容器 env 落进 0400 文件，
//! 不经命令行、不入库；compose 本身要 `GS_PASSWORD` 插值，先 `cp .env.example .env`
//! 或在同一条 shell 里导出，否则报 "GS_PASSWORD must be set"）：
//! ```text
//! docker compose -f golden/gaussdb-docker/docker-compose.yml up -d gaussdb-sha256
//! umask 077 && docker inspect sd-og-sha256 \
//!   --format '{{range .Config.Env}}{{println .}}{{end}}' \
//!   | sed -n 's/^GS_PASSWORD=//p' > /tmp/sd_sha256_pw    # 0400，用后删除
//! SPINDUCK_OG_SHA256_PW_FILE=/tmp/sd_sha256_pw \
//!   cargo test --features gauss-sha256 --test gauss_sha256
//! ```

#![cfg(feature = "gauss-sha256")]

use spinduck_core::io::GaussConn;

/// 口令只从文件读（0400），避免出现在进程命令行与 shell 历史里。
fn password(path: &str) -> String {
    let raw = std::fs::read_to_string(path).expect("读取 sha256 口令文件失败");
    raw.trim_end_matches('\n').to_string()
}

/// 两级门控后的连接。缺配置**直接红**，与 gauss_e2e"缺容器必红"同口径（AGENTS §4.1
/// 禁止"跳过即绿"）；不想跑这一层就不要开 `gauss-sha256` feature（第 3 级门控）。
fn sha256_conn() -> GaussConn {
    let pw_file = std::env::var("SPINDUCK_OG_SHA256_PW_FILE").unwrap_or_else(|_| {
        panic!(
            "本探针需要默认 sha256 口径实例：\n\
             docker compose -f golden/gaussdb-docker/docker-compose.yml up -d gaussdb-sha256\n\
             然后把容器 env 里的口令落到 0400 文件，并设 SPINDUCK_OG_SHA256_PW_FILE=/tmp/sd_sha256_pw\n\
             （不需要本层时，用不带 gauss-sha256 feature 的命令，而不是让这里假绿）"
        )
    });
    GaussConn::connect_parts(
        &std::env::var("SPINDUCK_OG_SHA256_HOST")
            .expect("必须提供 SPINDUCK_OG_SHA256_HOST（默认 127.0.0.1）"),
        std::env::var("SPINDUCK_OG_SHA256_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5434),
        &std::env::var("SPINDUCK_OG_SHA256_USER").unwrap_or_else(|_| "gaussdb".into()),
        &password(&pw_file),
        &std::env::var("SPINDUCK_OG_SHA256_DB").unwrap_or_else(|_| "postgres".into()),
    )
    .expect("连接默认 sha256 口径的 openGauss 实例（已配置即必须连上，否则为失败）")
}

/// 默认 sha256 实例必须可连接，且能取回行（驱动侧认证 + 简单查询协议）。
#[test]
fn should_connect_and_fetch_rows_on_sha256_auth_instance() {
    let mut g = sha256_conn();
    let (schema, rows) = g
        .fetch_rows("SELECT 1 AS one, 'sd' AS tag")
        .expect("sha256 实例上 simple_query 应成功");
    assert_eq!(schema, vec!["one".to_string(), "tag".to_string()]);
    assert_eq!(
        rows,
        vec![vec![Some("1".to_string()), Some("sd".to_string())]],
        "取回值仍是文本承载（§13.1 口径不变）"
    );
}

/// 参照侧口令形态确认：`password_encryption_type` 不能是 md5-only（0/1）。
/// 若实例被改回 md5 口径，本探针证明的就不是 sha256 —— 必须红，不能假绿。
#[test]
fn should_refuse_to_claim_sha256_when_instance_is_md5_only() {
    let mut g = sha256_conn();
    let (_, rows) = g
        .fetch_rows("SHOW password_encryption_type")
        .expect("SHOW 参数应成功");
    let value = rows
        .first()
        .and_then(|r| r.first())
        .and_then(|o| o.as_ref())
        .cloned()
        .unwrap_or_default();
    assert!(
        value == "2" || value == "3",
        "被测面必须是 sha256 参与（2=sha256only，3=sha256+md5），实测 {value}"
    );
}

/// DDL/DML 通道同样可用（`materialize`/`sync_append` 走的是 execute）。
#[test]
fn should_execute_ddl_and_dml_on_sha256_auth_instance() {
    let mut g = sha256_conn();
    g.exec("DROP TABLE IF EXISTS sd_probe_sha256")
        .expect("清理残留表");
    g.exec("CREATE TABLE sd_probe_sha256(a INT, b TEXT)")
        .expect("建表");
    let n = g
        .exec("INSERT INTO sd_probe_sha256 VALUES (7, 'seven')")
        .expect("插入");
    assert_eq!(n, 1, "回写通道以受影响行数计");
    let (_, rows) = g
        .fetch_rows("SELECT b FROM sd_probe_sha256 WHERE a = 7")
        .expect("读回");
    assert_eq!(rows, vec![vec![Some("seven".to_string())]]);
    g.exec("DROP TABLE sd_probe_sha256").expect("清理");
}
