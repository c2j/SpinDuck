//! 研发态项目结构（§12.1 `spinduck.toml` 项目模式）：项目声明 + 用例声明的识别。
//!
//! 引擎与 CLI 只通过本模块定位业务资产（DDL/例程/桩/数据集/基线），
//! 路径与用例定义一律来自 `spinduck.toml` / `*.case.toml`，代码零硬编码。
//! 黄金对比的 harness（tests/golden_*）即本结构的首个消费者。

use crate::error::EngineError;
use std::path::{Path, PathBuf};

/// 目录内单次读取上限（防止误把大文件当配置）。
const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// 项目声明（`spinduck.toml` 中引擎消费的字段；未识别的段落忽略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    /// 目标内核版本（M1 锚定；与规则文件 probe_duckdb 一致性由 rules probe 校验）
    pub kernel: String,
    pub ddl_dir: PathBuf,
    pub routines_dir: PathBuf,
    pub stubs_dirs: Vec<PathBuf>,
    pub rules_files: Vec<PathBuf>,
    pub probes_dir: PathBuf,
    pub datasets_dir: PathBuf,
    pub baselines_dir: PathBuf,
    pub home_dir: PathBuf,
    /// `[cases] include` 的条目（如 `cases/*.case.toml`）
    pub case_includes: Vec<String>,
    /// 项目根（相对路径基准）
    pub root: PathBuf,
}

impl ProjectConfig {
    /// 读取项目声明；相对路径以项目根为基准解析。
    pub fn load(root: &Path) -> Result<Self, EngineError> {
        let text = read_text(&root.join("spinduck.toml"))?;
        Self::from_toml_str(&text, root)
    }

    /// 从 TOML 文本解析（root 为相对路径基准）。
    pub fn from_toml_str(text: &str, root: &Path) -> Result<Self, EngineError> {
        let v: toml::Value = toml::from_str(text).map_err(|e| cfg_err("spinduck.toml", e))?;
        let project = v
            .get("project")
            .ok_or_else(|| EngineError::env(30, "spinduck.toml: 缺少 [project] 段"))?;
        let name = project
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| EngineError::env(30, "spinduck.toml: [project].name 必填"))?;
        let kernel = project
            .get("kernel")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        let paths = v
            .get("paths")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        let get_path = |key: &str, default: &str| -> PathBuf {
            paths
                .get(key)
                .and_then(|x| x.as_str())
                .map(|s| root.join(s))
                .unwrap_or_else(|| root.join(default))
        };
        let stubs_dirs = match paths.get("stubs") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| root.join(s))
                .collect(),
            Some(toml::Value::String(s)) => vec![root.join(s)],
            _ => Vec::new(),
        };
        let rules_files = match paths.get("rules") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| root.join(s))
                .collect(),
            Some(toml::Value::String(s)) => vec![root.join(s)],
            _ => Vec::new(),
        };
        let cases = v
            .get("cases")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        let case_includes = match cases.get("include") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(String::from)
                .collect(),
            _ => vec!["cases/*.case.toml".to_string()],
        };
        Ok(Self {
            name: name.to_string(),
            kernel: kernel.to_string(),
            ddl_dir: get_path("ddl", "sql/ddl"),
            routines_dir: get_path("routines", "sql/routines"),
            stubs_dirs,
            rules_files,
            probes_dir: get_path("probes", "probes"),
            datasets_dir: get_path("datasets", "cases/datasets"),
            baselines_dir: get_path("baselines", "cases/baselines"),
            home_dir: get_path("home", ".spinduck"),
            case_includes,
            root: root.to_path_buf(),
        })
    }

    /// 表/序列 DDL（`sql/ddl/*.sql` 与 `sql/ddl/tables/*.sql`，排序合并）。
    ///
    /// 约定：表在 `sql/ddl/` 或 `sql/ddl/tables/`（两侧都建）；
    /// 视图在 `sql/ddl/views/`（两侧都建，被测侧走同一改写管线）。
    pub fn ddl_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        let mut out = sql_files(&self.ddl_dir, false)?;
        out.extend(sql_files(&self.ddl_dir.join("tables"), false)?);
        out.sort();
        Ok(out)
    }

    /// 视图源码（`sql/ddl/views/*.sql`）：只用于参照侧部署。
    pub fn view_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        sql_files(&self.ddl_dir.join("views"), false)
    }

    /// 例程源码（`sql/routines/*.sql`，排序）。
    pub fn routine_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        sql_files(&self.routines_dir, false)
    }

    /// 桩包源码（`[paths].stubs` 各目录的 `*.sql`，按目录顺序拼接）。
    pub fn stub_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        let mut out = Vec::new();
        for d in &self.stubs_dirs {
            out.extend(sql_files(d, false)?);
        }
        Ok(out)
    }

    /// 用例定义（`[cases].include` 中 `*.case.toml`；当前仅支持 `dir/*.suffix` 形态）。
    pub fn case_files(&self) -> Result<Vec<PathBuf>, EngineError> {
        let mut out = Vec::new();
        for pat in &self.case_includes {
            let Some((dir_part, file_pat)) = pat.rsplit_once('/') else {
                continue;
            };
            let dir = self.root.join(dir_part);
            let suffix = file_pat.trim_start_matches('*');
            let mut files = list_dir_suffix(&dir, suffix)?;
            out.append(&mut files);
        }
        out.sort();
        Ok(out)
    }
}

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

fn str_list(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn cfg_err(what: &str, e: toml::de::Error) -> EngineError {
    EngineError::env(30, format!("{what} 解析失败: {e}"))
}

fn read_text(path: &Path) -> Result<String, EngineError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", path.display())))?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(EngineError::env(
            30,
            format!("{} 超过配置读取上限（1MiB）", path.display()),
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", path.display())))
}

/// 目录内 `*.sql`（非递归，按文件名排序；目录不存在返回空）。
fn sql_files(dir: &Path, _rec: bool) -> Result<Vec<PathBuf>, EngineError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = list_dir_suffix(dir, "sql")?;
    files.sort();
    Ok(files)
}

fn list_dir_suffix(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>, EngineError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let rd = std::fs::read_dir(dir)
        .map_err(|e| EngineError::env(30, format!("read_dir {}: {e}", dir.display())))?;
    let mut out = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| EngineError::env(30, format!("dir entry: {e}")))?;
        let p = ent.path();
        let name = p.file_name().and_then(|x| x.to_str()).unwrap_or_default();
        // 后缀按文件名结尾匹配：支持多段后缀（`.case.toml`）与简单扩展名（`sql`）
        let matched = if suffix.starts_with('.') {
            name.ends_with(suffix)
        } else {
            name.ends_with(&format!(".{suffix}"))
        };
        if p.is_file() && matched {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    const PROJ: &str = r#"
[project]
name = "demo-procs"
kernel = "1.5.5"

[paths]
ddl = "sql/ddl"
routines = "sql/routines"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
probes = "probes"
home = ".spinduck"

[cases]
datasets = "cases/datasets"
baselines = "cases/baselines"
include = ["cases/*.case.toml"]
"#;

    #[test]
    fn should_load_project_paths_and_case_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "spinduck.toml", PROJ);
        write(root, "sql/ddl/tables.sql", "CREATE TABLE t(a INT);");
        write(root, "sql/ddl/views/v.sql", "CREATE VIEW v AS SELECT 1;");
        write(
            root,
            "sql/routines/r1.sql",
            "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;",
        );
        write(
            root,
            "sql/stubs/s1.sql",
            "CREATE OR REPLACE PACKAGE pk AS END;",
        );
        write(root, "rules/spinduck-rules.toml", "[functions]\n");
        write(root, "cases/datasets/seed.sql", "INSERT INTO t VALUES (1);");
        write(root, "cases/baselines/expected.json", "{}");
        write(
            root,
            "cases/x.case.toml",
            r#"
[case]
name = "x"
kind = "procedure"
routine = "p"
baseline = "expected.json"
expect_succeed = "0"
[case.output]
sql = "SELECT a FROM t ORDER BY a"
[[case.scenarios]]
why = "单行"
args = ["'20240105'"]
"#,
        );

        let p = ProjectConfig::load(root).unwrap();
        assert_eq!(p.name, "demo-procs");
        assert_eq!(p.kernel, "1.5.5");
        assert_eq!(p.ddl_files().unwrap().len(), 1, "子目录视图不算表 DDL");
        assert_eq!(p.view_files().unwrap().len(), 1);
        assert_eq!(p.routine_files().unwrap().len(), 1);
        assert_eq!(p.stub_files().unwrap().len(), 1);
        assert_eq!(p.rules_files.len(), 1);
        assert_eq!(p.case_files().unwrap().len(), 1);

        let c = CaseConfig::load(&p.case_files().unwrap()[0]).unwrap();
        assert_eq!(c.name, "x");
        assert_eq!(c.kind, CaseKind::Procedure);
        assert_eq!(c.routine, "p");
        assert_eq!(c.scenarios.len(), 1);
        assert_eq!(c.scenarios[0].args, vec!["'20240105'".to_string()]);
        assert_eq!(c.output_sql.as_deref(), Some("SELECT a FROM t ORDER BY a"));
        assert_eq!(c.expect_succeed.as_deref(), Some("0"));
        assert_eq!(c.baseline, "expected.json");
        assert!(c.reference_routine.is_none());
        assert!(c.materialize.is_empty());
    }

    #[test]
    fn should_default_optional_paths_when_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "spinduck.toml", "[project]\nname = \"m\"\n");
        let p = ProjectConfig::load(root).unwrap();
        assert_eq!(p.ddl_dir, root.join("sql/ddl"));
        assert_eq!(p.routines_dir, root.join("sql/routines"));
        assert_eq!(p.datasets_dir, root.join("cases/datasets"));
        assert_eq!(p.baselines_dir, root.join("cases/baselines"));
        assert_eq!(p.case_includes, vec!["cases/*.case.toml".to_string()]);
        assert!(p.stubs_dirs.is_empty());
        assert!(p.rules_files.is_empty());
    }

    #[test]
    fn should_reject_procedure_case_without_output_sql() {
        let e = CaseConfig::from_toml_str(
            r#"
[case]
name = "bad"
kind = "procedure"
routine = "p"
[[case.scenarios]]
why = "x"
args = []
"#,
        )
        .unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0031");
    }

    #[test]
    fn should_parse_function_case_without_output_sql() {
        let c = CaseConfig::from_toml_str(
            r#"
[case]
name = "f"
kind = "function"
routine = "fnc_x"
[[case.scenarios]]
why = "命中"
args = ["'a'", "1"]
[[case.scenarios]]
why = "未命中"
args = ["'b'", "2"]
"#,
        )
        .unwrap();
        assert_eq!(c.kind, CaseKind::Function);
        assert_eq!(c.scenarios.len(), 2);
        assert!(c.output_sql.is_none());
        assert_eq!(c.baseline, "expected.json", "默认基线名");
    }

    #[test]
    fn should_report_missing_project_section() {
        let e =
            ProjectConfig::from_toml_str("[paths]\nddl = \"x\"\n", Path::new("/tmp")).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0030");
        assert!(e.message.contains("[project]"));
    }
}
