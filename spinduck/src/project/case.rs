//! 用例声明（`*.case.toml`）识别：数据集 × 例程 × 期望输出（§12.1）。

use crate::error::EngineError;
use crate::project::entry::qualified_routine;
use crate::project::fs::{cfg_err, read_text, str_list};
use std::path::Path;

/// 用例声明（`*.case.toml`）：数据集 × 例程 × 期望输出（§12.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseConfig {
    pub name: String,
    /// `function`（比对返回值）| `procedure`（比对输出表的 SQL 结果）
    pub kind: CaseKind,
    /// 被测例程名（`pkg.proc` 形态允许）
    pub routine: String,
    /// `[case].entry`：入口声明形态的被测例程全名（`package.procedure`，归一小写）
    pub entry: Option<String>,
    /// 场景列表（执行参数 + 用途说明）
    pub scenarios: Vec<Scenario>,
    /// 显式断言集（`[[case.assert]]`，可空）：行集/返回值/OUT 参/错误契约
    pub asserts: Vec<CaseAssert>,
    /// `procedure` 形态的输出读取 SQL（两侧共用，必须自含 ORDER BY）
    pub output_sql: Option<String>,
    /// 附加输出读取 SQL（两侧逐行比对，不参与基线漂移检查）
    pub extra_sql: Option<String>,
    /// `procedure` 形态的 `p_o_succeed` 期望（`None` = 不校验）
    pub expect_succeed: Option<String>,
    /// 基线文件名（`cases/baselines/` 下）
    pub baseline: String,
    /// 参照侧专用例程（`cases/baselines/` 下；`None` = 两侧共用 `sql/routines`）
    pub reference_routine: Option<String>,
    /// 参照侧调用模板（含 `{args}` 占位；`None` = 按 kind 默认 `SELECT`/`CALL`）
    pub reference_call_sql: Option<String>,
    /// 需从参照侧物化的输入表（输出表不在此列，保持被测侧为空）
    pub materialize: Vec<String>,
    /// 过程写入的输出表（harness 额外断言两侧行数一致）
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]

pub enum CaseKind {
    Function,
    Procedure,
}

/// 执行场景：字面量参数（SQL 字面量形态，如 `'20240105'`、`null`）。
#[derive(Debug, Clone, PartialEq, Eq)]

pub struct Scenario {
    pub why: String,
    pub args: Vec<String>,
}

/// 用例断言的一条（`[[case.assert]]`）：一个粒度、一个可观察面。
///
/// 与 `[case.scenarios]` 的分工：场景只证明"可调用"，断言锁定"可观察结果"——
/// 行集、返回值、OUT 参、错误契约四个维度，全部在进程内内核上执行（零外部环境）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseAssert {
    pub why: String,
    pub kind: AssertKind,
    /// `Rows`：断言 SQL（自带 ORDER BY 定序）；其余：被测例程全名
    pub target: String,
    /// 例程断言的实参（SQL 字面量形态，同 `[case.scenarios].args`）
    pub args: Vec<String>,
    /// `Out` 形态的 OUT 参名（归一小写；其余形态忽略）
    pub param: String,
    /// 期望文本。`Rows`：归一行集 `"[a, b] [c, d]"`；`Call`/`Out`：归一标量；
    /// `Error`：`"CODE|消息片段"`（`|` 后片段可省略 = 只比错误码）
    pub want: String,
}

/// 断言维度（决定 `target` 的解释方式与期望文本口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertKind {
    /// 内核查表行集（`target` 为 SQL）
    Rows,
    /// 例程返回值
    Call,
    /// 例程 OUT / INOUT 参
    Out,
    /// 例程必须失败，且错误码（可选消息片段）一致
    Error,
}

impl CaseConfig {
    pub fn load(path: &Path) -> Result<Self, EngineError> {
        let text = read_text(path)?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(text: &str) -> Result<Self, EngineError> {
        let v: toml::Value = toml::from_str(text).map_err(|e| cfg_err("case.toml", e))?;
        let case = v
            .get("case")
            .ok_or_else(|| EngineError::env(31, "case.toml: 缺少 [case] 段"))?;
        let name = case
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| EngineError::env(31, "case.toml: [case].name 必填"))?;
        let kind = match case
            .get("kind")
            .and_then(|x| x.as_str())
            .unwrap_or("procedure")
        {
            "function" => CaseKind::Function,
            "procedure" => CaseKind::Procedure,
            other => {
                return Err(EngineError::env(
                    31,
                    format!("case.toml: [case].kind 不支持 '{other}'（function|procedure）"),
                ))
            }
        };
        // 被测例程：`routine`（允许短名，harness 旧口径）或 `entry`（§12.1 入口声明，
        // 必须包限定）；两者都缺则显式报错，不静默默认
        let entry = match case.get("entry").and_then(|x| x.as_str()) {
            Some(raw) => Some(qualified_routine(raw).map_err(|_| {
                EngineError::env(
                    31,
                    format!("case.toml: [case].entry 必须为 package.procedure 全名，收到 '{raw}'"),
                )
            })?),
            None => None,
        };
        let routine = match (case.get("routine").and_then(|x| x.as_str()), &entry) {
            (Some(r), _) => r.to_string(),
            (None, Some(e)) => e.clone(),
            (None, None) => {
                return Err(EngineError::env(
                    31,
                    "case.toml: [case].routine 与 [case].entry 至少填一个（entry 须为 package.procedure）",
                ))
            }
        };
        let scenarios_raw = case
            .get("scenarios")
            .and_then(|x| x.as_array())
            .ok_or_else(|| EngineError::env(31, "case.toml: 至少一个 [[case.scenarios]]"))?;
        let mut scenarios = Vec::with_capacity(scenarios_raw.len());
        for s in scenarios_raw {
            let why = s
                .get("why")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let args = arg_list(s.get("args"), "scenario.args")?;
            scenarios.push(Scenario { why, args });
        }
        let asserts_raw = case.get("assert").and_then(|x| x.as_array());
        let mut asserts = Vec::new();
        for (i, a) in asserts_raw
            .as_ref()
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            asserts.push(parse_assert(a, i + 1)?);
        }
        let output = case
            .get("output")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        let output_sql = output.get("sql").and_then(|x| x.as_str()).map(String::from);
        let extra_sql = output
            .get("extra_sql")
            .and_then(|x| x.as_str())
            .map(String::from);
        if kind == CaseKind::Procedure && output_sql.is_none() {
            return Err(EngineError::env(
                31,
                "case.toml: procedure 形态必须声明 [case.output].sql",
            ));
        }
        Ok(Self {
            name: name.to_string(),
            kind,
            routine,
            entry,
            scenarios,
            asserts,
            output_sql,
            extra_sql,
            expect_succeed: case
                .get("expect_succeed")
                .and_then(|x| x.as_str())
                .map(String::from),
            baseline: case
                .get("baseline")
                .and_then(|x| x.as_str())
                .unwrap_or("expected.json")
                .to_string(),
            reference_routine: case
                .get("reference_routine")
                .and_then(|x| x.as_str())
                .map(String::from),
            reference_call_sql: case
                .get("reference_call_sql")
                .and_then(|x| x.as_str())
                .map(String::from),
            materialize: str_list(case.get("data").and_then(|d| d.get("materialize"))),
            outputs: str_list(case.get("data").and_then(|d| d.get("outputs"))),
        })
    }
}

/// SQL 字面量实参列表：保持声明原文（`'20240105'`、`null`、数字）供调用侧解释。
fn arg_list(v: Option<&toml::Value>, what: &str) -> Result<Vec<String>, EngineError> {
    match v {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(a)) => Ok(a.iter().map(literal_text).collect()),
        Some(_) => Err(EngineError::env(
            31,
            format!("case.toml: {what} 必须是数组"),
        )),
    }
}

/// 标量 TOML 值 → 实参文本（数组/表形态按空串处理，调用侧会显式拒绝）。
fn literal_text(x: &toml::Value) -> String {
    match x {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        _ => String::new(),
    }
}

/// `[[case.assert]]` 的缺项/非法值统一报 ENV-0031，消息带条目序号（定位到具体断言块）。
fn assert_field_err(idx: usize, msg: impl Into<String>) -> EngineError {
    EngineError::env(
        31,
        format!("case.toml: [[case.assert]]#{idx} {}", msg.into()),
    )
}

/// 一条 `[[case.assert]]`：`kind`/`target`/`want` 必填，`out` 另需 `param`。
///
/// 缺项一律显式报错——空 `want` 等于永真断言（AGENTS §2 Never），不接受静默通过。
fn parse_assert(v: &toml::Value, idx: usize) -> Result<CaseAssert, EngineError> {
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let kind_txt = field("kind");
    let kind = match kind_txt.as_str() {
        "rows" => AssertKind::Rows,
        "call" => AssertKind::Call,
        "out" => AssertKind::Out,
        "error" => AssertKind::Error,
        other => {
            return Err(assert_field_err(
                idx,
                format!("的 kind 不支持 '{other}'（rows|call|out|error）"),
            ))
        }
    };
    let target = field("target");
    if target.is_empty() {
        return Err(assert_field_err(
            idx,
            "的 target 必填（rows 为断言 SQL，其余为被测例程全名）",
        ));
    }
    let want = field("want");
    if want.is_empty() {
        return Err(assert_field_err(idx, "的 want 必填（空期望即永真断言）"));
    }
    let param = field("param").to_ascii_lowercase();
    if kind == AssertKind::Out && param.is_empty() {
        return Err(assert_field_err(
            idx,
            "的 param 必填（kind=out 需 OUT 参名）",
        ));
    }
    Ok(CaseAssert {
        why: field("why"),
        kind,
        target,
        args: arg_list(v.get("args"), &format!("[[case.assert]]#{idx} 的 args"))?,
        param,
        want,
    })
}
