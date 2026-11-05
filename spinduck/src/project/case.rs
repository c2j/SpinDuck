//! 用例声明（`*.case.toml`）识别：数据集 × 例程 × 期望输出（§12.1）。

use crate::error::EngineError;
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
    /// 场景列表（执行参数 + 用途说明）
    pub scenarios: Vec<Scenario>,
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
        let routine = case
            .get("routine")
            .and_then(|x| x.as_str())
            .ok_or_else(|| EngineError::env(31, "case.toml: [case].routine 必填"))?;
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
            let args = match s.get("args") {
                Some(toml::Value::Array(a)) => a
                    .iter()
                    .map(|x| match x {
                        toml::Value::String(v) => v.clone(),
                        toml::Value::Integer(i) => i.to_string(),
                        toml::Value::Float(f) => f.to_string(),
                        toml::Value::Boolean(b) => b.to_string(),
                        _ => String::new(),
                    })
                    .collect(),
                None => Vec::new(),
                Some(_) => return Err(EngineError::env(31, "case.toml: scenario.args 必须是数组")),
            };
            scenarios.push(Scenario { why, args });
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
            routine: routine.to_string(),
            scenarios,
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
