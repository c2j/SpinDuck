//! 语料部署（`verify` 的前置动作）：把项目声明的资产装进内核。
//!
//! 口径与黄金 harness 一致（tests/common/mod.rs）：表 DDL 去 `DEFAULT` 后走
//! 同一改写管线；视图用占位体（存在性核验，不参与语义）；桩包与例程整文件注册；
//! 数据集种子按语句下发。差异只在目的：verify 不接 GaussDB，纯本地内核。

use crate::engine::Engine;
use crate::error::EngineError;
use crate::exec::BindMap;
use crate::project::{CaseConfig, ProjectConfig};
use crate::rules::RuleRegistry;
use crate::verify::run::{kernel_objects, verify_entry, verify_scenarios, ChainFacts, EntryReport};

/// 部署并核验整个项目（`verify` 子命令的唯一入口，需 `native`）。
///
/// 顺序：表 DDL → 占位视图 → `sys_dummy`（A 模式单行表等价物）→ 桩包与例程注册
/// → 数据集种子；随后按 `[[entry]]` 逐条核验链路，并对声明了入口的用例执行场景。
/// 部署期任何一步失败都直接返回错误（fail fast，不产出半成品报告）。
#[cfg(feature = "native")]
pub fn verify_project(project: &ProjectConfig) -> Result<Vec<EntryReport>, EngineError> {
    if project.entries.is_empty() {
        return Err(EngineError::env(
            30,
            "spinduck.toml: 至少一条 [[entry]]（verify 的核验起点）",
        ));
    }
    let rules = RuleRegistry::default().with_project_rules(&project.rules_files)?;
    let ex = crate::sql::DuckExecutor::open_in_memory()?;
    let mut eng = Engine::new(Box::new(ex), rules);
    deploy_project(&mut eng, project)?;
    let objects = kernel_objects(eng.ex.as_ref())?;
    let facts = ChainFacts::from_engine(&eng, objects);
    let mut reports: Vec<EntryReport> = project
        .entries
        .iter()
        .map(|e| verify_entry(e, &facts))
        .collect();
    let cases = load_cases(project)?;
    verify_scenarios(&mut eng, &cases, &mut reports);
    Ok(reports)
}

/// 用例声明（`*.case.toml`，按 `[cases].include` 口径）。
fn load_cases(project: &ProjectConfig) -> Result<Vec<CaseConfig>, EngineError> {
    project
        .case_files()?
        .iter()
        .map(|p| CaseConfig::load(p))
        .collect()
}

/// 部署项目资产到内核（表 → 占位视图 → 例程 → 种子）。
pub fn deploy_project(eng: &mut Engine, project: &ProjectConfig) -> Result<(), EngineError> {
    let funcs = eng.rules.functions.clone();
    for f in project.ddl_files()? {
        let text = std::fs::read_to_string(&f)
            .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", f.display())))?;
        for unit in sql_units(&strip_defaults(&text)) {
            let rewritten =
                crate::sql::rewrite_tokens_with_rules(&unit, &Default::default(), &funcs)?;
            let sql = crate::sql::apply_ast_transforms(&rewritten.text)?;
            eng.ex.execute(&sql, &BindMap::new()).map_err(|e| {
                EngineError::kernel(70, format!("建表 {}: {}", f.display(), e.message))
            })?;
        }
    }
    for f in project.view_files()? {
        let text = std::fs::read_to_string(&f)
            .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", f.display())))?;
        let name = view_name(&text);
        if name.is_empty() {
            continue;
        }
        // 占位视图：核验的是"链路依赖的视图在语料里有声明"，视图体语义归黄金层
        eng.ex
            .execute(
                &format!("CREATE OR REPLACE VIEW {name} AS SELECT 1"),
                &BindMap::new(),
            )
            .map_err(|e| EngineError::kernel(71, format!("占位视图 {name}: {}", e.message)))?;
    }
    eng.ex.execute(
        "CREATE TABLE IF NOT EXISTS sys_dummy AS SELECT 1",
        &BindMap::new(),
    )?;
    let mut sources = project.stub_files()?;
    sources.extend(project.routine_files()?);
    for f in sources {
        let text = std::fs::read_to_string(&f)
            .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", f.display())))?;
        eng.register(&text)
            .map_err(|e| EngineError::env(30, format!("注册 {}: {}", f.display(), e.message)))?;
    }
    for f in project.dataset_files()? {
        let text = std::fs::read_to_string(&f)
            .map_err(|e| EngineError::env(30, format!("读取 {}: {e}", f.display())))?;
        for unit in sql_units(&text) {
            eng.ex.execute(&unit, &BindMap::new()).map_err(|e| {
                EngineError::kernel(72, format!("种子 {}: {}", f.display(), e.message))
            })?;
        }
    }
    Ok(())
}

/// 去 `DEFAULT (…)` 子句（与黄金 harness 同口径：DuckDB 对带括号默认值 + 逗号解析不稳）。
fn strip_defaults(ddl: &str) -> String {
    let mut out = String::with_capacity(ddl.len());
    for line in ddl.lines() {
        let up = line.to_ascii_uppercase();
        if up.contains("DEFAULT") && line.contains('(') {
            if let Some(pos) = up.find(" DEFAULT") {
                out.push_str(line[..pos].trim_end());
                out.push_str(",\n");
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// 视图名（`… VIEW <name> …` 首个非关键字标识符）。
fn view_name(text: &str) -> String {
    let up = text.to_ascii_uppercase();
    let mut from = 0usize;
    while let Some(i) = up[from..].find("VIEW ") {
        let at = from + i + 5;
        let word = text[at..]
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(word.as_str(), "if" | "exists" | "as" | "or" | "replace") {
            return word;
        }
        from = at;
    }
    String::new()
}

/// SQL 文本 → 可下发语句（Oracle `/` 分块 + 顶层 `;` 切分，引号/注释感知）。
///
/// 注释（`--`、`/* */`）整体剔除但保留换行，避免相邻语句粘连；
/// 字符串字面量内的 `;` 与 `''` 转义按字面处理；双引号标识符同理。
pub fn sql_units(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in oracle_chunks(text) {
        for unit in statements_in(&chunk) {
            let t = unit.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// Oracle SQL*Plus 形态：独占一行的 `/` 是语句块分隔符。
fn oracle_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.trim() == "/" {
            if !cur.trim().is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// 块内按顶层 `;` 切分（注释剔除、字面量保护）。
fn statements_in(chunk: &str) -> Vec<String> {
    let b = chunk.as_bytes();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i] as char;
        if c == '-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] as char != '\n' {
                i += 1;
            }
            buf.push('\n');
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            buf.push(' ');
            continue;
        }
        if c == '\'' || c == '"' {
            let end = literal_end(b, i, c);
            buf.push_str(&chunk[i..end]);
            i = end;
            continue;
        }
        if c == ';' {
            out.push(std::mem::take(&mut buf));
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    out.push(buf);
    out
}

/// 字面量结束位置（下标）：`''`/`""` 视为内部转义，继续向后吃。
fn literal_end(b: &[u8], start: usize, quote: char) -> usize {
    let q = quote as u8;
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_split_oracle_chunks_and_top_level_semicolons() {
        let text = "-- 注释里有 ; 号\nCREATE TABLE t (a INT);\nCREATE VIEW v AS SELECT 1;\n/\nINSERT INTO t VALUES ('x;y');\n";
        assert_eq!(
            sql_units(text),
            vec![
                "CREATE TABLE t (a INT)",
                "CREATE VIEW v AS SELECT 1",
                "INSERT INTO t VALUES ('x;y')",
            ],
            "`/` 分块、顶层 `;` 切分；注释与字符串里的分号不算"
        );
    }
}
