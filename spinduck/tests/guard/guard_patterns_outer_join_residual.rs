//! 行为锁定（AGENTS §4.3）：`(+)` 外连接标记的**不适配形态原样保留**这一现状。
//!
//! 改写器只覆盖"逗号 FROM + 末表带标记"，其余形态**不剥标记**（保守边界，见
//! `src/sql/patterns.rs` 的 `apply_on_from_where`）。本守护锁定两件事，供
//! docs/validation.md V27 的交付件命中面结论有可复现依据：
//! 1. 不适配形态的改写产物里 `(+)` 仍在（不静默改语义、也不静默丢标记）；
//! 2. 该产物交给进程内内核执行是**显式失败**，不是返回空结果——即"没改写"的后果
//!    是可诊断的错误，而非静默错值。

#![cfg(feature = "native")]

use spinduck_core::exec::{BindMap, SqlExecutor};
use spinduck_core::rules::RuleRegistry;
use spinduck_core::sql::{apply_text_patterns, DuckExecutor};

/// 标记侧不在 FROM 清单末位：`LEFT JOIN` 只能接链尾，插在中间会改连接顺序语义。
#[test]
fn should_keep_marker_when_marker_side_is_not_last() {
    let got = apply_text_patterns(
        "SELECT a.id, b.id, c.id FROM ta a, tb b, tc c WHERE a.id = b.id(+)",
        &RuleRegistry::default(),
    )
    .unwrap();
    assert!(
        !got.to_ascii_uppercase().contains("JOIN"),
        "不该产出 JOIN: {got}"
    );
    assert!(
        got.contains("(+)"),
        "标记必须原样保留（不是被剥掉后丢语义）: {got}"
    );
}

/// 两侧各有标记（Oracle 的 FULL 语义写法）：一行 LEFT JOIN 表达不了，不改写。
#[test]
fn should_keep_marker_when_both_sides_marked() {
    let got = apply_text_patterns(
        "SELECT a.id FROM ta a, tb b WHERE a.id = b.id(+) AND b.x = a.y(+)",
        &RuleRegistry::default(),
    )
    .unwrap();
    assert!(!got.to_ascii_uppercase().contains("JOIN"), "{got}");
    assert!(got.contains("(+)"), "标记必须原样保留: {got}");
}

/// 保留下来的 `(+)` 走内核必须是显式失败：这条把"没改写 = 可诊断错误"钉住，
/// 防止日后漂成"改写产物含标记、内核却静默返回空行集"。
///
/// 夹具表列名与语句里的引用对齐（`ta/tb/tc` 各有 `id`），使失败原因只能归给标记本身；
/// 对照组把同一条语句换成合法 JOIN，必须能执行——否则本用例证明的是夹具问题而非方言问题。
#[test]
fn should_fail_loudly_in_kernel_on_residual_marker() {
    let sql = apply_text_patterns(
        "SELECT a.id, b.id, c.id FROM ta a, tb b, tc c WHERE a.id = b.id(+)",
        &RuleRegistry::default(),
    )
    .unwrap();
    assert!(
        sql.contains("(+)"),
        "前置条件：本用例执行的是未改写文本: {sql}"
    );

    let ex = DuckExecutor::open_in_memory().unwrap();
    for t in ["ta", "tb", "tc"] {
        ex.execute(&format!("CREATE TABLE {t}(id INTEGER)"), &BindMap::new())
            .unwrap();
    }
    // 对照组：同一形状的合法 JOIN 必须过
    let mut ok = ex
        .query(
            "SELECT a.id, b.id, c.id FROM ta a JOIN tb b ON a.id = b.id, tc c",
            &BindMap::new(),
        )
        .expect("对照组（合法 JOIN）被拒，说明夹具/接缝有问题，本用例结论不成立");
    let _ = ok.fetch_many(1);

    let err = match ex.query(&sql, &BindMap::new()) {
        Ok(_) => {
            panic!("含 (+) 的文本竟被内核接受：\"未改写 → 显式报错\"的预期被实测否掉，须重评 §8")
        }
        Err(e) => e,
    };
    // 实测（登记 docs/validation.md V27.2）：内核报 `Parser Error: syntax error at or
    // near ")"`，且错误文本回显出错位置（含 `b.id(+)`）——"未改写"的后果是可诊断的
    // 解析错，不是静默空行集。断言按这一实测口径收紧，内核升级若改文案即红（V6 门禁复核）。
    let msg = err.message;
    assert!(
        msg.to_ascii_lowercase().contains("parser error"),
        "失败须落在内核 Parser Error 类，实际: {msg}"
    );
    assert!(
        msg.contains("(+)"),
        "错误须回显出错位置，能定位到标记本身: {msg}"
    );
}
