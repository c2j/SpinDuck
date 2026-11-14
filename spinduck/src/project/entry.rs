//! 入口存过声明（`[[entry]]`，§12.1）：本项目对外可被调用的例程全名。
//!
//! 项目态 `compile` 从这些全名（或显式 `--entry`）出发求闭包并做 IR 体检；
//! 库级链路核验（`verify::verify_project`，守护测试用）同样从这里出发：目录命中且种类一致 →
//! 链路引用的例程已登记 → 依赖的表/视图/序列在内核存在 → 声明该入口的用例场景可执行。
//! 声明口径一律来自 `spinduck.toml`，代码零硬编码（§6）。

use crate::error::EngineError;
use crate::project::CaseKind;

/// 一条入口声明（`[[entry]]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryConfig {
    /// 例程全名（归一小写，`package.procedure` 形态）
    pub routine: String,
    /// `function` | `procedure`（缺省 `procedure`；核验时与目录登记种类比对）
    pub kind: CaseKind,
}

impl EntryConfig {
    /// 从一条 `[[entry]]` 表解析；`routine` 必填且必须带包限定。
    pub fn from_toml_value(v: &toml::Value) -> Result<Self, EngineError> {
        let raw = v
            .get("routine")
            .and_then(|x| x.as_str())
            .ok_or_else(|| EngineError::env(30, "spinduck.toml: [[entry]].routine 必填"))?;
        let kind = match v
            .get("kind")
            .and_then(|x| x.as_str())
            .unwrap_or("procedure")
        {
            "function" => CaseKind::Function,
            "procedure" => CaseKind::Procedure,
            other => {
                return Err(EngineError::env(
                    30,
                    format!("spinduck.toml: [[entry]].kind 不支持 '{other}'（function|procedure）"),
                ))
            }
        };
        Ok(Self {
            routine: qualified_routine(raw)?,
            kind,
        })
    }
}

/// 归一并校验例程全名：小写；必须含包限定（`package.procedure`）。
///
/// 口径理由：裸过程名在语料里可能同名不同包，链路校验必须按全名定位。
pub(crate) fn qualified_routine(raw: &str) -> Result<String, EngineError> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.contains('.') {
        return Ok(lower);
    }
    Err(EngineError::env(
        30,
        format!("入口/例程全名必须为 package.procedure 形态，收到：'{raw}'"),
    ))
}
