//! 运行计划（design §6.1）：把"一次闭包求值所需的全部对象与文件"落成可复现的清单。
//!
//! 计划是**数据**，不是执行结果：`plan` 生成并落盘，`deploy` 消费（A2-3）。
//! 字节可复现是这里的承诺——同一份输入两次生成必须逐字节相同，所以路径清单一律
//! 排序去重、段序与字段序固定、不写日期、不写绝对路径。

pub mod closure;
pub mod codeweb;
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

/// 一条闭包对象引用（`[[ref]]`）。`kind` / `role` 的合法取值在读侧校验。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanRef {
    pub object: String,
    /// `table` | `view` | `sequence` | `routine`。
    pub kind: String,
    /// `read` | `write` | `type` | `call`。
    pub role: String,
    pub from: String,
    /// `<相对路径>:<行号>`，行号原样保留（含 0）。
    pub at: String,
}

/// 一条动态 SQL（`[[dynamic]]`）。`raw` 是 Debug 文本，可以含换行。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanDynamic {
    pub raw: String,
    pub from: String,
    pub at: String,
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
    /// 闭包内对象引用。空则不写出 `[[ref]]`。
    pub refs: Vec<PlanRef>,
    /// 动态 SQL。空则不写出 `[[dynamic]]`。
    pub dynamic: Vec<PlanDynamic>,
    pub missing: Vec<MissingObject>,
}

impl RunPlan {
    /// 唯一序列化口径（`plan` 落盘与测试断言共用）。
    ///
    /// 手写而非 `toml::to_string_pretty`：后者把 `[[missing]]` 排在 `[plan]` 之前、
    /// 且按元素数换行，计划文件是人要读、要 diff 的工件，段序与字段序必须固定
    /// （读侧 [`RunPlan::from_toml_str`] 走 `toml` 解析，两者构成一对往返口径）。
    ///
    /// 清单、引用、动态 SQL 与缺件都在渲染前排序去重：字节复现不能依赖生成侧的遍历顺序。
    /// 空的 `[[ref]]` / `[[dynamic]]` / `[[missing]]` 不写出，避免旧计划多出空段。
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

        push_refs(&mut s, &self.refs);
        push_dynamic(&mut s, &self.dynamic);
        push_missing(&mut s, &self.missing);
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
        Ok(Self {
            target,
            generator,
            project_dir,
            schema,
            ddl_routines: source_list(sources, "ddl_routines"),
            ddl_tables: source_list(sources, "ddl_tables"),
            materialize: source_list(sources, "materialize"),
            writeback: source_list(sources, "writeback"),
            refs: parse_refs(&v)?,
            dynamic: parse_dynamic(&v)?,
            missing: parse_missing(&v)?,
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

/// TOML 基础字符串。反斜杠与双引号始终转义；换行、回车、制表符只在出现时写成 `\n` `\r` `\t`，
/// 不含后三者的字符串与旧口径逐字节相同。
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn push_refs(out: &mut String, refs: &[PlanRef]) {
    let uniq: std::collections::BTreeSet<&PlanRef> = refs.iter().collect();
    for r in uniq {
        out.push_str("\n[[ref]]\n");
        out.push_str(&format!("object = {}\n", quoted(&r.object)));
        out.push_str(&format!("kind = {}\n", quoted(&r.kind)));
        out.push_str(&format!("role = {}\n", quoted(&r.role)));
        out.push_str(&format!("from = {}\n", quoted(&r.from)));
        out.push_str(&format!("at = {}\n", quoted(&r.at)));
    }
}

fn push_dynamic(out: &mut String, dynamic: &[PlanDynamic]) {
    let uniq: std::collections::BTreeSet<&PlanDynamic> = dynamic.iter().collect();
    for d in uniq {
        out.push_str("\n[[dynamic]]\n");
        out.push_str(&format!("raw = {}\n", quoted(&d.raw)));
        out.push_str(&format!("from = {}\n", quoted(&d.from)));
        out.push_str(&format!("at = {}\n", quoted(&d.at)));
    }
}

fn push_missing(out: &mut String, missing: &[MissingObject]) {
    let uniq: std::collections::BTreeSet<&MissingObject> = missing.iter().collect();
    for m in uniq {
        out.push_str("\n[[missing]]\n");
        out.push_str(&format!("object = {}\n", quoted(&m.object)));
        out.push_str(&format!("from = {}\n", quoted(&m.from)));
    }
}

fn source_list(sources: Option<&toml::Value>, key: &str) -> Vec<String> {
    sources
        .and_then(|s| s.get(key))
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn table_entries(root: &toml::Value, key: &str) -> Vec<toml::Value> {
    root.get(key)
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

fn entry_field(entry: &toml::Value, section: &str, key: &str) -> Result<String, EngineError> {
    entry
        .get(key)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| EngineError::env(30, format!("计划 [[{section}]] 缺少 {key}")))
}

fn parse_missing(root: &toml::Value) -> Result<Vec<MissingObject>, EngineError> {
    let mut missing = Vec::new();
    for entry in table_entries(root, "missing") {
        missing.push(MissingObject {
            object: entry_field(&entry, "missing", "object")?,
            from: entry_field(&entry, "missing", "from")?,
        });
    }
    Ok(missing)
}

fn parse_refs(root: &toml::Value) -> Result<Vec<PlanRef>, EngineError> {
    let mut refs = Vec::new();
    for entry in table_entries(root, "ref") {
        let kind = entry_field(&entry, "ref", "kind")?;
        if !matches!(kind.as_str(), "table" | "view" | "sequence" | "routine") {
            return Err(EngineError::env(
                30,
                format!("计划 [[ref]].kind 非法: {kind}"),
            ));
        }
        let role = entry_field(&entry, "ref", "role")?;
        if !matches!(role.as_str(), "read" | "write" | "type" | "call") {
            return Err(EngineError::env(
                30,
                format!("计划 [[ref]].role 非法: {role}"),
            ));
        }
        refs.push(PlanRef {
            object: entry_field(&entry, "ref", "object")?,
            kind,
            role,
            from: entry_field(&entry, "ref", "from")?,
            at: entry_field(&entry, "ref", "at")?,
        });
    }
    Ok(refs)
}

fn parse_dynamic(root: &toml::Value) -> Result<Vec<PlanDynamic>, EngineError> {
    let mut dynamic = Vec::new();
    for entry in table_entries(root, "dynamic") {
        dynamic.push(PlanDynamic {
            raw: entry_field(&entry, "dynamic", "raw")?,
            from: entry_field(&entry, "dynamic", "from")?,
            at: entry_field(&entry, "dynamic", "at")?,
        });
    }
    Ok(dynamic)
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
