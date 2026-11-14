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

#[cfg(test)]
mod tests {
    use super::*;
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
}
