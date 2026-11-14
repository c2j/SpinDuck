//! codeweb `export --format json` 的瘦反序列化，以及嵌入 ThinGraph 的等价映射。
//!
//! 不 `deny_unknown_fields`：`columns`、`column_analysis`、`sql`、`body` 留在
//! serde 的跳过路径里，不进入 [`CodewebGraph`]。`explicit` / `system` 缺省为 false。

use crate::error::EngineError;
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
