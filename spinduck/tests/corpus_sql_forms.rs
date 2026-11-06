//! 语料 SQL 形态覆盖测试（**进程内、零外部依赖**，AGENTS §4.1 探针/集成层）。
//!
//! 口径（2026-11-06 决策）：考题业务函数**不执行**——本文件不依赖考题目录，
//! 而是把语料里出现过的每一种 SQL/PL 语句形态用内联 SQL 复刻一遍，逐形态断言。
//! 形态清单来自 `config/corpus_forms.toml`（由 `scripts/extract_corpus_forms.py`
//! 从语料抽取、脱敏后入库），并用同一份清单做**覆盖门禁**：
//! 清单里有命中的构造必须有断言，零命中的构造必须落在拒绝/未覆盖面。
//!
//! 两个断言维度：改写产物文本 + 进程内 DuckDB 1.5.5 执行结果。
//! 探针模式（取实测值用，不参与门禁语义）：
//! `FORMS_PROBE=1 cargo test --features native --test corpus_sql_forms -- --nocapture`

mod corpus_forms;

#[path = "corpus_forms/dml.rs"]
mod dml;
#[path = "corpus_forms/expr.rs"]
mod expr;
#[path = "corpus_forms/pl_fn.rs"]
mod pl_fn;
#[path = "corpus_forms/pl_proc.rs"]
mod pl_proc;
#[path = "corpus_forms/query.rs"]
mod query;

use corpus_forms::{check, check_contains, fmt_rows, fmt_want, Kernel};
use std::collections::BTreeSet;
use std::path::Path;

/// 标量维度：`SELECT <形态>` 改写后在进程内内核执行，逐值比对。
#[test]
fn scalar_forms_evaluate_on_kernel() {
    let k = Kernel::new();
    let mut bad = Vec::new();
    for f in expr::SCALARS {
        let sql = f.sql;
        check(f.key, f.want, || k.scalar(sql), &mut bad);
    }
    assert!(bad.is_empty(), "标量形态断言失败：\n{}", bad.join("\n"));
}

/// 改写维度：方言映射产物（规则值的可执行证据）。
#[test]
fn rewrite_forms_match_dialect_targets() {
    let k = Kernel::new();
    let mut bad = Vec::new();
    for f in expr::REWRITES {
        let (sql, want) = (f.sql, f.want);
        check_contains(
            f.key,
            want,
            || k.rewrite(sql).unwrap_or_else(|e| panic!("{e}")),
            &mut bad,
        );
    }
    assert!(bad.is_empty(), "改写形态断言失败：\n{}", bad.join("\n"));
}

#[test]
fn query_forms_return_expected_rows() {
    let k = Kernel::new();
    let mut bad = Vec::new();
    for f in query::QUERIES {
        let (sql, want) = (f.sql, f.want);
        check(f.key, &fmt_want(want), || fmt_rows(&k.rows(sql)), &mut bad);
    }
    assert!(bad.is_empty(), "查询形态断言失败：\n{}", bad.join("\n"));
}

#[test]
fn dml_and_ddl_forms_write_expected_state() {
    let mut bad = Vec::new();
    for f in dml::STATEMENTS {
        let k = Kernel::new();
        let (setup, stmt, check_sql, want) = (f.setup, f.stmt, f.check, f.want);
        check(
            f.key,
            &fmt_want(want),
            || {
                k.script(setup);
                k.exec(stmt);
                fmt_rows(&k.rows(check_sql))
            },
            &mut bad,
        );
    }
    assert!(bad.is_empty(), "DML/DDL 形态断言失败：\n{}", bad.join("\n"));
}

/// PL 函数形态：注册 → 调用 → 返回值逐值比对；`ERR:` 前缀声明期望失败。
#[test]
fn pl_function_forms_return_expected_values() {
    let mut bad = Vec::new();
    for f in pl_fn::FNS {
        let mut eng = corpus_forms::engine();
        let (routine, call, args, want) = (f.routine, f.call, f.args, f.want);
        check(
            f.key,
            want,
            || {
                eng.register(routine)
                    .unwrap_or_else(|e| panic!("注册失败: {}", e.message));
                match want.strip_prefix("ERR:") {
                    Some(needle) => match corpus_forms::try_call(&mut eng, call, args) {
                        Err(e) => {
                            assert!(
                                e.message.contains(needle),
                                "错误消息不含 {needle:?}: {}",
                                e.message
                            );
                            format!("ERR:{needle}")
                        }
                        Ok(_) => panic!("{} 期望失败（ERR: 前缀）", call),
                    },
                    None => corpus_forms::norm_scalar(
                        corpus_forms::call(&mut eng, call, args).return_value,
                    ),
                }
            },
            &mut bad,
        );
    }
    assert!(bad.is_empty(), "PL 函数形态断言失败：\n{}", bad.join("\n"));
}

/// PL 过程/包形态：注册 → 调用 → 表状态 + OUT 参 / 返回值断言。
#[test]
fn pl_procedure_forms_write_expected_state() {
    let mut bad = Vec::new();
    for f in pl_proc::ROUTINES {
        let mut eng = corpus_forms::engine();
        let (routine, call, args) = (f.routine, f.call, f.args);
        let (check_sql, want, out_param, want_out) = (f.check, f.want, f.out_param, f.want_out);
        check(
            f.key,
            &fmt_want(want),
            || {
                eng.register(routine)
                    .unwrap_or_else(|e| panic!("注册失败: {}", e.message));
                let r = corpus_forms::call(&mut eng, call, args);
                if !want_out.is_empty() {
                    let got = if out_param.is_empty() {
                        r.return_value
                            .map(|v| corpus_forms::norm(Some(v.as_str_loose())))
                            .unwrap_or_else(|| "NULL".to_string())
                    } else {
                        r.outs
                            .get(out_param)
                            .map(|v| corpus_forms::norm(Some(v.as_str_loose())))
                            .unwrap_or_else(|| panic!("无 OUT 参 {out_param}"))
                    };
                    assert_eq!(got, want_out, "OUT/返回口径不符");
                }
                if check_sql.is_empty() {
                    return String::new();
                }
                fmt_rows(&corpus_forms::query(&eng, check_sql))
            },
            &mut bad,
        );
    }
    assert!(bad.is_empty(), "PL 过程形态断言失败：\n{}", bad.join("\n"));
}

/// 覆盖门禁：清单里有命中的构造与测试表登记的构造**必须一一对应**。
#[test]
fn every_corpus_construct_has_an_assertion() {
    let manifest = manifest_constructs();
    let covered: BTreeSet<String> = expr::SCALARS
        .iter()
        .map(|f| f.key.to_string())
        .chain(expr::REWRITES.iter().map(|f| f.key.to_string()))
        .chain(query::QUERIES.iter().map(|f| f.key.to_string()))
        .chain(dml::STATEMENTS.iter().map(|f| f.key.to_string()))
        .chain(pl_fn::FNS.iter().map(|f| f.key.to_string()))
        .chain(pl_proc::ROUTINES.iter().map(|f| f.key.to_string()))
        .collect();
    let missing: Vec<&String> = manifest.iter().filter(|k| !covered.contains(*k)).collect();
    let extra: Vec<&String> = covered.iter().filter(|k| !manifest.contains(*k)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "形态覆盖与清单不一致（清单 {} 项 / 已断言 {} 项）\n  清单有、测试无: {:?}\n  测试有、清单无: {:?}",
        manifest.len(),
        covered.len(),
        missing,
        extra
    );
}

/// 拒绝面门禁：`reject_*` 构造必须在语料零命中清单里（命中即需重新评审 `[rejects]`）。
#[test]
fn reject_faces_are_absent_from_corpus() {
    let zero = manifest_zero_hits();
    let rejects: Vec<&String> = zero.iter().filter(|k| k.starts_with("reject_")).collect();
    assert!(
        rejects.len() >= 4,
        "清单的拒绝面条目异常（应含 distribute/rowtype/partition/ddms 等零命中项）: {rejects:?}"
    );
}

/// 语料序列 DDL 逐字（`sql/ddl/seq_*.sql`，去掉注释头）——含内核不支持的两个选项。
const CORPUS_SEQ_DDL: &str = "CREATE SEQUENCE seq_dispatch_job_id\n    \
    START WITH 20001\n    INCREMENT BY 1\n    MINVALUE 20001\n    \
    MAXVALUE 999999999999999999999999999\n    NO CYCLE\n    CACHE 1";

/// 部署期归一化：语料逐字序列 DDL 经 `clamp_sequence_bounds` + `drop_cache_option`
/// 后可在进程内内核建出来并正确取号；`START WITH`/`MINVALUE`/`NO CYCLE` 一字不改。
/// （`ddl_sequence_start_with` 形态里 CACHE 剥离即由此断言，见 dml.rs 的 why。）
#[test]
fn deploy_path_strips_corpus_sequence_options() {
    use spinduck_core::verify::{clamp_sequence_bounds, drop_cache_option, sql_units};
    let k = Kernel::new();
    let units = sql_units(CORPUS_SEQ_DDL);
    assert_eq!(units.len(), 1, "语料序列 DDL 应为单语句：{units:?}");
    let text = drop_cache_option(&clamp_sequence_bounds(&units[0]));
    for keep in [
        "START WITH 20001",
        "INCREMENT BY 1",
        "MINVALUE 20001",
        "NO CYCLE",
    ] {
        assert!(text.contains(keep), "支持的子句被改动: {text}");
    }
    for cut in ["CACHE", "MAXVALUE 999"] {
        assert!(
            !text.contains(cut),
            "内核不支持的选项未剥离（{cut}）: {text}"
        );
    }
    k.exec(&text);
    assert_eq!(
        k.rows("SELECT seq_dispatch_job_id.nextval FROM sys_dummy"),
        vec![vec!["20001".to_string()]],
        "剥离后取号仍从 START WITH 起"
    );
    // 非序列语句一字不改（表名/列名里含 cache 也不算）
    assert_eq!(
        drop_cache_option("CREATE TABLE cache_t (a INT)"),
        "CREATE TABLE cache_t (a INT)"
    );
}

fn manifest_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/corpus_forms.toml")
}

/// `[constructs]` 段键名（有命中的构造）。
fn manifest_constructs() -> BTreeSet<String> {
    section_keys(
        &std::fs::read_to_string(manifest_path()).expect("读 config/corpus_forms.toml"),
        "constructs",
    )
}

/// `[zero_hits]` 段 `keys = ["a", "b", …]` 的字符串项。
fn manifest_zero_hits() -> BTreeSet<String> {
    let text = std::fs::read_to_string(manifest_path()).expect("读 config/corpus_forms.toml");
    let mut inside = false;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            inside = l == "[zero_hits]";
            continue;
        }
        if inside {
            if let Some(list) = l.strip_prefix("keys = [").and_then(|s| s.strip_suffix(']')) {
                for item in list.split(',') {
                    let t = item.trim().trim_matches('"');
                    if !t.is_empty() {
                        out.insert(t.to_string());
                    }
                }
            }
        }
    }
    out
}

/// TOML 段落键名（行式解析：清单形态固定，不需要完整 TOML 解析器）。
fn section_keys(text: &str, section: &str) -> BTreeSet<String> {
    let mut inside = false;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            inside = l == format!("[{section}]");
            continue;
        }
        if inside {
            if let Some((k, _)) = l.split_once('=') {
                let k = k.trim();
                if !k.is_empty() {
                    out.insert(k.to_string());
                }
            }
        }
    }
    out
}
