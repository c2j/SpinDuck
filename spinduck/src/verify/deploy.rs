//! 语料部署（`verify` 的前置动作）：把项目声明的资产装进内核。
//!
//! [`deploy_schema`] / [`deploy_routines`] 是被测侧的**唯一**部署口径：`verify` 与
//! 黄金 harness（tests/common/mod.rs）都调它们，两侧不再有各自的建表/建视图/注册实现。
//! 差异只在目的：verify 额外下发本地种子并逐入口核验链路，harness 改为从参照侧物化数据。

use crate::engine::Engine;
use crate::error::EngineError;
use crate::exec::BindMap;
use crate::project::{CaseConfig, ProjectConfig};
use crate::rules::RuleRegistry;
use crate::verify::run::{kernel_objects, verify_entry, verify_scenarios, ChainFacts, EntryReport};
use std::path::{Path, PathBuf};

/// `deploy_schema` 自建的表：A 模式单行表等价物，项目里没有对应的 DDL 文件。
///
/// 唯一口径：新增自建对象只改这里，[`deploy_schema`] 的下发文本按它生成。
/// `plan` 的静态闭包也读同一份清单——按"无声明即缺件"处理会对每个使用者报假缺件。
pub const ENGINE_PROVIDED_TABLES: &[&str] = &["sys_dummy"];

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

/// 部署项目资产到内核（schema → 例程 → 种子）。
///
/// 三段各自独立可复用：黄金 harness 只取前两段（数据由参照侧物化，不跑本地种子）。
pub fn deploy_project(eng: &mut Engine, project: &ProjectConfig) -> Result<(), EngineError> {
    deploy_schema(eng, project)?;
    deploy_routines(eng, project)?;
    deploy_seeds(eng, project)
}

/// schema 部署：表/序列 DDL → 视图（视图体优先）→ `sys_dummy`。
///
/// 部署期归一化只三处，且都是内核不支持、不改变语义的形态：`DEFAULT (…)` 子句、
/// 超 BIGINT 的序列边界、`CACHE` 选项——语料文件保持逐字，裁剪发生在下发文本上。
pub fn deploy_schema(eng: &mut Engine, project: &ProjectConfig) -> Result<(), EngineError> {
    let funcs = eng.rules.functions.clone();
    for f in project.ddl_files()? {
        deploy_ddl_file(eng, &f, &funcs)?;
    }
    for f in project.view_files()? {
        deploy_view_file(eng, &f, &funcs)?;
    }
    ensure_engine_provided_tables(eng)
}

/// 单个表/序列 DDL 文件下发（`deploy_schema` 与计划面 `deploy` 共用同一条口径）。
pub(crate) fn deploy_ddl_file(
    eng: &mut Engine,
    file: &Path,
    funcs: &std::collections::BTreeMap<String, String>,
) -> Result<(), EngineError> {
    let text = crate::source::read_text(file)?;
    for unit in sql_units(&strip_defaults(&text)) {
        let unit = drop_cache_option(&clamp_sequence_bounds(&unit));
        let rewritten = crate::sql::rewrite_tokens_with_rules(&unit, &Default::default(), funcs)?;
        let sql = crate::sql::apply_ast_transforms(&rewritten.text)?;
        eng.ex.execute(&sql, &BindMap::new()).map_err(|e| {
            EngineError::kernel(70, format!("建表 {}: {}", file.display(), e.message))
        })?;
    }
    Ok(())
}

/// 单个视图文件下发（视图体优先，失败退占位）。
pub(crate) fn deploy_view_file(
    eng: &mut Engine,
    file: &Path,
    funcs: &std::collections::BTreeMap<String, String>,
) -> Result<(), EngineError> {
    let text = crate::source::read_text(file)?;
    deploy_view(eng, &text, funcs)
}

/// 引擎自建对象：无源文件，`plan` 的闭包与这里读同一份 [`ENGINE_PROVIDED_TABLES`]。
pub(crate) fn ensure_engine_provided_tables(eng: &mut Engine) -> Result<(), EngineError> {
    for table in ENGINE_PROVIDED_TABLES {
        eng.ex.execute(
            &format!("CREATE TABLE IF NOT EXISTS {table} AS SELECT 1"),
            &BindMap::new(),
        )?;
    }
    Ok(())
}

/// 例程部署：桩包与例程源码逐文件注册。
pub fn deploy_routines(eng: &mut Engine, project: &ProjectConfig) -> Result<(), EngineError> {
    let mut sources = project.stub_files()?;
    sources.extend(project.routine_files()?);
    register_routine_files(eng, &sources)
}

/// 逐文件注册例程源码（`deploy_routines` 与计划面 `deploy` 共用的唯一实现）。
pub(crate) fn register_routine_files(
    eng: &mut Engine,
    files: &[PathBuf],
) -> Result<(), EngineError> {
    for f in files {
        let text = crate::source::read_text(f)?;
        eng.register(&text)
            .map_err(|e| EngineError::env(30, format!("注册 {}: {}", f.display(), e.message)))?;
    }
    Ok(())
}

/// 数据集种子：`cases/datasets/*.sql` 按语句下发。
pub fn deploy_seeds(eng: &mut Engine, project: &ProjectConfig) -> Result<(), EngineError> {
    for f in project.dataset_files()? {
        let text = crate::source::read_text(&f)?;
        for unit in sql_units(&text) {
            eng.ex.execute(&unit, &BindMap::new()).map_err(|e| {
                EngineError::kernel(72, format!("种子 {}: {}", f.display(), e.message))
            })?;
        }
    }
    Ok(())
}

/// 视图部署：视图体优先，内核不接受时才退回占位视图。
///
/// 口径（设计 §12.1）：链路 SQL 会读视图的列，`SELECT 1` 占位只能证明"对象存在"，
/// 不能证明可执行；因此先按与表 DDL 同一条改写管线下发视图体，失败（GaussDB 专属
/// 函数、`/*FORCE*/` 之外的内核方言片段等）才降级为占位——核验项仍 PASS，视图体
/// 语义正确性归黄金层比对。
fn deploy_view(
    eng: &mut Engine,
    text: &str,
    funcs: &std::collections::BTreeMap<String, String>,
) -> Result<(), EngineError> {
    let name = view_name(text);
    if name.is_empty() {
        return Ok(());
    }
    for unit in sql_units(text) {
        if !creates_view(&unit) {
            continue;
        }
        let Ok(unit) = crate::sql::rewrite_tokens_with_rules(&unit, &Default::default(), funcs)
        else {
            continue;
        };
        let Ok(sql) = crate::sql::apply_ast_transforms(&unit.text) else {
            continue;
        };
        if eng.ex.execute(&sql, &BindMap::new()).is_ok() {
            return Ok(());
        }
    }
    eng.ex
        .execute(
            &format!("CREATE OR REPLACE VIEW {name} AS SELECT 1"),
            &BindMap::new(),
        )
        .map(|_| ())
        .map_err(|e| EngineError::kernel(71, format!("占位视图 {name}: {}", e.message)))
}

/// 这条语句是不是 `CREATE [OR REPLACE] … VIEW`——判定走 [`crate::verify::tokens`]，
/// 与链路抽取同一把词法刀，注释与字符串字面量都不参与。
///
/// 按原文 `contains` 会漏掉语料的 `CREATE OR REPLACE /*FORCE*/ VIEW`（考题 9 个
/// `1_V_*.sql`）：判定不成立 ⇒ 循环体一条都不下发，库里只剩 `SELECT 1` 占位视图，
/// 视图体的列根本不存在（V29.4 ① 那条教训的部署面对偶）。
fn creates_view(unit: &str) -> bool {
    let toks = crate::verify::tokens(&unit.to_ascii_lowercase());
    let words: Vec<&str> = toks
        .iter()
        .filter_map(|t| match t {
            crate::verify::Tok::Word(w) => Some(w.as_str()),
            crate::verify::Tok::Punct(_) => None,
        })
        .collect();
    let direct = ["create", "view"];
    let replaceable = ["create", "or", "replace", "view"];
    words.windows(2).any(|w| w == &direct[..]) || words.windows(4).any(|w| w == &replaceable[..])
}

/// 去 `DEFAULT (…)` 子句（与黄金 harness 同口径：DuckDB 对带括号默认值 + 逗号解析不稳）。
pub fn strip_defaults(ddl: &str) -> String {
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

/// 序列选项子句的形态（决定剔除范围）。
enum OptionForm {
    /// 只认 `<KW> <整数>`，且该值判为要剪时才剪（`NO MAXVALUE` 一类不动）
    ValueOnly,
    /// 内核整体不支持该选项：`<KW> <整数>`、`<KW>`、`NO <KW>` 都剪
    DropWhole,
}

/// 序列边界裁剪（部署期归一化，与 [`strip_defaults`] 同族：语料保持逐字）。
///
/// DuckDB 的 `MINVALUE` / `MAXVALUE` 只接受 BIGINT 范围内的整数，语料里 27 位的
/// GaussDB 任意精度上界会让内核直接解析失败。**语义损失**：超出 BIGINT 的边界
/// 不再约束（实际取用量远低于 2^63，不影响结果等价）；范围内的一字不改。
pub fn clamp_sequence_bounds(unit: &str) -> String {
    cut_or_keep(
        unit,
        &["MINVALUE", "MAXVALUE"],
        OptionForm::ValueOnly,
        &|arg| arg.parse::<i64>().is_err(),
    )
}

/// 剔除 `CACHE <n>` / `NO CACHE`（DuckDB `CREATE SEQUENCE` 无该选项）。
///
/// **语义损失**：无——`CACHE` 只影响内核预分配号段的节奏，不改变取号顺序与值域。
pub fn drop_cache_option(unit: &str) -> String {
    cut_or_keep(unit, &["CACHE"], OptionForm::DropWhole, &|_arg| true)
}

/// 在 `CREATE SEQUENCE` 语句内按关键字剔除选项子句；无需剔除时原样返回。
///
/// 剔除后把空白压平为单空格——序列 DDL 不含字符串字面量，压平只影响排版。
fn cut_or_keep(
    unit: &str,
    kws: &[&str],
    form: OptionForm,
    drop_arg: &dyn Fn(&str) -> bool,
) -> String {
    let up = unit.to_ascii_uppercase();
    if !up.contains("CREATE SEQUENCE") {
        return unit.to_string();
    }
    let b = up.as_bytes();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for kw in kws {
        let mut at = 0usize;
        while let Some(rel) = up[at..].find(kw) {
            let i = at + rel;
            at = i + kw.len();
            if !is_token_at(&up, b, i, kw) {
                continue;
            }
            let after = &up[i + kw.len()..];
            let arg = token_prefix(after.trim_start());
            let numeric = !arg.is_empty() && arg.bytes().all(|c| c.is_ascii_digit());
            if numeric && drop_arg(arg) {
                let lead = after.len() - after.trim_start().len();
                cuts.push((i, i + kw.len() + lead + arg.len()));
            } else if matches!(form, OptionForm::DropWhole) {
                // 无数字参数形态（`CACHE` / `NO CACHE`）：连可能的 `NO` 一起剪，
                // 后随的下一个选项关键字不属于本子句，不能吃掉
                let start = preceding_no(&up, i).unwrap_or(i);
                cuts.push((start, i + kw.len()));
            }
        }
    }
    if cuts.is_empty() {
        return unit.to_string();
    }
    cuts.sort_unstable();
    let mut kept = String::with_capacity(unit.len());
    let mut pos = 0usize;
    for (s, e) in cuts {
        kept.push_str(&unit[pos..s]);
        pos = e;
    }
    kept.push_str(&unit[pos..]);
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `at` 之前紧邻的标识符恰为 `NO` 时，返回它的起始下标。
fn preceding_no(up: &str, at: usize) -> Option<usize> {
    let head = up[..at].trim_end();
    let start = head.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))? + 1;
    (&head[start..] == "NO").then_some(start)
}

/// `kw` 在 `at` 处是否为独立标识符（前后不接标识符字符）。
fn is_token_at(up: &str, b: &[u8], at: usize, kw: &str) -> bool {
    if !up[at..].starts_with(kw) {
        return false;
    }
    let prev_ok = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_');
    let after = at + kw.len();
    let next_ok = after >= b.len() || !(b[after].is_ascii_alphanumeric() || b[after] == b'_');
    prev_ok && next_ok
}

fn token_prefix(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
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

    #[test]
    fn should_drop_sequence_bounds_outside_bigint_and_keep_the_rest() {
        let seq = "CREATE SEQUENCE seq_external_no\n    START WITH 70000000001\n    INCREMENT BY 1\n    MINVALUE 70000000001\n    MAXVALUE 999999999999999999999999999\n    NO CYCLE\n    CACHE 1";
        let out = clamp_sequence_bounds(seq);
        assert!(
            !out.contains("MAXVALUE 9999999"),
            "溢出 BIGINT 的上界必须裁剪（内核直接解析失败）：{out}"
        );
        assert!(
            out.contains("MINVALUE 70000000001"),
            "可容纳的下界不得动：{out}"
        );
        assert!(out.contains("START WITH 70000000001"), "起点不得动：{out}");
        assert!(out.contains("NO CYCLE"), "其余选项不得动：{out}");
        assert_eq!(
            clamp_sequence_bounds("CREATE SEQUENCE s MAXVALUE 9223372036854775807 NO CYCLE"),
            "CREATE SEQUENCE s MAXVALUE 9223372036854775807 NO CYCLE",
            "BIGINT 范围内的边界一字不改"
        );
        assert_eq!(
            clamp_sequence_bounds("CREATE TABLE t (a BIGINT);"),
            "CREATE TABLE t (a BIGINT);",
            "非序列语句一字不改"
        );
    }

    #[test]
    fn should_drop_cache_option_only_from_sequence_ddl() {
        let seq = "CREATE SEQUENCE seq_dispatch_job_id\n    START WITH 20001\n    INCREMENT BY 1\n    NO CYCLE\n    CACHE 1";
        let out = drop_cache_option(seq);
        assert!(
            !out.contains("CACHE 1"),
            "内核不认 CACHE 选项，须整子句剔除：{out}"
        );
        assert!(out.contains("NO CYCLE"), "支持的选项一字不改：{out}");
        assert!(out.contains("START WITH 20001"), "起点不得动：{}", out);
        assert_eq!(
            drop_cache_option("CREATE SEQUENCE s NO CACHE INCREMENT BY 2"),
            "CREATE SEQUENCE s INCREMENT BY 2",
            "NO CACHE 形态连同 NO 一起剔除，空白压平"
        );
        assert_eq!(
            drop_cache_option("CREATE TABLE cache_t (a INT);"),
            "CREATE TABLE cache_t (a INT);",
            "非序列语句一字不改（表名里含 cache 也不算）"
        );
    }
}
