//! 一次导出解析成瘦图，再按入口各求一份闭包并映射为 `RunPlan`。
//!
//! 不调用 `plan::closure::build`，也不按入口各起一次子进程。

use super::export::CodewebExport;
use super::{closure_from_graph, parse_codeweb_reader, ClosureContext, GraphClosure};
use crate::error::EngineError;
use crate::plan::closure::{ddl_name_index, project_dir_text};
use crate::plan::{MissingObject, PlanDynamic, PlanRef, RunPlan};
use crate::project::ProjectConfig;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 新计划的产出方字面量。不写日期，也不写 spinduck 自己的版本。
pub const GENERATOR: &str = "codeweb 0.9.7";

/// 导出并解析一次，再对每个入口调用 [`closure_from_graph`]。
pub fn plans_from_export(
    project: &ProjectConfig,
    entries: &[String],
    schema: Option<String>,
    export: &impl CodewebExport,
) -> Result<Vec<RunPlan>, EngineError> {
    let codeweb_root = codeweb_project_root(project)?;
    let mut reader = export.export_json(&codeweb_root)?;
    let parsed = parse_codeweb_reader(reader.as_mut());
    drain(reader.as_mut())?;
    let graph = parsed.map_err(export_failure)?;
    let ddl = ddl_name_index(project)?;
    let schema = lowered_schema(schema);
    let ctx = ClosureContext {
        project_root: &project.root,
        ddl_files: &ddl,
    };
    let mut plans = Vec::with_capacity(entries.len());
    for entry in entries {
        let closure = closure_from_graph(&graph, entry, &ctx)?;
        plans.push(plan_from_closure(project, closure, schema.clone()));
    }
    Ok(plans)
}

/// `--codeweb-project` 由调用方预先写进 [`ProjectConfig::codeweb_dir`]，其次才向上找。
pub(crate) fn codeweb_project_root(project: &ProjectConfig) -> Result<PathBuf, EngineError> {
    if let Some(dir) = &project.codeweb_dir {
        return Ok(dir.clone());
    }
    project
        .root
        .ancestors()
        .find(|dir| dir.join("codeweb.toml").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            EngineError::env(
                30,
                format!("未找到 codeweb 项目: {}", project.root.display()),
            )
        })
}

fn drain(reader: &mut dyn Read) -> Result<(), EngineError> {
    match std::io::copy(reader, &mut std::io::sink()) {
        Ok(_) => Ok(()),
        Err(e) => Err(export_failure(EngineError::env(30, e.to_string()))),
    }
}

fn export_failure(err: EngineError) -> EngineError {
    if err.message.starts_with("codeweb export 失败:") {
        return err;
    }
    if let Some(at) = err.message.find("codeweb export 失败:") {
        return EngineError::env(30, err.message[at..].to_string());
    }
    EngineError::env(30, format!("codeweb export 失败: {}", err.message))
}

fn lowered_schema(schema: Option<String>) -> Option<String> {
    // 只写入计划，不参与名字归一：整段小写，点分段全部保留。
    schema.map(|s| s.trim().to_ascii_lowercase())
}

fn plan_from_closure(
    project: &ProjectConfig,
    closure: GraphClosure,
    schema: Option<String>,
) -> RunPlan {
    RunPlan {
        target: closure.target,
        generator: GENERATOR.to_string(),
        project_dir: project_dir_text(project),
        schema,
        ddl_routines: closure.ddl_routines,
        ddl_tables: closure.ddl_tables,
        materialize: Vec::new(),
        writeback: Vec::new(),
        refs: sorted_by(closure.refs.into_iter().map(into_ref)),
        dynamic: sorted_by(closure.dynamic.into_iter().map(into_dynamic)),
        missing: sorted_by(closure.missing.into_iter().map(into_missing)),
    }
}

fn into_ref(r: super::ClosureRef) -> PlanRef {
    PlanRef {
        object: r.object,
        kind: r.kind,
        role: r.role,
        from: r.from,
        at: r.at,
    }
}

fn into_dynamic(d: super::ClosureDynamic) -> PlanDynamic {
    PlanDynamic {
        raw: d.raw,
        from: d.from,
        at: d.at,
    }
}

fn into_missing(m: super::ClosureMissing) -> MissingObject {
    MissingObject {
        object: m.object,
        from: m.from,
    }
}

fn sorted_by<T: Ord>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let set: BTreeSet<T> = items.into_iter().collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run::compile_with_export;
    use std::cell::Cell;
    use std::path::Path;

    struct FakeExport {
        calls: Cell<usize>,
        json: Vec<u8>,
    }

    impl CodewebExport for FakeExport {
        fn export_json(&self, _project: &Path) -> Result<Box<dyn Read + Send>, EngineError> {
            self.calls.set(self.calls.get() + 1);
            Ok(Box::new(std::io::Cursor::new(self.json.clone())))
        }
    }

    const NULL_PROC: &str = "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;\n";
    const FORALL_PROC: &str =
        "CREATE OR REPLACE PROCEDURE p IS BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;\n";

    fn load_project(root: &Path, extra: &str) -> ProjectConfig {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("spinduck.toml"),
            format!("[project]\nname = \"p\"\nkernel = \"1.5.5\"\n{extra}\n"),
        )
        .unwrap();
        ProjectConfig::load(root).unwrap()
    }

    fn write_sql(root: &Path, rel: &str, body: &str) -> String {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().replace('\\', "/")
    }

    fn compile(
        project: &ProjectConfig,
        json: serde_json::Value,
        entries: &[&str],
        schema: Option<&str>,
    ) -> (i32, String, usize) {
        if project.codeweb_dir.is_none() && codeweb_project_root(project).is_err() {
            std::fs::write(project.root.join("codeweb.toml"), "").unwrap();
        }
        let fake = FakeExport {
            calls: Cell::new(0),
            json: serde_json::to_vec(&json).unwrap(),
        };
        let entries: Vec<String> = entries.iter().map(|s| (*s).to_string()).collect();
        let mut buf = Vec::new();
        let code = compile_with_export(
            project,
            &entries,
            schema.map(str::to_string),
            &fake,
            &mut buf,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        (code, String::from_utf8(buf).unwrap(), fake.calls.get())
    }

    fn proc_node(id: usize, schema: &str, name: &str, file: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "procedure",
            "name": name,
            "schema": schema,
            "file": file,
            "line": 1
        })
    }

    #[test]
    fn should_export_once_for_two_entries_and_lowercase_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = write_sql(root, "sql/routines/pkg.sql", NULL_PROC);
        let project = load_project(root, "[paths]\nhome = \"x/y\"\n");
        let json = serde_json::json!({
            "nodes": [
                proc_node(1, "pkg_a", "prc_a", &file),
                proc_node(2, "pkg_b", "prc_b", &file)
            ],
            "edges": []
        });
        let (code, out, calls) = compile(
            &project,
            json,
            &["pkg_a.prc_a", "pkg_b.prc_b"],
            Some(" A.B.C "),
        );
        assert_eq!(calls, 1, "两个入口必须共用一次导出");
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("compile: entries=2 missing=0 failed=0 dynamic=0"),
            "{out}"
        );
        for target in ["pkg_a.prc_a", "pkg_b.prc_b"] {
            let text =
                std::fs::read_to_string(root.join(format!("x/y/plan/{target}.toml"))).unwrap();
            assert!(text.contains("generator = \"codeweb 0.9.7\""), "{text}");
            assert!(text.contains("schema = \"a.b.c\""), "{text}");
            assert!(text.contains("project_dir = \"../../..\""), "{text}");
            assert!(text.contains("materialize = []"), "{text}");
            assert!(!text.contains("[[missing]]"), "{text}");
        }
    }

    #[test]
    fn should_exit_1_and_write_missing_without_counting_a_builtin_edge() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = write_sql(root, "sql/routines/pkg_a.sql", NULL_PROC);
        let project = load_project(root, "");
        let missing = serde_json::json!({
            "nodes": [
                proc_node(1, "pkg_a", "prc_main", &file),
                proc_node(2, "pkg_a", "prc_child", &file),
                proc_node(3, "pkg_a", "prc_zeta", &file),
                {"id": 4, "type": "unresolved", "raw_expr": "fnc_get_fund_data_mode"}
            ],
            "edges": [
                {"source": 1, "target": 3, "type": "direct", "file": file, "line": 0},
                {"source": 1, "target": 2, "type": "direct", "file": file, "line": 0},
                {"source": 1, "target": 4, "type": "direct", "file": file, "line": 0},
                {"source": 1, "target": 1, "type": "dynamic", "raw_expr": "PlVariable([\"v_sql\"])", "file": file, "line": 0}
            ]
        });
        let (code, out, calls) = compile(&project, missing, &["pkg_a.prc_main"], None);
        assert_eq!(calls, 1);
        assert_eq!(code, 1, "{out}");
        assert!(
            out.contains("plan: target=pkg_a.prc_main routines=1 tables=0 missing=1"),
            "{out}"
        );
        let miss = out
            .find("MISS routine:fnc_get_fund_data_mode <- pkg_a.prc_main")
            .unwrap();
        let child = out
            .find("REF routine:pkg_a.prc_child call <- pkg_a.prc_main @ sql/routines/pkg_a.sql:0")
            .unwrap();
        let zeta = out
            .find("REF routine:pkg_a.prc_zeta call <- pkg_a.prc_main @ sql/routines/pkg_a.sql:0")
            .unwrap();
        let dyn_at = out
            .find("DYN PlVariable([\"v_sql\"]) <- pkg_a.prc_main @ sql/routines/pkg_a.sql:0")
            .unwrap();
        assert!(miss < child && child < zeta && zeta < dyn_at, "{out}");
        let plan =
            std::fs::read_to_string(root.join(".spinduck/plan/pkg_a.prc_main.toml")).unwrap();
        assert!(plan.contains("[[missing]]"), "{plan}");
        assert!(
            plan.contains("object = \"routine:fnc_get_fund_data_mode\""),
            "{plan}"
        );
        assert!(
            plan.find("prc_child").unwrap() < plan.find("prc_zeta").unwrap(),
            "{plan}"
        );
    }

    #[test]
    fn should_not_record_a_builtin_edge_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = write_sql(root, "sql/routines/pkg_a.sql", NULL_PROC);
        let project = load_project(root, "");
        let builtin = serde_json::json!({
            "nodes": [
                proc_node(1, "pkg_a", "prc_main", &file),
                {"id": 2, "type": "builtin_function", "name": "coalesce"}
            ],
            "edges": [
                {"source": 1, "target": 2, "type": "uses_builtin_function", "file": file, "line": 0},
                {"source": 1, "target": 2, "type": "direct", "file": file, "line": 0}
            ]
        });
        let (code, out, calls) = compile(&project, builtin, &["pkg_a.prc_main"], None);
        assert_eq!(calls, 1);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("missing=0"), "{out}");
        assert!(!out.contains("MISS "), "{out}");
        let plan =
            std::fs::read_to_string(root.join(".spinduck/plan/pkg_a.prc_main.toml")).unwrap();
        assert!(!plan.contains("[[missing]]"), "{plan}");
    }

    #[test]
    fn should_write_the_plan_then_print_ir_failure_after_the_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = write_sql(root, "sql/routines/pkg_a.sql", FORALL_PROC);
        let project = load_project(root, "");
        let json = serde_json::json!({
            "nodes": [proc_node(1, "pkg_a", "prc_main", &file)],
            "edges": []
        });
        let (code, out, _) = compile(&project, json, &["pkg_a.prc_main"], None);
        assert_eq!(code, 1, "{out}");
        let plan_at = out.find("plan: target=pkg_a.prc_main").unwrap();
        let fail_at = out.find("FAIL sql/routines/pkg_a.sql  [ENG-0022]").unwrap();
        let sum_at = out
            .find("compile: entries=1 missing=0 failed=1 dynamic=0")
            .unwrap();
        assert!(plan_at < fail_at && fail_at < sum_at, "{out}");
        let plan =
            std::fs::read_to_string(root.join(".spinduck/plan/pkg_a.prc_main.toml")).unwrap();
        assert!(!plan.contains("[[missing]]"), "{plan}");
        assert!(!plan.contains("ENG-0022"), "{plan}");
    }

    #[test]
    fn should_not_write_a_plan_when_the_entry_is_absent_from_the_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = write_sql(root, "sql/routines/pkg_a.sql", NULL_PROC);
        let project = load_project(root, "");
        std::fs::write(root.join("codeweb.toml"), "").unwrap();
        let fake = FakeExport {
            calls: Cell::new(0),
            json: serde_json::to_vec(&serde_json::json!({
                "nodes": [proc_node(1, "pkg_a", "prc_main", &file)],
                "edges": []
            }))
            .unwrap(),
        };
        let err = compile_with_export(
            &project,
            &["pkg_a.prc_other".to_string()],
            None,
            &fake,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(err.code.to_string(), "BIZ-0150");
        assert_eq!(fake.calls.get(), 1);
        assert!(!root.join(".spinduck/plan/pkg_a.prc_other.toml").exists());
    }

    #[test]
    fn should_prefer_paths_codeweb_then_walk_up_for_codeweb_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("app");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(tmp.path().join("codeweb.toml"), "").unwrap();
        let walked = load_project(&root, "");
        assert_eq!(codeweb_project_root(&walked).unwrap(), tmp.path());
        let pinned = load_project(
            &root,
            "[paths]\ncodeweb = \"nested\"\nextra = \"ignored\"\n",
        );
        assert_eq!(
            pinned.codeweb_dir.as_deref(),
            Some(root.join("nested").as_path())
        );
        assert_eq!(codeweb_project_root(&pinned).unwrap(), root.join("nested"));
        let bare = tempfile::tempdir().unwrap();
        std::fs::write(
            bare.path().join("spinduck.toml"),
            "[project]\nname = \"p\"\n",
        )
        .unwrap();
        let project = ProjectConfig::load(bare.path()).unwrap();
        let err = codeweb_project_root(&project).unwrap_err();
        assert!(
            err.message.starts_with("未找到 codeweb 项目:"),
            "{}",
            err.message
        );
    }
}
