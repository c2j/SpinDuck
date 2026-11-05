#![cfg(feature = "gauss")]
//! 部署脚手架（迭代用；成熟后固化为 golden 用例）。
use spinduck_core::io::GaussConn;
use std::path::{Path, PathBuf};

fn og() -> GaussConn {
    GaussConn::connect_parts(
        &std::env::var("SPINDUCK_OG_HOST").unwrap(),
        std::env::var("SPINDUCK_OG_PORT").unwrap().parse().unwrap(),
        "gaussdb",
        &std::env::var("SPINDUCK_OG_PASSWORD").unwrap(),
        "postgres",
    )
    .unwrap()
}

fn base() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("lib/fastaas/exam/清算拆分优化考题/基线代码")
}

/// 按 `/` 行拆分语句块（Oracle 风格分隔符；无分隔符文件整体一块）。
fn split_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.trim() == "/" {
            let c = cur.trim();
            if !c.is_empty() {
                chunks.push(c.to_string());
            }
            cur.clear();
        } else {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    let c = cur.trim();
    if !c.is_empty() {
        chunks.push(c.to_string());
    }
    chunks
}

#[test]
fn deploy_closure_and_run_native() {
    let mut og = og();
    let dirs = [
        "1_表结构",
        "2_序列",
        "3_视图",
        "4_外部依赖包_桩",
        "5_函数",
        "6_子过程",
    ];
    let mut ok = 0usize;
    let mut fails: Vec<(String, String)> = Vec::new();
    for d in dirs {
        let dir = base().join(d);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
            .collect();
        files.sort();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            for (i, chunk) in split_chunks(&text).into_iter().enumerate() {
                let tag = format!("{}/{}#{}", d, f.file_name().unwrap().to_string_lossy(), i);
                match og.exec_script(&chunk) {
                    Ok(()) => ok += 1,
                    Err(e) => fails.push((tag, e.message.lines().next().unwrap_or("").to_string())),
                }
            }
        }
    }
    // 核心包：先 step2（前置链），再 step3（目标）
    for pkg in [
        "7_核心存储过程包/PKG_SPLIT_TRADE_STEP2.sql",
        "7_核心存储过程包/PKG_SPLIT_TRADE_STEP3_SH.sql",
    ] {
        let text = std::fs::read_to_string(base().join(pkg)).unwrap();
        for (i, chunk) in split_chunks(&text).into_iter().enumerate() {
            let tag = format!("{}#{}", pkg, i);
            match og.exec_script(&chunk) {
                Ok(()) => ok += 1,
                Err(e) => fails.push((tag, e.message.lines().next().unwrap_or("").to_string())),
            }
        }
    }
    println!("DEPLOY ok={ok} fail={}", fails.len());
    for (tag, msg) in fails.iter().take(40) {
        println!("FAIL {tag}: {msg}");
    }
}
