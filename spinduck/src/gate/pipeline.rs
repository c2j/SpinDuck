//! Gate 编排（设计 V4 §5.3 Stage 1–3）：文件集 → 逐文件解析 →
//! `SchemaSnapshot`（Stage 2）→ 对象/列引用检查（Stage 3A）。
//! 产出 `BIZ-0152` 前形态的 `GateFinding`；诊断渲染、`(file,line,
//! col)` 位置归一与 `[[missing]]` 去重由 CLI 编排层负责。
//!
//! Stage 1 口径：Tokenizer 失败的文件计入 `parse_failed`（其
//! Gate 1A 错误由 `eng.register` 路径产出，本层不重复报）；
//! `Parser::parse` 无致命错误返回（内部错误恢复），解析产物
//! 为空/降级的兜底由 register 的静默丢弃护栏（`ENG-0064`）负责。
//! 解析失败文件不贡献 Stage 2 符号——其余文件可能因此出现
//! Missing，这是设计确认的时序语义（先 1A 后建快照）。

use ogsql_parser::ast::visitor::{walk_statement, Visitor, VisitorResult};
use ogsql_parser::ast::Statement;
use ogsql_parser::parser::Parser;
use ogsql_parser::token::tokenizer::Tokenizer;

use crate::error::EngineError;

use super::check::check_statement;
use super::columns::{check_columns, MissingColumn};
use super::schema::{object_key, SchemaSnapshot};

/// 一个待检源文件（`rel` 为项目相对路径，仅用于诊断归属）。
#[derive(Debug)]
pub struct GateFile {
    pub rel: String,
    pub sql: String,
}

/// 单条 Gate 发现（`BIZ-0152` 前的中间形态）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GateFinding {
    /// 项目相对路径。
    pub file: String,
    /// 归一对象名（`object_key` 口径；列级为 `列名` 或 `qualifier.列名`）。
    pub object: String,
    /// 问题类别：对象级为 `MissingRef.kind`（`"table"`/`"view"`），
    /// 列级为 `MissingColumn.problem`（`"missing"`/`"ambiguous"`/
    /// `"missing_table"`）。
    pub kind: String,
}

/// Stage 1–3 执行结果。
#[derive(Debug, Default)]
pub struct GateRun {
    /// 去重排序后的发现列表。
    pub findings: Vec<GateFinding>,
    /// Stage 2 DDL 语义冲突（`ENG-0070`）及其归属文件；`Some` 时
    /// `findings` 为空（快照不完整时列/对象检查没有可信依据）。
    pub schema_error: Option<(String, EngineError)>,
    /// Tokenizer 失败的文件（Gate 1A 词法错误由 register 路径报出）。
    pub parse_failed: Vec<String>,
    /// Gate 2A 顶层 DDL 拒绝（`ENG-0071`）：`(文件, 错误)`，
    /// 覆盖 register 通道之外的纯 DDL 文件。
    pub rejected: Vec<(String, EngineError)>,
    /// Stage 2 产物快照：Gate 2B stub DDL 的输入；`schema_error`
    /// 为 `Some` 时为 `None`（快照不完整，不可用于 prepare）。
    pub snapshot: Option<crate::gate::schema::SchemaSnapshot>,
}

/// 执行 Stage 1–4。`files` 顺序即处理顺序；快照按文件序演化，
/// 输出排序保证确定性。
pub fn run(files: &[GateFile]) -> GateRun {
    let mut parsed: Vec<(&str, Vec<Statement>)> = Vec::with_capacity(files.len());
    let mut parse_failed = Vec::new();
    // Stage 1.5：Gate 2A 顶层 DDL 拒绝——纯 DDL 文件不经 eng.register，
    // 拒绝清单必须在此覆盖，否则不支持的 DDL 会被静默放过。
    let mut rejected = Vec::new();
    for f in files {
        match Tokenizer::new(&f.sql).tokenize() {
            Ok(tokens) => {
                let stmts = Parser::new(tokens).parse();
                for st in &stmts {
                    if let Err(e) = super::reject_unsupported_ddl(st) {
                        rejected.push((f.rel.clone(), e));
                    }
                }
                parsed.push((f.rel.as_str(), stmts));
            }
            Err(_) => parse_failed.push(f.rel.clone()),
        }
    }
    // Stage 2：逐文件应用 DDL，冲突可归属到具体文件
    let mut snapshot = SchemaSnapshot::default();
    for (rel, stmts) in &parsed {
        for st in stmts {
            if let Err(e) = snapshot.apply(st) {
                return GateRun {
                    findings: Vec::new(),
                    schema_error: Some((rel.to_string(), e)),
                    parse_failed,
                    rejected,
                    snapshot: None,
                };
            }
        }
    }
    let mut findings = Vec::new();
    for (rel, stmts) in &parsed {
        // 文件级 PL 临时表集合：`CREATE [TEMP] TABLE .. AS` 可出现在
        // 例程体内（SqlStatement 下钻），会话级对象不进全局快照，
        // 但同文件内引用视为已知（列集合延迟到 Gate 2B）。
        let local = temp_tables(stmts);
        for st in stmts {
            collect_refs(rel, st, &snapshot, &local, &mut findings);
            collect_cols(rel, st, &snapshot, &local, &mut findings);
        }
    }
    findings.sort();
    findings.dedup();
    GateRun {
        findings,
        schema_error: None,
        parse_failed,
        rejected,
        snapshot: Some(snapshot),
    }
}

/// 收集语句树（含 PL 体 `SqlStatement` 下钻）中声明的 TEMP 表名。
/// `CREATE TEMP TABLE t AS ..` 的列集合不可静态确定，引用方按
/// "对象已知、列延迟"处理（与全局 CTAS 的 `columns_deferred` 同口径）。
fn temp_tables(stmts: &[Statement]) -> std::collections::BTreeSet<String> {
    struct TempNames(std::collections::BTreeSet<String>);
    impl Visitor for TempNames {
        fn visit_statement(&mut self, st: &Statement) -> VisitorResult {
            let key = match st {
                Statement::CreateTableAs(t) if t.temporary => Some(object_key(&t.name)),
                Statement::CreateTable(t) if t.temporary => Some(object_key(&t.name)),
                _ => None,
            };
            if let Some(k) = key {
                self.0.insert(k);
            }
            VisitorResult::Continue
        }
    }
    let mut v = TempNames(std::collections::BTreeSet::new());
    for st in stmts {
        walk_statement(&mut v, st);
    }
    v.0
}

fn collect_refs(
    rel: &str,
    st: &Statement,
    snap: &SchemaSnapshot,
    local: &std::collections::BTreeSet<String>,
    out: &mut Vec<GateFinding>,
) {
    for m in check_statement(st, snap, local) {
        out.push(GateFinding {
            file: rel.to_string(),
            object: m.object,
            kind: m.kind.to_string(),
        });
    }
}

fn collect_cols(
    rel: &str,
    st: &Statement,
    snap: &SchemaSnapshot,
    local: &std::collections::BTreeSet<String>,
    out: &mut Vec<GateFinding>,
) {
    for m in check_columns(st, snap, local) {
        out.push(GateFinding {
            file: rel.to_string(),
            object: col_object(&m),
            kind: m.problem.to_string(),
        });
    }
}

/// 列级发现的对象名：`qualifier.column` 或裸 `column`。
fn col_object(m: &MissingColumn) -> String {
    match &m.qualifier {
        Some(q) => format!("{q}.{}", m.column),
        None => m.column.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str, sql: &str) -> GateFile {
        GateFile {
            rel: rel.to_string(),
            sql: sql.to_string(),
        }
    }

    #[test]
    fn should_report_missing_object_and_column_across_files() {
        let run = run(&[
            file("ddl/t1.sql", "CREATE TABLE t1(a INT, b INT);"),
            file(
                "pkg/p1.sql",
                "CREATE PROCEDURE p IS BEGIN \
                 INSERT INTO ghost_tbl SELECT bad_col FROM t1; END;",
            ),
        ]);
        assert!(run.schema_error.is_none());
        let objs: Vec<(&str, &str, &str)> = run
            .findings
            .iter()
            .map(|f| (f.file.as_str(), f.object.as_str(), f.kind.as_str()))
            .collect();
        assert!(
            objs.contains(&("pkg/p1.sql", "ghost_tbl", "table")),
            "{objs:?}"
        );
        assert!(
            objs.contains(&("pkg/p1.sql", "bad_col", "missing")),
            "{objs:?}"
        );
    }

    #[test]
    fn should_surface_schema_conflict_as_eng_0070() {
        let run = run(&[
            file("a.sql", "CREATE TABLE t1(a INT);"),
            file("b.sql", "CREATE TABLE t1(b INT);"),
        ]);
        let (rel, err) = run.schema_error.expect("应报 ENG-0070");
        assert_eq!(err.code.to_string(), "ENG-0070", "{err:?}");
        assert_eq!(rel, "b.sql", "冲突应归属到后定义的文件");
        assert!(run.findings.is_empty(), "快照失败时不产列级发现");
    }

    #[test]
    fn should_skip_unparseable_file_but_keep_others() {
        let run = run(&[
            file("bad.sql", "INSERT INTO t VALUES ('unterminated"),
            file("ok.sql", "CREATE TABLE t1(a INT);"),
        ]);
        assert_eq!(run.parse_failed, ["bad.sql".to_string()]);
        assert!(run.schema_error.is_none());
    }

    #[test]
    fn should_dedup_repeated_findings() {
        let run = run(&[file(
            "p.sql",
            "CREATE PROCEDURE p IS BEGIN \
             INSERT INTO ghost SELECT * FROM ghost; END;",
        )]);
        let n = run.findings.iter().filter(|f| f.object == "ghost").count();
        assert_eq!(n, 1, "{:?}", run.findings);
    }
}
