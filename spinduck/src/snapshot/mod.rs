//! 结果快照（design §6.2 A5 的 `query`/`compare` 共用载体）。
//!
//! 快照 = 一次查询的 `{schema, rows}`：单元格存**内核给回的文本**（未归一），
//! 数据库 NULL 用 JSON `null` 承载。裁决口径一律留给 [`crate::norm`]——归一发生在比对时
//! 而不是落盘时，这样同一份快照既能和新快照比，也能和人手写的基线比，且基线改了口径
//! 不需要重跑查询。

pub mod compare;

use crate::error::EngineError;
use crate::exec::{BindMap, SqlExecutor};
use crate::value::SpinValue;
use serde_json::Value;

/// 快照 JSON 不合约定（缺键、类型不对、语法错）——同一个码、同一段话术，
/// 与 `case.toml` 的 `[case]` 段缺失判定同族（`ENV-0031`）。
fn shape_err(msg: impl Into<String>) -> EngineError {
    EngineError::env(31, msg)
}

/// 单元格 JSON → 文本：`null` 保持 NULL，数字/布尔按字面文本收下。
///
/// 宽容是给**手写基线**的：`"600000"` 与 `600000` 必须落到同一个值，否则比对口径取决于
/// 谁手抖加了引号。`query` 侧永远写文本，走不到数字分支。
fn cell_json(v: &Value) -> Result<Option<String>, EngineError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        other => Err(shape_err(format!(
            "快照单元格只能是文本/数字/布尔/null，实际是 {}",
            type_name(other)
        ))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "布尔",
        Value::Number(_) => "数字",
        Value::String(_) => "文本",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

/// 单元格 → JSON 文本（转义交给 `serde_json`，不手写引号）。
fn cell_to_json(cell: &Option<String>) -> String {
    match cell {
        Some(t) => Value::String(t.clone()).to_string(),
        None => "null".to_string(),
    }
}

/// 快照 JSON 形态（`query` 与 `compare` 的公共契约，见模块头）。
///
/// ```text
/// {
///   "schema": ["a", "b"],
///   "rows": [
///     ["1", "abc"],
///     ["2", null]
///   ]
/// }
/// ```
///
/// 一行一行写是为了让快照本身可 `diff`、能在评审里直接读，文件结尾恒带一个换行；
/// `rows` 为空时是 `[]`（没有 `[]` 内层行）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub schema: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Snapshot {
    /// JSON 文本 → 快照。
    pub fn from_json_str(text: &str) -> Result<Self, EngineError> {
        let v: Value = serde_json::from_str(text)
            .map_err(|e| shape_err(format!("快照 JSON 无法解析: {e}")))?;
        let obj = v
            .as_object()
            .ok_or_else(|| shape_err("快照 JSON 顶层必须是 {{schema, rows}} 对象"))?;
        let schema_json = obj
            .get("schema")
            .and_then(Value::as_array)
            .ok_or_else(|| shape_err("快照 JSON 缺 schema 数组"))?;
        let schema = schema_json
            .iter()
            .map(|c| {
                c.as_str().map(str::to_string).ok_or_else(|| {
                    shape_err(format!("schema 列名必须是文本，实际是 {}", type_name(c)))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let rows_json = obj
            .get("rows")
            .and_then(Value::as_array)
            .ok_or_else(|| shape_err("快照 JSON 缺 rows 数组"))?;
        let mut rows = Vec::with_capacity(rows_json.len());
        for r in rows_json {
            let cells = r.as_array().ok_or_else(|| {
                shape_err(format!("rows 每项必须是行数组，实际是 {}", type_name(r)))
            })?;
            let mut row = Vec::with_capacity(cells.len());
            for c in cells {
                row.push(cell_json(c)?);
            }
            rows.push(row);
        }
        Ok(Self { schema, rows })
    }

    /// 快照 → JSON 文本（同输入同字节）。
    pub fn to_json_string(&self) -> String {
        let cols: Vec<String> = self
            .schema
            .iter()
            .map(|c| Value::String(c.clone()).to_string())
            .collect();
        let mut out = String::from("{\n  \"schema\": [");
        out.push_str(&cols.join(", "));
        out.push_str("],\n  \"rows\": [");
        if self.rows.is_empty() {
            out.push_str("]\n}\n");
            return out;
        }
        out.push('\n');
        for r in &self.rows {
            let cells: Vec<String> = r.iter().map(cell_to_json).collect();
            out.push_str(&format!("    [{}],\n", cells.join(", ")));
        }
        // 去掉最后一行的逗号与换行
        out.truncate(out.len() - 2);
        out.push_str("\n  ]\n}\n");
        out
    }
}

/// 跑一条 SQL，把结果集做成快照（`query` 的内核侧实现，CLI 只做壳）。
///
/// 语句文本 = 文件内容按**引号与注释之外**的分号切句后剩下的那一条（`.sql` 末尾写分号
/// 是书写习惯）。多语句必须在这里拒绝而不是交给内核：实测 DuckDB 的 `prepare` 会把文件
/// 里每条语句都执行，却只把**最后一条**的结果集给回来——两句的文件会得到一份只覆盖第二
/// 句的快照，而第一句若含写入已经落进库了。`query` 的契约是一条 SQL，静默少比一条诚实。
///
/// 结果集整体驻留内存（[`crate::exec::RowStream`] 的契约就是快照）——所以本命令面向
/// **结果裁决**（聚合输出、小表全量），千万级取数属 design A3 的 COPY OUT 流式改造。
pub fn capture(ex: &dyn SqlExecutor, text: &str) -> Result<Snapshot, EngineError> {
    let sql = match statements(text).as_slice() {
        [] => return Err(shape_err("query: SQL 文件里没有语句")),
        [one] => *one,
        many => {
            return Err(shape_err(format!(
                "query: SQL 文件里有 {} 条语句，一次只跑一条（拆开写）",
                many.len()
            )))
        }
    };
    let mut stream = ex.query(sql, &BindMap::new())?;
    let n = stream.len();
    let rows = stream
        .fetch_many(n)
        .iter()
        .map(|r| r.iter().map(cell_of).collect())
        .collect();
    Ok(Snapshot {
        schema: stream.schema,
        rows,
    })
}

/// 引擎值 → 单元格文本；`Null` 保持 NULL，不塌成空文本（快照要能区分两者，归一时才判同值）。
fn cell_of(v: &SpinValue) -> Option<String> {
    match v {
        SpinValue::Null => None,
        other => Some(other.as_str_loose()),
    }
}

/// 文件文本 → 语句清单：只在 `'…'`/`"…"`（`''`、`""` 为转义）、`--` 行注释、`/* */`
/// 块注释**之外**的分号处切句；每段去首尾空白，空段丢弃。
///
/// 按字节扫描是安全的：切句符全为 ASCII，UTF-8 续字节恒 ≥0x80，切片边界必然落在字符边界上。
/// 未闭合的引号或块注释不另判：那一行内核本来就不认，`query` 不抢它的故障域（§5）。
fn statements(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() {
                    if b[i] != q {
                        i += 1;
                    } else if b.get(i + 1) == Some(&q) {
                        // doubled 引号是 SQL 里的转义写法，不是串结束
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b';' => {
                push_chunk(&mut out, &text[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_chunk(&mut out, &text[start..]);
    out
}

/// 分号切出来的一段 → 语句：去首尾空白，只有空白的段丢弃（`SELECT 1;` 末尾那段）。
fn push_chunk<'a>(out: &mut Vec<&'a str>, chunk: &'a str) {
    let t = chunk.trim();
    if !t.is_empty() {
        out.push(t);
    }
}

#[cfg(test)]
mod tests {
    use super::Snapshot;

    #[test]
    fn should_roundtrip_schema_and_cells_through_json() {
        let s = Snapshot {
            schema: vec!["k".into(), "v".into()],
            rows: vec![
                vec![Some("1".into()), Some("abc".into())],
                vec![Some("2".into()), None],
            ],
        };
        let json = s.to_json_string();
        assert_eq!(Snapshot::from_json_str(&json).unwrap(), s);
    }

    #[test]
    fn should_write_null_for_database_null_and_keep_cell_text_verbatim() {
        // 落盘的是内核给回的文本，归一留给 compare：`00123` 不得被削成 `123`
        let s = Snapshot {
            schema: vec!["a".into()],
            rows: vec![vec![Some("00123".into())], vec![None]],
        };
        // 一行一行的排布是契约的一部分：快照本身要能 diff、能在评审里读
        // 结尾换行同理：没有它，`cat` 会把产物和下一条提示符黏在一起，`diff` 还要多打
        // 一行 "No newline at end of file"——快照是给人读的交付物
        assert_eq!(
            s.to_json_string(),
            r#"{
  "schema": ["a"],
  "rows": [
    ["00123"],
    [null]
  ]
}
"#
        );
    }

    #[test]
    fn should_read_numbers_and_booleans_as_text_for_handwritten_baselines() {
        // 手写基线里 `"600000"` 与 `600000` 必须落到同一个单元格，否则比对口径取决于谁
        // 手抖加了引号
        let s =
            Snapshot::from_json_str(r#"{"schema":["a","b"],"rows":[[600000,true],[null,"x"]]}"#)
                .unwrap();
        assert_eq!(s.rows[0], vec![Some("600000".into()), Some("true".into())]);
        assert_eq!(s.rows[1], vec![None, Some("x".into())]);
    }

    #[test]
    fn should_reject_json_without_schema_or_rows() {
        for bad in [
            r#"{"rows":[[]]}"#,
            r#"{"schema":["a"]}"#,
            r#"{"schema":"a","rows":[]}"#,
            r#"not json"#,
        ] {
            let e = Snapshot::from_json_str(bad).unwrap_err();
            assert_eq!(e.code.to_string(), "ENV-0031", "{bad}");
            assert_eq!(e.domain, crate::error::FaultDomain::Env);
        }
    }

    #[test]
    fn should_read_an_empty_result_set_as_zero_rows() {
        // 空结果集是"绿"的一种形态，不得与"没跑成"混成一个形状
        let s = Snapshot::from_json_str(r#"{"schema":["a"],"rows":[]}"#).unwrap();
        assert_eq!(s.rows.len(), 0);
        assert_eq!(
            s.to_json_string(),
            r#"{
  "schema": ["a"],
  "rows": []
}
"#
        );
    }
}

#[cfg(test)]
mod capture_tests {
    use super::capture;
    use crate::error::EngineError;
    use crate::exec::{BindMap, ExecOutcome, RowStream, SqlExecutor};
    use crate::snapshot::Snapshot;
    use crate::value::SpinValue;

    #[test]
    fn should_capture_schema_and_cells_as_kernel_given_text() {
        let ex = crate::exec::FakeExecutor::with_rows(
            vec!["k".into(), "v".into()],
            vec![
                vec![SpinValue::Int(7), SpinValue::Str("abc".into())],
                vec![SpinValue::Decimal(1234, 38, 2), SpinValue::Null],
            ],
        );
        assert_eq!(
            capture(&ex, "SELECT k, v FROM t").unwrap(),
            Snapshot {
                schema: vec!["k".into(), "v".into()],
                rows: vec![
                    vec![Some("7".into()), Some("abc".into())],
                    // DECIMAL 走显示串（12.34），NULL 是 null 而不是空文本
                    vec![Some("12.34".into()), None],
                ],
            }
        );
    }

    #[test]
    fn should_send_the_statement_after_stripping_one_trailing_semicolon() {
        // .sql 文件末尾的分号是书写习惯，不是语句的一部分；内核 prepare 会因它报错
        let ex = crate::exec::FakeExecutor::default();
        capture(&ex, "\n  SELECT a FROM t ;\n").unwrap();
        assert_eq!(ex.calls()[0].0, "SELECT a FROM t");
    }

    #[test]
    fn should_fetch_every_row_not_a_capped_page() {
        let rows: Vec<Vec<SpinValue>> = (0..5_000).map(|i| vec![SpinValue::Int(i)]).collect();
        let ex = crate::exec::FakeExecutor::with_rows(vec!["i".into()], rows);
        assert_eq!(capture(&ex, "SELECT i FROM t").unwrap().rows.len(), 5_000);
    }

    #[test]
    fn should_reuse_the_named_bind_channel_with_no_binds() {
        // query 的 SQL 面不接实参：绑定表恒空，占位符留给 call 那条路
        let ex = crate::exec::FakeExecutor::default();
        capture(&ex, "SELECT 1").unwrap();
        assert_eq!(ex.calls()[0].1, BindMap::new());
    }

    #[test]
    fn should_reject_a_file_without_any_statement() {
        // 空语句交给内核只会得到一条语法错，指不到"文件里没有语句"这件事——所以在这里判
        let ex = crate::exec::FakeExecutor::default();
        for bad in ["", "   \n  ", "  ;  "] {
            let e = capture(&ex, bad).unwrap_err();
            assert_eq!(e.code.to_string(), "ENV-0031", "{bad:?}");
        }
        assert!(ex.calls().is_empty(), "判红不该先把空语句发给内核");
    }

    #[test]
    fn should_refuse_a_file_with_more_than_one_statement_before_running_any() {
        // 实测：DuckDB 的 prepare 会把两句都执行，只把**最后一句**的结果给回来
        // （那条事实本身在 kernel_evidence_tests 里钉住，本条测的是我们的拒绝）。
        // 所以"交给内核报红"这条路不存在——不在这里拒，就得到一份只覆盖第二句的快照，
        // 而第一句（可能是 INSERT）已经写进库里了。
        let ex = crate::exec::FakeExecutor::default();
        let e = capture(&ex, "SELECT 1; SELECT 2").unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0031");
        assert!(e.message.contains("2 条语句"), "{}", e.message);
        assert!(
            ex.calls().is_empty(),
            "判红不得先把第一句发给内核（侧效应已经发生就无法回退）"
        );
    }

    #[test]
    fn should_not_split_at_a_semicolon_inside_a_string_or_a_comment() {
        // 切句只在结构边界上做：字符串里的分号（`'a;b'`）与注释里的分号都不是语句分隔
        let cases = [
            "SELECT 'a;b' AS v",
            "SELECT 1 -- 收尾分号写在注释里 ;\n",
            "SELECT 1 /* ; */",
            r#"SELECT "a;b" FROM t"#,
            "SELECT 'it''s ; fine' AS v",
        ];
        for sql in cases {
            let ex = crate::exec::FakeExecutor::default();
            capture(&ex, sql).unwrap();
            assert_eq!(ex.calls().len(), 1, "{sql:?} 被切成了多句");
            assert_eq!(ex.calls()[0].0, sql.trim(), "{sql:?}");
        }
    }

    #[test]
    fn should_propagate_kernel_errors_with_their_own_fault_domain() {
        struct Boom;
        impl SqlExecutor for Boom {
            fn query(&self, _sql: &str, _b: &BindMap) -> Result<RowStream, EngineError> {
                Err(EngineError::kernel(70, "syntax error at or near \"FRIM\""))
            }
            fn execute(&self, _sql: &str, _b: &BindMap) -> Result<ExecOutcome, EngineError> {
                Err(EngineError::kernel(70, "unreachable"))
            }
        }
        let e = capture(&Boom, "SELECT 1 FRIM t").unwrap_err();
        assert_eq!(e.domain, crate::error::FaultDomain::Kernel);
        assert_eq!(e.code.to_string(), "KERNEL-0070");
    }
}

/// 上一条契约的前提在真内核里的取证位（`should_refuse_a_file_with_more_than_one_statement…`
/// 用的是 FakeExecutor，它证明的是"我们的切句拒绝"，不是"DuckDB 会怎么做"）。
#[cfg(all(test, feature = "native"))]
mod kernel_evidence_tests {
    use crate::exec::{BindMap, SqlExecutor};

    #[test]
    fn should_run_every_statement_and_return_only_the_last_result_set() {
        // 内核不拒多语句：INSERT 的侧效应已经发生，回给你的却只有 SELECT 那一行。
        // 这就是 capture 必须在发出去之前切句拒绝的理由——事后拦不住。
        let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
        ex.execute("CREATE TABLE t (v INT)", &BindMap::new())
            .unwrap();
        let mut stream = ex
            .query(
                "INSERT INTO t VALUES (1); SELECT COUNT(*) AS n FROM t",
                &BindMap::new(),
            )
            .unwrap();
        let n = stream.len();
        let rows = stream.fetch_many(n);
        assert_eq!(stream.schema, vec!["n".to_string()]);
        assert_eq!(n, 1);
        assert_eq!(rows[0][0], crate::value::SpinValue::Int(1));
    }
}
