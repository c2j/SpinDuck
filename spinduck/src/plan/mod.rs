//! 运行计划（design §6.1）：把"一次闭包求值所需的全部对象与文件"落成可复现的清单。
//!
//! 计划是**数据**，不是执行结果：`plan` 生成并落盘，`deploy` 消费（A2-3）。
//! 字节可复现是这里的承诺——同一份输入两次生成必须逐字节相同，所以路径清单一律
//! 排序去重、段序与字段序固定、不写日期、不写绝对路径。

pub mod closure;
pub mod deploy;

#[cfg(test)]
mod tests;

pub(crate) use closure::kernel_gaps;
pub use closure::{build, plan_dir};

use crate::error::EngineError;
use std::path::{Path, PathBuf};

/// 计划 schema 版本（读侧据此拒绝未来形态，不猜字段含义）。
pub const SCHEMA_VERSION: u32 = 1;

/// 一条缺件（`[[missing]]`）：非空即拒绝执行（design §6.1、R3）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MissingObject {
    /// `relation:<归一名>` / `sequence:<归一名>` / `routine:<归一名>`。
    ///
    /// 用 `relation` 而非 `table`：链路抽取（`src/verify/chain/`）把表与视图归同一个
    /// 类别，没有同口径的区分能力，计划不声称自己没有的信息。
    pub object: String,
    /// 引用它的例程全名（归一小写），用于"缺件从哪来"的定位。
    pub from: String,
}

/// 一次运行计划的完整清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// 目标例程全名（归一小写；独立函数用其可定位全名）。
    pub target: String,
    /// 产出方与版本，红时先怀疑口径。
    pub generator: String,
    /// 项目根相对**计划文件所在目录**的路径（可移植：不含绝对路径）。
    pub project_dir: String,
    /// 对象所属 schema（仅在调用方给出 `--schema` 时写入）。
    pub schema: Option<String>,
    /// 闭包内例程源（相对 `project_dir`）。
    pub ddl_routines: Vec<String>,
    /// 闭包内表/视图/序列 DDL（相对 `project_dir`）。
    pub ddl_tables: Vec<String>,
    /// 只读物化清单（A3 的输入；`plan` 生成时恒空，由人或 `--from-codeweb` 填）。
    pub materialize: Vec<String>,
    /// 回写白名单（A4 的输入；**默认空**，§6.4 安全门禁）。
    pub writeback: Vec<String>,
    pub missing: Vec<MissingObject>,
}

impl RunPlan {
    /// 唯一序列化口径（`plan` 落盘与测试断言共用）。
    ///
    /// 手写而非 `toml::to_string_pretty`：后者把 `[[missing]]` 排在 `[plan]` 之前、
    /// 且按元素数换行，计划文件是人要读、要 diff 的工件，段序与字段序必须固定
    /// （读侧 [`RunPlan::from_toml_str`] 走 `toml` 解析，两者构成一对往返口径）。
    ///
    /// 三份清单与缺件表都在渲染前排序去重：字节复现不能依赖生成侧的遍历顺序。
    pub fn to_toml_string(&self) -> String {
        let mut s = String::from("[plan]\n");
        s.push_str(&format!("schema_version = {SCHEMA_VERSION}\n"));
        s.push_str(&format!("target = {}\n", quoted(&self.target)));
        s.push_str(&format!("generator = {}\n", quoted(&self.generator)));
        s.push_str(&format!("project_dir = {}\n", quoted(&self.project_dir)));
        if let Some(schema) = &self.schema {
            s.push_str(&format!("schema = {}\n", quoted(schema)));
        }

        s.push_str("\n[sources]\n");
        for (key, items) in [
            ("ddl_routines", &self.ddl_routines),
            ("ddl_tables", &self.ddl_tables),
            ("materialize", &self.materialize),
            ("writeback", &self.writeback),
        ] {
            let list = sorted_list(items);
            s.push_str(&format!("{key} = {list}\n"));
        }

        let uniq: std::collections::BTreeSet<&MissingObject> = self.missing.iter().collect();
        for m in &uniq {
            s.push_str("\n[[missing]]\n");
            s.push_str(&format!("object = {}\n", quoted(&m.object)));
            s.push_str(&format!("from = {}\n", quoted(&m.from)));
        }
        s
    }

    /// 读侧（`deploy` 的输入，design §6.2）：解析 [`RunPlan::to_toml_string`] 落盘的文本。
    ///
    /// 只认当前 [`SCHEMA_VERSION`]：版本不符即 ENV-0030 拒绝，不猜未来字段的含义。
    /// 必填项缺位同样拒绝——半张计划建出半个环境，比不建更糟。
    pub fn from_toml_str(text: &str) -> Result<Self, EngineError> {
        let v: toml::Value = toml::from_str(text)
            .map_err(|e| EngineError::env(30, format!("计划文件解析失败: {e}")))?;
        let plan = v
            .get("plan")
            .ok_or_else(|| EngineError::env(30, "计划缺少 [plan] 段"))?;
        let version = plan
            .get("schema_version")
            .and_then(|x| x.as_integer())
            .ok_or_else(|| EngineError::env(30, "计划缺少 [plan].schema_version"))?;
        if version != SCHEMA_VERSION as i64 {
            return Err(EngineError::env(
                30,
                format!("计划 schema_version={version}，本程序只认 {SCHEMA_VERSION}"),
            ));
        }
        let required = |key: &str| -> Result<String, EngineError> {
            plan.get(key)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .ok_or_else(|| EngineError::env(30, format!("计划缺少 [plan].{key}")))
        };
        let target = required("target")?;
        let generator = required("generator")?;
        let project_dir = required("project_dir")?;
        let schema = plan
            .get("schema")
            .and_then(|x| x.as_str())
            .map(str::to_string);

        let sources = v.get("sources");
        let list = |key: &str| -> Vec<String> {
            sources
                .and_then(|s| s.get(key))
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut missing = Vec::new();
        for entry in v
            .get("missing")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let field = |key: &str| -> Result<String, EngineError> {
                entry
                    .get(key)
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| EngineError::env(30, format!("计划 [[missing]] 缺少 {key}")))
            };
            missing.push(MissingObject {
                object: field("object")?,
                from: field("from")?,
            });
        }

        Ok(Self {
            target,
            generator,
            project_dir,
            schema,
            ddl_routines: list("ddl_routines"),
            ddl_tables: list("ddl_tables"),
            materialize: list("materialize"),
            writeback: list("writeback"),
            missing,
        })
    }

    /// 项目根：`project_dir` 相对**计划文件所在目录**（[`RunPlan::to_toml_string`] 的写侧口径）。
    pub fn project_root(&self, plan_path: &Path) -> PathBuf {
        plan_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(self.project_dir.as_str())
    }
}

/// TOML 基础字符串字面量（只转义反斜杠与双引号，其余按 UTF-8 原样）。
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 排序去重后的内联数组文本（计划里的路径一律 `/` 分隔、相对 `project_dir`）。
fn sorted_list(items: &[String]) -> String {
    let uniq = std::collections::BTreeSet::from_iter(items.iter().cloned());
    let rendered = uniq
        .iter()
        .map(|p| quoted(p))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{rendered}]")
}
