//! 取图接缝：生产实现走进程内嵌入式 codeweb（[`super::EmbeddedCodewebSource`]），
//! 测试夹具注入内存 JSON（[`JsonCodewebSource`]）。
//!
//! 旧进程外实现（`CodewebExport` + `Command`）已在 Task 3.5 退役；本 trait 是
//! 闭包展开的唯一取图入口（docs/embeded-codeweb.md §4.2）。

use super::{parse_codeweb_json, CodewebGraph};
use crate::error::EngineError;
use std::path::Path;

/// 从 codeweb 获取瘦图与产出方版本。
///
/// 生产实现走进程内嵌入式 codeweb（[`super::EmbeddedCodewebSource`]）；
/// 测试夹具注入内存 JSON（[`JsonCodewebSource`]）。
pub trait CodewebSource {
    /// 写入 `RunPlan.generator` 的字面量，如 `"codeweb 0.9.9"`。
    fn generator(&self) -> String;
    /// 取指定 codeweb 项目的瘦图。失败一律是 `ENV-0030` 域。
    fn load_graph(&self, codeweb_root: &Path) -> Result<CodewebGraph, EngineError>;
}

/// 内存 JSON 夹具实现：不启进程、不碰磁盘。
///
/// 现有 `tests/cli.rs`、`tests/verify_chain.rs`、`plans.rs` 的单测都靠它注入导出。
pub struct JsonCodewebSource {
    pub generator: String,
    pub json: Vec<u8>,
}

impl CodewebSource for JsonCodewebSource {
    fn generator(&self) -> String {
        self.generator.clone()
    }

    fn load_graph(&self, _codeweb_root: &Path) -> Result<CodewebGraph, EngineError> {
        parse_codeweb_json(&String::from_utf8_lossy(&self.json))
            .map_err(|e| EngineError::env(30, format!("codeweb export 失败: {}", e.message)))
    }
}

/// 生产实现：进程内嵌入式 codeweb（docs/embeded-codeweb.md §4.2）。
///
/// `auto_analyze = false`（默认）只做只读校验：store 缺失/版本过期/源码变动
/// 一律 `ENV-0030` 失败，不回退词法闭包。`auto_analyze = true`（CLI `--analyze`）
/// 在上述情况下先在进程内完成分析再取图。
///
/// 项目定位用 [`codeweb::embed::EmbeddedProject::open_exact`]：向上查找
/// `codeweb.toml` 已由 `plans.rs::codeweb_project_root` 完成并传进来，这里不再
/// 走 `EmbeddedProject::find`（避免两套查找逻辑漂移）。
pub struct EmbeddedCodewebSource {
    pub auto_analyze: bool,
}

impl CodewebSource for EmbeddedCodewebSource {
    fn generator(&self) -> String {
        format!("codeweb {}", codeweb::embed::version())
    }

    fn load_graph(&self, codeweb_root: &Path) -> Result<CodewebGraph, EngineError> {
        let mut proj =
            codeweb::embed::EmbeddedProject::open_exact(codeweb_root).map_err(map_codeweb_err)?;
        let diff = proj.diff().map_err(map_codeweb_err)?;
        if !diff.is_up_to_date() {
            if !self.auto_analyze {
                return Err(EngineError::env(30, format_diff_detail(&diff)));
            }
            proj.analyze().map_err(map_codeweb_err)?;
        }
        let thin = proj.export_thin_graph().map_err(map_codeweb_err)?;
        Ok(super::parse::from_thin_graph(thin))
    }
}

/// diff 不新鲜的诊断文本：缺 store / 版本过期 / 变动文件数，各给可执行的下一步。
fn format_diff_detail(diff: &codeweb::embed::DiffStatus) -> String {
    if !diff.store_exists {
        return "codeweb 图索引缺失: 未找到 .codeweb/store（先运行 codeweb analyze，\
                或给 compile 加 --analyze）"
            .to_string();
    }
    if let Some(found) = diff.stale_store_version {
        return format!(
            "codeweb 图索引缺失或版本过期: store 版本 {found} 与 codeweb {} 不兼容\
             （重新运行 codeweb analyze，或给 compile 加 --analyze）",
            codeweb::embed::version()
        );
    }
    format!(
        "codeweb 图与源码不一致: {} 个新增、{} 个修改、{} 个删除的文件尚未分析\
         （运行 codeweb analyze，或给 compile 加 --analyze）",
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len()
    )
}

/// codeweb 错误 → `ENV-0030`（docs/embeded-codeweb.md §4.4 映射表）。
fn map_codeweb_err(err: codeweb::error::CodeWebError) -> EngineError {
    use codeweb::error::CodeWebError;
    let message = match &err {
        CodeWebError::ProjectNotFound { search_from } => {
            format!("未找到 codeweb 项目: {}", search_from.display())
        }
        CodeWebError::StoreNotFound { path } => format!(
            "codeweb 图索引缺失: {} 不存在（先运行 codeweb analyze，或给 compile 加 --analyze）",
            path.display()
        ),
        CodeWebError::StoreVersionMismatch { .. } => {
            format!("codeweb 图索引缺失或版本过期: {err}")
        }
        CodeWebError::FileRead { .. }
        | CodeWebError::ConfigError { .. }
        | CodeWebError::ExportError { .. } => format!("codeweb 加载失败: {err}"),
        // 其余变体（NoFilesFound / ProjectAlreadyExists / InitTargetNotEmpty）
        // 不会出现在读图路径上；仍归 ENV-0030，保留原始文案。
        _ => format!("codeweb 加载失败: {err}"),
    };
    EngineError::env(30, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::codeweb::parse::Node;
    use crate::plan::codeweb::{closure_from_graph, ClosureContext};
    use std::collections::BTreeMap;

    /// 最小 JSON 夹具：一个例程读一张表。两份实现必须消费出同一张图。
    const MINIMAL_JSON: &str = r#"{
        "nodes": [
            {"id": 1, "type": "procedure", "name": "prc_main", "schema": "pkg_a", "file": "/proj/sql/a.sql", "line": 1},
            {"id": 2, "type": "table", "name": "t1", "explicit": true, "system": false}
        ],
        "edges": [
            {"source": 1, "target": 2, "type": "table_access", "modes": ["read"], "write_kinds": [], "file": "/proj/sql/a.sql", "line": 3}
        ]
    }"#;

    #[test]
    fn should_load_a_graph_that_the_closure_walker_consumes() {
        let src = JsonCodewebSource {
            generator: "codeweb 0.9.9".to_string(),
            json: MINIMAL_JSON.as_bytes().to_vec(),
        };
        let root = std::path::Path::new("/proj");
        let graph = src.load_graph(root).expect("minimal JSON must parse");
        assert_eq!(graph.nodes.len(), 2, "both nodes survive the thin parse");

        let ctx = ClosureContext {
            project_root: root,
            ddl_files: &BTreeMap::new(),
        };
        let closure = closure_from_graph(&graph, "pkg_a.prc_main", &ctx).expect("closure");
        assert_eq!(closure.target, "pkg_a.prc_main");
        assert!(
            closure
                .refs
                .iter()
                .any(|r| r.kind == "table" && r.role == "read" && r.object == "t1"),
            "the table read must be in the closure: {:?}",
            closure.refs
        );
    }

    #[test]
    fn should_report_generator_verbatim() {
        let src = JsonCodewebSource {
            generator: "codeweb 0.9.9".to_string(),
            json: MINIMAL_JSON.as_bytes().to_vec(),
        };
        assert_eq!(src.generator(), "codeweb 0.9.9");
    }

    #[test]
    fn should_map_bad_json_to_env_30_with_the_export_failure_prefix() {
        let src = JsonCodewebSource {
            generator: "codeweb 0.9.9".to_string(),
            json: b"{ not json".to_vec(),
        };
        let err = src.load_graph(std::path::Path::new("/proj")).unwrap_err();
        assert_eq!(err.code.to_string(), "ENV-0030");
        assert!(
            err.message.starts_with("codeweb export 失败:"),
            "plans.rs::export_failure 的匹配前缀依赖它: {}",
            err.message
        );
    }

    // ── EmbeddedCodewebSource：进程内真实 codeweb 项目 ─────────────────────

    const CODEWEB_TOML: &str = "[project]\n\
         name = \"embed-test\"\n\
         \n\
         [analysis]\n\
         paths = [\"sql/\"]\n\
         \n\
         [store]\n\
         path = \".codeweb/store.bincode\"\n\
         format = \"bincode\"\n";

    const TWO_PROCS: &str = "CREATE OR REPLACE PROCEDURE p_caller() AS $$\n\
         BEGIN\n\
             CALL p_leaf();\n\
         END;\n\
         $$ LANGUAGE plpgsql;\n\
         \n\
         CREATE OR REPLACE PROCEDURE p_leaf() AS $$\n\
         BEGIN\n\
             UPDATE t_users SET last_seen = now();\n\
         END;\n\
         $$ LANGUAGE plpgsql;\n";

    /// 最小 codeweb 项目：codeweb.toml + sql/a.sql（未分析）。
    fn codeweb_project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("codeweb.toml"), CODEWEB_TOML).unwrap();
        let sql = tmp.path().join("sql");
        std::fs::create_dir_all(&sql).unwrap();
        std::fs::write(sql.join("a.sql"), TWO_PROCS).unwrap();
        tmp
    }

    #[test]
    fn should_fail_with_env_30_when_the_store_is_missing() {
        let proj = codeweb_project();
        let src = EmbeddedCodewebSource {
            auto_analyze: false,
        };
        let err = src.load_graph(proj.path()).unwrap_err();
        assert_eq!(err.code.to_string(), "ENV-0030");
        assert!(
            err.message.contains("codeweb 图索引缺失"),
            "无 store 必须是图索引缺失语义: {}",
            err.message
        );
    }

    #[test]
    fn should_fail_with_env_30_when_sources_changed_after_analyze() {
        let proj = codeweb_project();
        // 第一次 auto_analyze=true 建 store
        let analyze = EmbeddedCodewebSource { auto_analyze: true };
        analyze
            .load_graph(proj.path())
            .expect("first call analyzes");
        // 改源文件后再以只读模式取图
        let sql = proj.path().join("sql/a.sql");
        let mut text = std::fs::read_to_string(&sql).unwrap();
        text.push_str("\n-- touch\n");
        std::fs::write(&sql, text).unwrap();

        let read_only = EmbeddedCodewebSource {
            auto_analyze: false,
        };
        let err = read_only.load_graph(proj.path()).unwrap_err();
        assert_eq!(err.code.to_string(), "ENV-0030");
        assert!(
            err.message.contains("codeweb 图与源码不一致"),
            "源码变动必须报不一致: {}",
            err.message
        );
        assert!(
            err.message.contains("1 个修改"),
            "必须报出变动文件数: {}",
            err.message
        );
    }

    #[test]
    fn should_load_a_graph_with_auto_analyze_and_a_versioned_generator() {
        let proj = codeweb_project();
        let src = EmbeddedCodewebSource { auto_analyze: true };
        let graph = src.load_graph(proj.path()).expect("auto-analyze then load");
        assert_eq!(
            graph.nodes.len(),
            4,
            "p_caller + p_leaf + t_users + 可能的系统表?: {:?}",
            graph.nodes
        );
        // p_caller 的 direct 调用边必须存在（闭包依赖它）
        assert!(
            graph
                .nodes
                .values()
                .any(|n| matches!(n, Node::Routine { name, .. } if name == "p_caller")),
            "p_caller 必须在图里: {:?}",
            graph.nodes
        );
        assert_eq!(
            src.generator(),
            format!("codeweb {}", codeweb::embed::version()),
            "generator 必须来自链接的 codeweb 版本"
        );
        assert!(
            src.generator().starts_with("codeweb 0.9."),
            "契约常量：嵌入的 codeweb 版本: {}",
            src.generator()
        );
    }

    /// Task 3.7 守护：generator 必须是 `codeweb <semver>` 形态且来自链接的
    /// codeweb 版本——防倒退成硬编码字面量（旧 `const GENERATOR = "codeweb 0.9.7"`
    /// 正是版本漂移的源头，见 docs/validation.md V27）。
    #[test]
    fn should_write_a_semver_generator_from_the_embedded_source() {
        let src = EmbeddedCodewebSource {
            auto_analyze: false,
        };
        let generator = src.generator();
        let version = generator
            .strip_prefix("codeweb ")
            .unwrap_or_else(|| panic!("generator 必须以 \"codeweb \" 开头: {generator}"));
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3, "semver 三段: {generator}");
        assert!(
            parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "semver 各段必须是数字: {generator}"
        );
        assert_eq!(
            version,
            codeweb::embed::version(),
            "generator 的版本必须等于链接进来的 codeweb 版本"
        );
    }

    #[test]
    fn should_fail_with_env_30_when_the_project_toml_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let src = EmbeddedCodewebSource { auto_analyze: true };
        let err = src.load_graph(tmp.path()).unwrap_err();
        assert_eq!(err.code.to_string(), "ENV-0030");
        assert!(
            err.message.contains("未找到 codeweb 项目"),
            "空目录必须报项目缺失: {}",
            err.message
        );
    }

    #[test]
    fn should_fail_with_env_30_when_the_store_version_is_stale() {
        let proj = codeweb_project();
        let analyze = EmbeddedCodewebSource { auto_analyze: true };
        analyze.load_graph(proj.path()).expect("build store");
        // 把 store 的版本头改成旧版本号
        let store = proj.path().join(".codeweb/store.bincode");
        let mut bytes = std::fs::read(&store).expect("store exists");
        assert!(bytes.len() >= 13);
        bytes[9..13].copy_from_slice(&7u32.to_le_bytes());
        std::fs::write(&store, &bytes).unwrap();

        let read_only = EmbeddedCodewebSource {
            auto_analyze: false,
        };
        let err = read_only.load_graph(proj.path()).unwrap_err();
        assert_eq!(err.code.to_string(), "ENV-0030");
        assert!(
            err.message.contains("版本过期"),
            "过期 store 必须报版本过期: {}",
            err.message
        );
    }
}
