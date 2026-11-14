//! codeweb 0.9.7 `export --format json` 的瘦反序列化。
//!
//! 不 `deny_unknown_fields`：`columns`、`column_analysis`、`sql`、`body` 留在
//! serde 的跳过路径里，不进入 [`CodewebGraph`]。`explicit` / `system` 缺省为 false。

use crate::error::EngineError;
use codeweb::embed::{AccessModeTag, ThinEdgeKind, ThinGraph, ThinNodeKind};
use serde::Deserialize;
use std::collections::BTreeMap;

/// 瘦图。节点 id 用 JSON 的 `id`，不是数组下标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodewebGraph {
    pub(super) nodes: BTreeMap<usize, Node>,
    pub(super) out: BTreeMap<usize, Vec<OutEdge>>,
    /// `contains_routine`：源是 package，目标是 procedure/function。只供目录键。
    pub(super) contains: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Node {
    Routine {
        name: String,
        schema: Option<String>,
        file: String,
    },
    Package {
        schema: Option<String>,
        name: String,
    },
    Table {
        schema: Option<String>,
        name: String,
        system: bool,
    },
    View {
        schema: Option<String>,
        name: String,
        system: bool,
    },
    /// 没有 `explicit` / `system`；缺件按两者都是 false。
    MView {
        schema: Option<String>,
        name: String,
    },
    Sequence {
        schema: Option<String>,
        name: String,
    },
    Unresolved {
        raw_expr: String,
    },
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OutEdge {
    pub(super) target: usize,
    pub(super) kind: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EdgeKind {
    Direct {
        file: String,
        line: u64,
    },
    Dynamic {
        raw: String,
        file: String,
        line: u64,
    },
    TableAccess {
        modes: Vec<String>,
        write_kinds: Vec<String>,
        file: String,
        line: u64,
    },
    DependsOn {
        file: String,
        line: u64,
    },
    UsesSequence {
        file: String,
        line: u64,
    },
    AnchorsOn {
        file: String,
        line: u64,
    },
}

/// 把一份导出 JSON 收成瘦图。Java/JSP/mapper/index/trigger/synonym/event/custom
/// 以及碰到它们的边直接丢掉。
pub fn parse_codeweb_json(text: &str) -> Result<CodewebGraph, EngineError> {
    parse_codeweb_reader(std::io::Cursor::new(text.as_bytes()))
}

/// 从导出管道解析瘦图。生产路径不要先把整份文档读成 `Vec<u8>`。
pub fn parse_codeweb_reader(reader: impl std::io::Read) -> Result<CodewebGraph, EngineError> {
    let raw: RawGraph = serde_json::from_reader(std::io::BufReader::new(reader))
        .map_err(|e| EngineError::env(30, format!("codeweb JSON 解析失败: {e}")))?;
    Ok(graph_from_raw(raw))
}

fn graph_from_raw(raw: RawGraph) -> CodewebGraph {
    let mut nodes = BTreeMap::new();
    for node in raw.nodes {
        if let Some(kind) = node.kind.into_node() {
            nodes.entry(node.id).or_insert(kind);
        }
    }
    let mut out: BTreeMap<usize, Vec<OutEdge>> = BTreeMap::new();
    let mut contains = Vec::new();
    for edge in raw.edges {
        match edge.kind {
            RawEdgeKind::ContainsRoutine => {
                if nodes.contains_key(&edge.source) && nodes.contains_key(&edge.target) {
                    contains.push((edge.source, edge.target));
                }
            }
            RawEdgeKind::Other => {}
            other => {
                if !nodes.contains_key(&edge.source) {
                    continue;
                }
                if let Some(kind) = other.into_walk() {
                    if nodes.contains_key(&edge.target) || matches!(kind, EdgeKind::Dynamic { .. })
                    {
                        out.entry(edge.source).or_default().push(OutEdge {
                            target: edge.target,
                            kind,
                        });
                    }
                }
            }
        }
    }
    CodewebGraph {
        nodes,
        out,
        contains,
    }
}

/// 嵌入路径取图入口：`codeweb::embed::ThinGraph` → 内部瘦图。
///
/// 与 [`parse_codeweb_json`]（JSON 路径）产出同一个模型，过滤语义与
/// `graph_from_raw` 逐条对齐：
/// - `contains_routine` 只在两端都存在时收录；
/// - 其它边要求 source 存在，且（target 存在 或 边是 `Dynamic`）——dynamic 边的
///   target 是发出调用的例程自身，与 JSON 口径一致地豁免 target 检查；
/// - `explicit` 字段丢弃：`walk.rs` 只消费 `system`（过滤 system 与引擎自建
///   对象），`explicit` 在嵌入契约里保留只为 codeweb 侧语义完整；
/// - routine 的 `package` 丢弃：包成员关系由 `contains_routine` 边还原
///   （`walk.rs::catalog_keys` 的既有口径），与 JSON 的 procedure/function
///   节点不含 package 字段保持一致。
pub(super) fn from_thin_graph(thin: ThinGraph) -> CodewebGraph {
    let mut nodes = BTreeMap::new();
    for entry in thin.nodes {
        if let Some(kind) = thin_node(entry.kind) {
            nodes.entry(entry.id).or_insert(kind);
        }
    }
    let mut out: BTreeMap<usize, Vec<OutEdge>> = BTreeMap::new();
    let mut contains = Vec::new();
    for edge in thin.edges {
        if let ThinEdgeKind::ContainsRoutine = edge.kind {
            if nodes.contains_key(&edge.source) && nodes.contains_key(&edge.target) {
                contains.push((edge.source, edge.target));
            }
            continue;
        }
        if !nodes.contains_key(&edge.source) {
            continue;
        }
        if let Some(kind) = thin_edge(edge.kind) {
            if nodes.contains_key(&edge.target) || matches!(kind, EdgeKind::Dynamic { .. }) {
                out.entry(edge.source).or_default().push(OutEdge {
                    target: edge.target,
                    kind,
                });
            }
        }
    }
    CodewebGraph {
        nodes,
        out,
        contains,
    }
}

fn thin_node(kind: ThinNodeKind) -> Option<Node> {
    Some(match kind {
        ThinNodeKind::Procedure {
            schema,
            package: _,
            name,
            file,
            ..
        }
        | ThinNodeKind::Function {
            schema,
            package: _,
            name,
            file,
            ..
        } => Node::Routine {
            name,
            schema,
            file: file.to_string_lossy().into_owned(),
        },
        ThinNodeKind::Package { schema, name } => Node::Package { schema, name },
        // explicit 丢弃（见 from_thin_graph 文档），system 直传。
        ThinNodeKind::Table {
            schema,
            name,
            explicit: _,
            system,
        } => Node::Table {
            schema,
            name,
            system,
        },
        ThinNodeKind::View {
            schema,
            name,
            explicit: _,
            system,
        } => Node::View {
            schema,
            name,
            system,
        },
        ThinNodeKind::MaterializedView { schema, name } => Node::MView { schema, name },
        // explicit 丢弃：Sequence 的缺件判定与 JSON 路径一致（只看名字）。
        ThinNodeKind::Sequence {
            schema,
            name,
            explicit: _,
        } => Node::Sequence { schema, name },
        ThinNodeKind::Unresolved { raw_expr } => Node::Unresolved { raw_expr },
        ThinNodeKind::BuiltinFunction { name: _ } => Node::Builtin,
    })
}

fn thin_edge(kind: ThinEdgeKind) -> Option<EdgeKind> {
    let text = |file: std::path::PathBuf| file.to_string_lossy().into_owned();
    Some(match kind {
        ThinEdgeKind::Direct { file, line } => EdgeKind::Direct {
            file: text(file),
            line: line as u64,
        },
        ThinEdgeKind::Dynamic {
            raw_expr,
            file,
            line,
        } => EdgeKind::Dynamic {
            raw: raw_expr,
            file: text(file),
            line: line as u64,
        },
        ThinEdgeKind::TableAccess {
            modes,
            write_kinds,
            file,
            line,
        } => EdgeKind::TableAccess {
            modes: modes
                .into_iter()
                .map(access_mode_text)
                .map(String::from)
                .collect(),
            write_kinds,
            file: text(file),
            line: line as u64,
        },
        ThinEdgeKind::DependsOn { file, line } => EdgeKind::DependsOn {
            file: text(file),
            line: line as u64,
        },
        ThinEdgeKind::UsesSequence { file, line } => EdgeKind::UsesSequence {
            file: text(file),
            line: line as u64,
        },
        ThinEdgeKind::AnchorsOn { file, line } => EdgeKind::AnchorsOn {
            file: text(file),
            line: line as u64,
        },
        ThinEdgeKind::ContainsRoutine => return None,
    })
}

/// `AccessModeTag` → JSON 线上的小写文本（`walk.rs::access_roles` 吃这一口径）。
fn access_mode_text(tag: AccessModeTag) -> &'static str {
    match tag {
        AccessModeTag::Read => "read",
        AccessModeTag::Write => "write",
        AccessModeTag::LockRead => "lock_read",
        AccessModeTag::AccessExclusive => "access_exclusive",
        AccessModeTag::ShareUpdateExclusive => "share_update_exclusive",
        AccessModeTag::Share => "share",
        AccessModeTag::ShareRowExclusive => "share_row_exclusive",
        AccessModeTag::Exclusive => "exclusive",
    }
}

#[derive(Debug, Deserialize)]
struct RawGraph {
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default)]
    edges: Vec<RawEdge>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    id: usize,
    #[serde(flatten)]
    kind: RawNodeKind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawNodeKind {
    Procedure {
        name: String,
        #[serde(default)]
        schema: Option<String>,
        #[serde(default)]
        file: String,
    },
    Function {
        name: String,
        #[serde(default)]
        schema: Option<String>,
        #[serde(default)]
        file: String,
    },
    Package {
        #[serde(default)]
        schema: Option<String>,
        name: String,
    },
    Table {
        #[serde(default)]
        schema: Option<String>,
        name: String,
        #[serde(default)]
        system: bool,
    },
    View {
        #[serde(default)]
        schema: Option<String>,
        name: String,
        #[serde(default)]
        system: bool,
    },
    MaterializedView {
        #[serde(default)]
        schema: Option<String>,
        name: String,
    },
    Sequence {
        #[serde(default)]
        schema: Option<String>,
        name: String,
    },
    Unresolved {
        raw_expr: String,
    },
    BuiltinFunction,
    #[serde(other)]
    Other,
}

impl RawNodeKind {
    fn into_node(self) -> Option<Node> {
        Some(match self {
            Self::Procedure { name, schema, file } | Self::Function { name, schema, file } => {
                Node::Routine { name, schema, file }
            }
            Self::Package { schema, name } => Node::Package { schema, name },
            Self::Table {
                schema,
                name,
                system,
            } => Node::Table {
                schema,
                name,
                system,
            },
            Self::View {
                schema,
                name,
                system,
            } => Node::View {
                schema,
                name,
                system,
            },
            Self::MaterializedView { schema, name } => Node::MView { schema, name },
            Self::Sequence { schema, name } => Node::Sequence { schema, name },
            Self::Unresolved { raw_expr } => Node::Unresolved { raw_expr },
            Self::BuiltinFunction => Node::Builtin,
            Self::Other => return None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawEdge {
    source: usize,
    target: usize,
    #[serde(flatten)]
    kind: RawEdgeKind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawEdgeKind {
    #[serde(rename = "direct")]
    Direct {
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: u64,
    },
    #[serde(rename = "dynamic")]
    Dynamic {
        raw_expr: String,
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: u64,
    },
    #[serde(rename = "uses_builtin_function")]
    UsesBuiltin,
    #[serde(rename = "table_access")]
    TableAccess {
        #[serde(default)]
        modes: Vec<String>,
        #[serde(default)]
        write_kinds: Vec<String>,
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: u64,
    },
    #[serde(rename = "depends_on")]
    DependsOn {
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: u64,
    },
    #[serde(rename = "contains_routine")]
    ContainsRoutine,
    #[serde(rename = "uses_sequence")]
    UsesSequence {
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: u64,
    },
    #[serde(rename = "anchors_on")]
    AnchorsOn {
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: u64,
    },
    #[serde(other)]
    Other,
}

impl RawEdgeKind {
    fn into_walk(self) -> Option<EdgeKind> {
        Some(match self {
            Self::Direct { file, line } => EdgeKind::Direct { file, line },
            Self::Dynamic {
                raw_expr,
                file,
                line,
            } => EdgeKind::Dynamic {
                raw: raw_expr,
                file,
                line,
            },
            Self::TableAccess {
                modes,
                write_kinds,
                file,
                line,
            } => EdgeKind::TableAccess {
                modes,
                write_kinds,
                file,
                line,
            },
            Self::DependsOn { file, line } => EdgeKind::DependsOn { file, line },
            Self::UsesSequence { file, line } => EdgeKind::UsesSequence { file, line },
            Self::AnchorsOn { file, line } => EdgeKind::AnchorsOn { file, line },
            Self::UsesBuiltin | Self::ContainsRoutine | Self::Other => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeweb::embed::{
        AccessModeTag, ThinEdge, ThinEdgeKind, ThinGraph, ThinNodeEntry, ThinNodeKind,
    };
    use std::path::PathBuf;

    /// 全变体等价夹具：同一张逻辑图的两份表示。
    /// JSON 走既有 `parse_codeweb_json`，ThinGraph 值手工构造；`from_thin_graph`
    /// 必须产出**逐字段相等**的 `CodewebGraph`。
    ///
    /// 覆盖节点：procedure(+package 经 contains 边)、function、package、table
    /// (+explicit/system)、view(+explicit/system)、materialized_view、sequence
    /// (+explicit)、unresolved、builtin。
    /// 覆盖边：direct、dynamic、table_access(8 种 modes + write_kinds)、depends_on、
    /// uses_sequence、anchors_on、contains_routine。
    fn json_fixture() -> String {
        r#"{
            "nodes": [
                {"id": 0, "type": "procedure", "name": "prc_main", "schema": "pkg_a", "file": "/proj/sql/pkg_a.sql", "line": 1, "partial": true},
                {"id": 1, "type": "function", "name": "fnc_x", "file": "/proj/sql/f.sql", "line": 2},
                {"id": 2, "type": "package", "name": "pkg_a", "schema": "pkg_a"},
                {"id": 3, "type": "table", "name": "t1", "explicit": true, "system": false},
                {"id": 4, "type": "view", "name": "v1", "explicit": true, "system": false},
                {"id": 5, "type": "materialized_view", "name": "mv1"},
                {"id": 6, "type": "sequence", "name": "seq1", "explicit": false},
                {"id": 7, "type": "unresolved", "raw_expr": "dyn_proc()", "context": "ctx"},
                {"id": 8, "type": "builtin_function", "name": "coalesce", "category": "x", "domain": "y", "file": "/proj/sql/pkg_a.sql", "line": 3},
                {"id": 9, "type": "java_sql", "class_name": null, "method_name": null, "extraction_method": "jdbc", "file": "/proj/X.java", "line": 4, "sql": null}
            ],
            "edges": [
                {"source": 0, "target": 1, "type": "direct", "scope": "sql", "file": "/proj/sql/pkg_a.sql", "line": 5},
                {"source": 0, "target": 7, "type": "dynamic", "raw_expr": "PlVariable([\"v_sql\"])", "file": "/proj/sql/pkg_a.sql", "line": 6},
                {"source": 1, "target": 3, "type": "table_access", "flow_kind": "dml", "modes": ["read", "write", "lock_read", "access_exclusive", "share_update_exclusive", "share", "share_row_exclusive", "exclusive"], "write_kinds": ["insert"], "file": "/proj/sql/f.sql", "line": 7},
                {"source": 1, "target": 4, "type": "depends_on", "file": "/proj/sql/f.sql", "line": 8},
                {"source": 1, "target": 6, "type": "uses_sequence", "file": "/proj/sql/f.sql", "line": 9},
                {"source": 1, "target": 5, "type": "anchors_on", "file": "/proj/sql/f.sql", "line": 10, "kind": "percent_rowtype", "site": "return_type"},
                {"source": 2, "target": 0, "type": "contains_routine"},
                {"source": 0, "target": 8, "type": "uses_builtin_function", "file": "/proj/sql/pkg_a.sql", "line": 11},
                {"source": 9, "target": 3, "type": "table_access", "modes": ["read"], "write_kinds": [], "file": "/proj/X.java", "line": 12}
            ]
        }"#
        .to_string()
    }

    fn thin_fixture() -> ThinGraph {
        let f = |p: &str| PathBuf::from(p);
        let opt = |s: Option<&str>| s.map(str::to_string);
        ThinGraph {
            nodes: vec![
                ThinNodeEntry {
                    id: 0,
                    kind: ThinNodeKind::Procedure {
                        schema: opt(Some("pkg_a")),
                        package: opt(Some("pkg_a")),
                        name: "prc_main".to_string(),
                        file: f("/proj/sql/pkg_a.sql"),
                        line: 1,
                        partial: true,
                    },
                },
                ThinNodeEntry {
                    id: 1,
                    kind: ThinNodeKind::Function {
                        schema: None,
                        package: None,
                        name: "fnc_x".to_string(),
                        file: f("/proj/sql/f.sql"),
                        line: 2,
                        partial: false,
                    },
                },
                ThinNodeEntry {
                    id: 2,
                    kind: ThinNodeKind::Package {
                        schema: opt(Some("pkg_a")),
                        name: "pkg_a".to_string(),
                    },
                },
                ThinNodeEntry {
                    id: 3,
                    kind: ThinNodeKind::Table {
                        schema: None,
                        name: "t1".to_string(),
                        explicit: true,
                        system: false,
                    },
                },
                ThinNodeEntry {
                    id: 4,
                    kind: ThinNodeKind::View {
                        schema: None,
                        name: "v1".to_string(),
                        explicit: true,
                        system: false,
                    },
                },
                ThinNodeEntry {
                    id: 5,
                    kind: ThinNodeKind::MaterializedView {
                        schema: None,
                        name: "mv1".to_string(),
                    },
                },
                ThinNodeEntry {
                    id: 6,
                    kind: ThinNodeKind::Sequence {
                        schema: None,
                        name: "seq1".to_string(),
                        explicit: false,
                    },
                },
                ThinNodeEntry {
                    id: 7,
                    kind: ThinNodeKind::Unresolved {
                        raw_expr: "dyn_proc()".to_string(),
                    },
                },
                ThinNodeEntry {
                    id: 8,
                    kind: ThinNodeKind::BuiltinFunction {
                        name: "coalesce".to_string(),
                    },
                },
                // 9 = JavaSql：嵌入投影已丢弃；这里也不放节点（等价面只含保留类）
            ],
            edges: vec![
                ThinEdge {
                    source: 0,
                    target: 1,
                    kind: ThinEdgeKind::Direct {
                        file: f("/proj/sql/pkg_a.sql"),
                        line: 5,
                    },
                },
                ThinEdge {
                    source: 0,
                    target: 7,
                    kind: ThinEdgeKind::Dynamic {
                        raw_expr: "PlVariable([\"v_sql\"])".to_string(),
                        file: f("/proj/sql/pkg_a.sql"),
                        line: 6,
                    },
                },
                ThinEdge {
                    source: 1,
                    target: 3,
                    kind: ThinEdgeKind::TableAccess {
                        modes: vec![
                            AccessModeTag::Read,
                            AccessModeTag::Write,
                            AccessModeTag::LockRead,
                            AccessModeTag::AccessExclusive,
                            AccessModeTag::ShareUpdateExclusive,
                            AccessModeTag::Share,
                            AccessModeTag::ShareRowExclusive,
                            AccessModeTag::Exclusive,
                        ],
                        write_kinds: vec!["insert".to_string()],
                        file: f("/proj/sql/f.sql"),
                        line: 7,
                    },
                },
                ThinEdge {
                    source: 1,
                    target: 4,
                    kind: ThinEdgeKind::DependsOn {
                        file: f("/proj/sql/f.sql"),
                        line: 8,
                    },
                },
                ThinEdge {
                    source: 1,
                    target: 6,
                    kind: ThinEdgeKind::UsesSequence {
                        file: f("/proj/sql/f.sql"),
                        line: 9,
                    },
                },
                ThinEdge {
                    source: 1,
                    target: 5,
                    kind: ThinEdgeKind::AnchorsOn {
                        file: f("/proj/sql/f.sql"),
                        line: 10,
                    },
                },
                ThinEdge {
                    source: 2,
                    target: 0,
                    kind: ThinEdgeKind::ContainsRoutine,
                },
            ],
        }
    }

    #[test]
    fn should_map_thin_graph_to_the_same_thin_graph_as_the_equivalent_json() {
        let from_json = parse_codeweb_json(&json_fixture()).expect("json fixture parses");
        let from_thin = from_thin_graph(thin_fixture());
        assert_eq!(
            from_json, from_thin,
            "ThinGraph 与 JSON 两条路径必须产出逐字段相等的瘦图"
        );
    }

    #[test]
    fn should_drop_a_direct_edge_whose_target_is_not_a_kept_node() {
        let mut thin = thin_fixture();
        thin.edges.push(ThinEdge {
            source: 0,
            target: 99, // 投影后不存在的 id
            kind: ThinEdgeKind::Direct {
                file: PathBuf::from("/proj/sql/pkg_a.sql"),
                line: 42,
            },
        });
        let graph = from_thin_graph(thin);
        assert!(
            !graph.out.values().flatten().any(|e| e.target == 99),
            "指向未知节点的边必须丢弃"
        );
    }

    #[test]
    fn should_keep_a_dynamic_edge_even_when_its_target_is_not_a_kept_node() {
        // JSON 口径：dynamic 边的 target 是发出调用的例程自身，target 检查豁免
        // （parse.rs::graph_from_raw 对 EdgeKind::Dynamic 的既有语义）。
        let thin = ThinGraph {
            nodes: vec![ThinNodeEntry {
                id: 0,
                kind: ThinNodeKind::Procedure {
                    schema: None,
                    package: None,
                    name: "p".to_string(),
                    file: PathBuf::from("/proj/sql/p.sql"),
                    line: 1,
                    partial: false,
                },
            }],
            edges: vec![ThinEdge {
                source: 0,
                target: 7,
                kind: ThinEdgeKind::Dynamic {
                    raw_expr: "PlVariable([\"v_sql\"])".to_string(),
                    file: PathBuf::from("/proj/sql/p.sql"),
                    line: 2,
                },
            }],
        };
        let graph = from_thin_graph(thin);
        assert!(
            graph
                .out
                .get(&0)
                .is_some_and(|edges| edges.iter().any(|e| e.target == 7)),
            "dynamic 边不因 target 缺失而丢弃"
        );
    }

    #[test]
    fn should_map_access_mode_tags_to_the_json_wire_spelling() {
        // walk.rs::access_roles 吃 JSON 口径的小写文本数组。
        assert_eq!(access_mode_text(AccessModeTag::Read), "read");
        assert_eq!(access_mode_text(AccessModeTag::Write), "write");
        assert_eq!(access_mode_text(AccessModeTag::LockRead), "lock_read");
        assert_eq!(
            access_mode_text(AccessModeTag::AccessExclusive),
            "access_exclusive"
        );
        assert_eq!(
            access_mode_text(AccessModeTag::ShareUpdateExclusive),
            "share_update_exclusive"
        );
        assert_eq!(access_mode_text(AccessModeTag::Share), "share");
        assert_eq!(
            access_mode_text(AccessModeTag::ShareRowExclusive),
            "share_row_exclusive"
        );
        assert_eq!(access_mode_text(AccessModeTag::Exclusive), "exclusive");
    }
}
