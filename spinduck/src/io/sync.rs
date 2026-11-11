//! sync 回写通道（docs/sync-writeback-design.md）：DuckDB `COPY TO` CSV 中转快
//! 路径 + append/slice/upsert 三模式 + 前置守卫族（G1 schema / G2 slice /
//! G3 key / G4 目标唯一索引）。缺省行为与 V26 `sync_append` 逐字一致。

use crate::error::EngineError;
use crate::exec::{BindMap, RowStream, SqlExecutor};
use crate::value::SpinValue;

/// ENV-0016：`--slice` 值引用的环境变量未设置。
pub(crate) const ENV_NO_ENV_REF: u16 = 16;
/// ENV-0021：目标表 schema 与 `--cols` 不兼容（漂移守卫 G1）。
pub(crate) const ENV_SCHEMA_DRIFT: u16 = 21;
/// ENV-0022：目标表 upsert key 无唯一/PK 索引（守卫 G4）。
pub(crate) const ENV_NO_UNIQUE_KEY: u16 = 22;
/// BIZ-0102：staging 存在越界 slice 的行（守卫 G2）。
pub(crate) const BIZ_SLICE_VIOLATION: u16 = 102;
/// BIZ-0103：staging upsert key 缺失/含 NULL/不唯一（守卫 G3）。
pub(crate) const BIZ_KEY_VIOLATION: u16 = 103;
/// upsert 模式的目标侧 staging 临时表名（同事务内建/删）。
pub(crate) const TMP_TABLE: &str = "sd_sync_tmp";

/// slice 模式单条谓词：`col = value`（值已含环境变量展开结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicePredicate {
    /// 谓词列名（原样标识符，目录声明口径）。
    pub col: String,
    /// 谓词字面量值（单引号转义在拼 SQL 时统一做）。
    pub value: String,
}

/// 回写模式（设计 §4：读取过滤与目标侧语义绑定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMode {
    /// 追加（V26 契约）；`--where` 为可选读取过滤。
    Append {
        /// 可选 WHERE（拼进读取 SELECT）。
        where_clause: Option<String>,
    },
    /// 同事务先 DELETE slice 再 COPY（幂等重跑）。
    Slice {
        /// 结构化等值谓词。
        predicates: Vec<SlicePredicate>,
    },
    /// 键控合并：staging 临时表 + UPDATE FROM + INSERT NOT EXISTS。
    Upsert {
        /// 键列（`--cols` 真子集）。
        keys: Vec<String>,
    },
}

impl SyncMode {
    /// stdout 契约中的模式标签。
    pub fn label(&self) -> &'static str {
        match self {
            SyncMode::Append { .. } => "append",
            SyncMode::Slice { .. } => "slice",
            SyncMode::Upsert { .. } => "upsert",
        }
    }
}

/// 一次回写的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    /// 目标侧生效行数（append/slice = COPY 行数；upsert = UPDATE+INSERT）。
    pub rows: usize,
    /// 模式标签（stdout 契约）。
    pub mode_label: &'static str,
}

/// 解析 `--slice k=v[,k=v]`（纯语法；不含环境变量展开）。
/// 空段跳过；缺 `=` 或列名为空 = 用法红。
pub fn parse_slice(raw: &str) -> Result<Vec<SlicePredicate>, EngineError> {
    let mut out = Vec::new();
    for seg in raw.split(',') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let (col, value) = seg.split_once('=').ok_or_else(|| {
            crate::error::EngineError::env(10, format!("--slice 谓词必须是 k=v 形态，得到 {seg:?}"))
        })?;
        let col = col.trim();
        if col.is_empty() {
            return Err(crate::error::EngineError::env(
                10,
                format!("--slice 谓词列名不能为空，得到 {seg:?}"),
            ));
        }
        out.push(SlicePredicate {
            col: col.to_string(),
            value: value.to_string(),
        });
    }
    if out.is_empty() {
        return Err(crate::error::EngineError::env(
            10,
            "--slice 至少需要一个 k=v 谓词",
        ));
    }
    Ok(out)
}

/// 展开 `$NAME` / `${NAME}` 环境变量引用（其余字符原样保留）。
/// 未设置的变量 = ENV-0016 红（fail-closed，不静默替换空串）。
pub fn expand_env_refs(text: &str) -> Result<String, EngineError> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let rest = &text[i + 1..];
        let (name, consumed) = if let Some(stripped) = rest.strip_prefix('{') {
            let end = stripped.find('}').ok_or_else(|| {
                EngineError::env(ENV_NO_ENV_REF, format!("${{ 缺少配对 }}：{text}"))
            })?;
            (&stripped[..end], end + 2)
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (&rest[..end], end + 1)
        };
        if name.is_empty()
            || !name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            out.push('$');
            continue;
        }
        let val = std::env::var(name).map_err(|_| {
            EngineError::env(
                ENV_NO_ENV_REF,
                format!("环境变量 {name} 未设置（--slice 值引用 ${name}）"),
            )
        })?;
        out.push_str(&val);
        for _ in 0..consumed.saturating_sub(1) {
            chars.next();
        }
    }
    Ok(out)
}

/// 解析 slice 原始串为谓词列表（语法 + 环境变量展开）。
pub fn resolve_slice(raw: &str) -> Result<Vec<SlicePredicate>, EngineError> {
    let raw = expand_env_refs(raw)?;
    let parsed = parse_slice(&raw)?;
    parsed
        .iter()
        .map(|p| {
            let value = expand_env_refs(&p.value)?;
            Ok(SlicePredicate {
                col: p.col.clone(),
                value,
            })
        })
        .collect()
}

/// CLI 原始旗标 → 回写模式（usage 层已校验成对性，这里负责引擎侧解析）。
pub fn resolve_mode(
    mode_raw: Option<&str>,
    slice_raw: Option<&str>,
    key_raw: Option<&str>,
    where_raw: Option<&str>,
) -> Result<SyncMode, EngineError> {
    match mode_raw {
        None => Ok(SyncMode::Append {
            where_clause: where_raw.map(str::to_string),
        }),
        Some("append") => Ok(SyncMode::Append {
            where_clause: where_raw.map(str::to_string),
        }),
        Some("slice") => {
            let raw = slice_raw.ok_or_else(|| EngineError::env(10, "--mode slice 需要 --slice"))?;
            Ok(SyncMode::Slice {
                predicates: resolve_slice(raw)?,
            })
        }
        Some("upsert") => {
            let raw = key_raw.ok_or_else(|| EngineError::env(10, "--mode upsert 需要 --key"))?;
            let keys = raw
                .split(',')
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect::<Vec<_>>();
            if keys.is_empty() {
                return Err(EngineError::env(10, "--key 至少需要一列"));
            }
            Ok(SyncMode::Upsert { keys })
        }
        Some(other) => Err(EngineError::env(
            10,
            format!("--mode 取值须为 append|slice|upsert，得到 {other}"),
        )),
    }
}

/// slice 谓词 → WHERE 片段（值单引号转义，AND 连接）。
pub fn slice_where(preds: &[SlicePredicate]) -> String {
    preds
        .iter()
        .map(|p| format!("{} = '{}'", p.col, p.value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// G3 前置：upsert key 必须是 `--cols` 真子集。
pub fn validate_keys_subset(keys: &[String], cols: &[String]) -> Result<(), EngineError> {
    let missing: Vec<&String> = keys
        .iter()
        .filter(|k| !cols.iter().any(|c| c.eq_ignore_ascii_case(k)))
        .collect();
    if !missing.is_empty() {
        return Err(EngineError::business(
            BIZ_KEY_VIOLATION,
            format!("--key 列不在 --cols 中：{missing:?}（cols={cols:?}）"),
        ));
    }
    if keys.len() >= cols.len() {
        return Err(EngineError::business(
            BIZ_KEY_VIOLATION,
            format!("--key 必须是 --cols 的真子集（upsert 需要非键列可更新）：keys={keys:?}"),
        ));
    }
    Ok(())
}

/// 目标列元数据（G1 守卫输入；来自 information_schema）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCol {
    /// 列名。
    pub name: String,
    /// 是否 NOT NULL。
    pub not_null: bool,
    /// 是否有默认值。
    pub has_default: bool,
}

/// G1 纯判定：`--cols` 与目标列集的兼容性违规清单（空 = 兼容）。
/// 写入列不存在 → 必失败；目标 NOT NULL 无默认且不写 → 必失败。
pub fn schema_violations(target: &[TargetCol], cols: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    for c in cols {
        if !target.iter().any(|t| t.name.eq_ignore_ascii_case(c)) {
            v.push(format!("写入列 {c} 不存在于目标表"));
        }
    }
    for t in target {
        let written = cols.iter().any(|c| c.eq_ignore_ascii_case(&t.name));
        if t.not_null && !t.has_default && !written {
            v.push(format!(
                "目标列 {} 为 NOT NULL 且无默认值，但不在写入列集",
                t.name
            ));
        }
    }
    v
}

/// G4 纯判定：是否存在唯一约束/PK 的列集覆盖 upsert key（大小写不敏感）。
pub fn has_unique_covering(constraints: &[Vec<String>], keys: &[String]) -> bool {
    constraints.iter().any(|c| {
        keys.iter()
            .all(|k| c.iter().any(|cc| cc.eq_ignore_ascii_case(k)))
    })
}

/// upsert 语句对：join 式 UPDATE + NOT EXISTS INSERT（设计 §4.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertStatements {
    /// 键控更新（staging → 目标已存在行）。
    pub update: String,
    /// 仅合并目标中不存在的键。
    pub insert: String,
}

/// 构造 upsert 语句对：staging 临时表 join 式（集合操作，语句数与行数无关）。
pub fn build_upsert_statements(table: &str, cols: &[String], keys: &[String]) -> UpsertStatements {
    let sets: Vec<String> = cols
        .iter()
        .filter(|c| !keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
        .map(|c| format!("{c} = tmp.{c}"))
        .collect();
    let update = format!(
        "UPDATE {table} SET {} FROM {TMP_TABLE} tmp WHERE {}",
        sets.join(", "),
        key_pred(keys)
    );
    let collist = cols.join(", ");
    let tmpcols = cols
        .iter()
        .map(|c| format!("tmp.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let insert = format!(
        "INSERT INTO {table} ({collist}) SELECT {tmpcols} FROM {TMP_TABLE} tmp \
         WHERE NOT EXISTS (SELECT 1 FROM {table} x WHERE {})",
        key_pred_for(keys, "x", "tmp")
    );
    UpsertStatements { update, insert }
}

/// 键等值谓词（两侧同别名口径）。
fn key_pred(keys: &[String]) -> String {
    keys.iter()
        .map(|k| format!("tmp.{k} = {k}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// 键等值谓词（显式两侧别名）。
fn key_pred_for(keys: &[String], target_alias: &str, tmp_alias: &str) -> String {
    keys.iter()
        .map(|k| format!("{tmp_alias}.{k} = {target_alias}.{k}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// G2：staging 内越界 slice 的行数（NULL 同判越界）。
pub fn staging_slice_violations(
    duck: &dyn SqlExecutor,
    table: &str,
    preds: &[SlicePredicate],
) -> Result<i64, EngineError> {
    let pred = slice_where(preds);
    let sql = format!("SELECT count(*) FROM {table} WHERE ({pred}) IS NOT TRUE");
    scalar_count(&mut duck.query(&sql, &BindMap::new())?)
}

/// G3：staging key 守卫——key 列须在结果集中、无 NULL、唯一。
pub fn assert_staging_keys(
    duck: &dyn SqlExecutor,
    table: &str,
    keys: &[String],
) -> Result<(), EngineError> {
    let keylist = keys.join(", ");
    let null_sql = format!(
        "SELECT count(*) FROM {table} WHERE {}",
        keys.iter()
            .map(|k| format!("{k} IS NULL"))
            .collect::<Vec<_>>()
            .join(" OR ")
    );
    let null_n = scalar_count(&mut duck.query(&null_sql, &BindMap::new())?)?;
    if null_n > 0 {
        return Err(EngineError::business(
            BIZ_KEY_VIOLATION,
            format!("staging 有 {null_n} 行 upsert key 含 NULL（{keylist}）——每行必须携带键"),
        ));
    }
    let rows =
        scalar_count(&mut duck.query(&format!("SELECT count(*) FROM {table}"), &BindMap::new())?)?;
    let distinct = scalar_count(&mut duck.query(
        &format!("SELECT count(*) FROM (SELECT DISTINCT {keylist} FROM {table})"),
        &BindMap::new(),
    )?)?;
    if distinct < rows {
        return Err(EngineError::business(
            BIZ_KEY_VIOLATION,
            format!(
                "staging upsert key 不唯一：{keylist}（{rows} 行 / {distinct} distinct，重复 {} 组）",
                rows - distinct
            ),
        ));
    }
    Ok(())
}

/// 标量计数取值（count(*) → SpinValue::Int）。
fn scalar_count(s: &mut RowStream) -> Result<i64, EngineError> {
    match s.fetch_many(1).first().and_then(|r| r.first()) {
        Some(SpinValue::Int(c)) => Ok(*c),
        other => Err(EngineError::env(
            15,
            format!("计数查询返回异常值 {other:?}"),
        )),
    }
}

/// 读取端 SELECT（append 的可选 WHERE 与 slice 谓词共用拼装口径）。
pub(crate) fn read_select(table: &str, cols: &[String], where_clause: Option<&str>) -> String {
    match where_clause {
        Some(w) => format!("SELECT {} FROM {table} WHERE {w}", cols.join(", ")),
        None => format!("SELECT {} FROM {table}", cols.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 假执行器：对配置的 SQL 回放计数，并记录收到的 SQL 供断言。
    struct FakeDuck {
        counts: RefCell<Vec<(String, i64)>>,
        seen: RefCell<Vec<String>>,
    }

    impl FakeDuck {
        fn with_counts(pairs: &[(&str, i64)]) -> Self {
            Self {
                counts: RefCell::new(pairs.iter().map(|(s, n)| (s.to_string(), *n)).collect()),
                seen: RefCell::new(Vec::new()),
            }
        }
        fn answer(&self, sql: &str) -> i64 {
            self.seen.borrow_mut().push(sql.to_string());
            self.counts
                .borrow()
                .iter()
                .find(|(s, _)| sql.contains(s))
                .map(|(_, n)| *n)
                .unwrap_or(0)
        }
    }

    impl SqlExecutor for FakeDuck {
        fn query(&self, sql: &str, _binds: &BindMap) -> Result<RowStream, EngineError> {
            let n = self.answer(sql);
            Ok(RowStream::new(
                vec!["count_star()".into()],
                vec![vec![SpinValue::Int(n)]],
            ))
        }
        fn execute(
            &self,
            _sql: &str,
            _binds: &BindMap,
        ) -> Result<crate::exec::ExecOutcome, EngineError> {
            unreachable!("守卫只发查询")
        }
    }

    #[test]
    fn should_parse_slice_predicates_and_expand_env_refs() {
        // SAFETY-less: 单线程测试进程内设置唯一变量名
        std::env::set_var("SPINDUCK_TEST_SLICE_DAY", "20261111");
        let preds = resolve_slice("biz_date=$SPINDUCK_TEST_SLICE_DAY, split_src=BUY").unwrap();
        assert_eq!(preds.len(), 2);
        assert_eq!(preds[0].col, "biz_date");
        assert_eq!(preds[0].value, "20261111");
        assert_eq!(preds[1].value, "BUY");
    }

    #[test]
    fn should_err_env_0016_when_slice_env_ref_missing() {
        std::env::remove_var("SPINDUCK_TEST_SLICE_ABSENT");
        let e = resolve_slice("biz_date=${SPINDUCK_TEST_SLICE_ABSENT}").unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0016");
        assert!(e.message.contains("SPINDUCK_TEST_SLICE_ABSENT"));
    }

    #[test]
    fn should_reject_malformed_slice_predicate() {
        let e = parse_slice("biz_date").unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0010");
        assert!(e.message.contains("k=v"));
    }

    #[test]
    fn should_build_slice_where_with_quote_escape() {
        let preds = resolve_slice("d=it's,src=BUY").unwrap();
        assert_eq!(slice_where(&preds), "d = 'it''s' AND src = 'BUY'");
    }

    #[test]
    fn should_resolve_modes_from_raw_flags() {
        assert_eq!(
            resolve_mode(None, None, None, Some("n>1")).unwrap(),
            SyncMode::Append {
                where_clause: Some("n>1".into())
            }
        );
        let preds = resolve_slice("d=20261111").unwrap();
        assert_eq!(
            resolve_mode(Some("slice"), Some("d=20261111"), None, None).unwrap(),
            SyncMode::Slice { predicates: preds }
        );
        assert_eq!(
            resolve_mode(Some("upsert"), None, Some("a,b"), None).unwrap(),
            SyncMode::Upsert {
                keys: vec!["a".into(), "b".into()]
            }
        );
    }

    #[test]
    fn should_reject_upsert_key_not_subset_of_cols() {
        let e = validate_keys_subset(&["k".into()], &["a".into(), "b".into()]).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0103");
        assert!(e.message.contains("不在 --cols"));
    }

    #[test]
    fn should_reject_upsert_key_equal_to_cols() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let e = validate_keys_subset(&["a".into(), "b".into()], &cols).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0103");
        assert!(e.message.contains("真子集"));
    }

    #[test]
    fn should_fail_schema_guard_on_missing_write_col_and_missing_not_null() {
        let target = vec![
            TargetCol {
                name: "a".into(),
                not_null: false,
                has_default: false,
            },
            TargetCol {
                name: "req".into(),
                not_null: true,
                has_default: false,
            },
        ];
        let v = schema_violations(&target, &["a".to_string(), "ghost".to_string()]);
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("ghost"));
        assert!(v[1].contains("req"));
    }

    #[test]
    fn should_pass_schema_guard_when_target_covered() {
        let target = vec![
            TargetCol {
                name: "a".into(),
                not_null: true,
                has_default: true,
            },
            TargetCol {
                name: "b".into(),
                not_null: false,
                has_default: false,
            },
        ];
        assert!(schema_violations(&target, &["a".to_string(), "b".to_string()]).is_empty());
    }

    #[test]
    fn should_require_unique_constraint_covering_all_keys() {
        let constraints = vec![
            vec!["a".to_string()],
            vec!["x".to_string(), "y".to_string()],
        ];
        assert!(has_unique_covering(&constraints, &["x".into(), "y".into()]));
        assert!(!has_unique_covering(
            &constraints,
            &["x".into(), "z".into()]
        ));
    }

    #[test]
    fn should_build_upsert_statements_with_key_predicate() {
        let cols = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let keys = vec!["a".to_string()];
        let UpsertStatements { update, insert } = build_upsert_statements("t_out", &cols, &keys);
        assert_eq!(
            update,
            "UPDATE t_out SET b = tmp.b, c = tmp.c FROM sd_sync_tmp tmp WHERE tmp.a = a"
        );
        assert!(insert.starts_with("INSERT INTO t_out (a, b, c) SELECT tmp.a, tmp.b, tmp.c"));
        assert!(insert.contains("WHERE NOT EXISTS (SELECT 1 FROM t_out x WHERE tmp.a = x.a)"));
    }

    #[test]
    fn should_err_biz_0103_when_staging_key_has_null() {
        let duck = FakeDuck::with_counts(&[("IS NULL", 3)]);
        let e = assert_staging_keys(&duck, "stg", &["k".to_string()]).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0103");
        assert!(e.message.contains("3 行"));
    }

    #[test]
    fn should_err_biz_0103_when_staging_key_not_unique() {
        let duck = FakeDuck::with_counts(&[
            ("IS NULL", 0),
            ("SELECT count(*) FROM stg", 10),
            ("DISTINCT", 8),
        ]);
        let e = assert_staging_keys(&duck, "stg", &["k".to_string()]).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0103");
        assert!(e.message.contains("10 行 / 8 distinct"));
    }

    #[test]
    fn should_pass_staging_key_guard_when_unique() {
        let duck = FakeDuck::with_counts(&[
            ("IS NULL", 0),
            ("SELECT count(*) FROM stg", 10),
            ("DISTINCT", 10),
        ]);
        assert!(assert_staging_keys(&duck, "stg", &["k".to_string()]).is_ok());
    }
}
