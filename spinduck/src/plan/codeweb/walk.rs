//! 瘦图上的一次 BFS。边只来自 codeweb，没有词法 `walk_chain`。

use super::parse::{CodewebGraph, EdgeKind, Node, OutEdge};
use super::{
    at_text, project_rel, ClosureContext, ClosureDynamic, ClosureMissing, ClosureRef, GraphClosure,
};
use crate::error::EngineError;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// 从一个已加载的瘦图和一个入口求闭包。
///
/// `entry` 只做与 `qualified_routine` 相同的小写（trim + ASCII lower），再与目录键
/// 逐字比较。目录键不走 `normalize`：包成员是包 `ObjectName` 的每一段加成员名。
pub fn closure_from_graph(
    graph: &CodewebGraph,
    entry: &str,
    ctx: &ClosureContext<'_>,
) -> Result<GraphClosure, EngineError> {
    let keys = catalog_keys(graph)?;
    let (entry_id, target) = match_entry(&keys, entry)?;
    ensure_entry_under_root(graph, entry_id, ctx)?;
    let mut walk = Walk {
        graph,
        ctx,
        keys: &keys,
        refs: BTreeSet::new(),
        dynamic: BTreeSet::new(),
        missing: BTreeSet::new(),
        ddl_routines: BTreeSet::new(),
        ddl_tables: BTreeSet::new(),
        seen: BTreeSet::new(),
    };
    walk.run(entry_id);
    Ok(GraphClosure {
        target,
        refs: walk.refs.into_iter().collect(),
        dynamic: walk.dynamic.into_iter().collect(),
        missing: walk.missing.into_iter().collect(),
        ddl_routines: walk.ddl_routines.into_iter().collect(),
        ddl_tables: walk.ddl_tables.into_iter().collect(),
    })
}

struct Walk<'a> {
    graph: &'a CodewebGraph,
    ctx: &'a ClosureContext<'a>,
    keys: &'a BTreeMap<usize, String>,
    refs: BTreeSet<ClosureRef>,
    dynamic: BTreeSet<ClosureDynamic>,
    missing: BTreeSet<ClosureMissing>,
    ddl_routines: BTreeSet<String>,
    ddl_tables: BTreeSet<String>,
    seen: BTreeSet<usize>,
}

impl Walk<'_> {
    fn run(&mut self, entry: usize) {
        let mut queue = VecDeque::new();
        queue.push_back(entry);
        while let Some(id) = queue.pop_front() {
            if !self.seen.insert(id) {
                continue;
            }
            self.note_routine_file(id);
            let edges = self.graph.out.get(&id).cloned().unwrap_or_default();
            for edge in edges {
                self.apply(id, &edge, &mut queue);
            }
        }
    }

    fn apply(&mut self, src: usize, edge: &OutEdge, queue: &mut VecDeque<usize>) {
        match &edge.kind {
            EdgeKind::Direct { file, line } => self.on_direct(src, edge.target, file, *line, queue),
            EdgeKind::Dynamic { raw, file, line } => {
                self.dynamic.insert(ClosureDynamic {
                    raw: raw.clone(),
                    from: self.label(src),
                    at: at_text(self.ctx.project_root, file, *line),
                });
            }
            EdgeKind::TableAccess {
                modes,
                write_kinds,
                file,
                line,
            } => {
                let roles = access_roles(modes, write_kinds);
                self.on_relation(src, edge.target, rel_edge(&roles, true, file, *line), queue);
            }
            EdgeKind::DependsOn { file, line } => {
                self.on_relation(
                    src,
                    edge.target,
                    rel_edge(&["read"], true, file, *line),
                    queue,
                );
            }
            EdgeKind::AnchorsOn { file, line } => {
                self.on_relation(
                    src,
                    edge.target,
                    rel_edge(&["type"], false, file, *line),
                    queue,
                );
            }
            EdgeKind::UsesSequence { file, line } => {
                self.on_sequence(src, edge.target, file, *line)
            }
        }
    }

    fn on_direct(
        &mut self,
        src: usize,
        target: usize,
        file: &str,
        line: u64,
        queue: &mut VecDeque<usize>,
    ) {
        let at = at_text(self.ctx.project_root, file, line);
        let from = self.label(src);
        match self.graph.nodes.get(&target) {
            Some(Node::Routine { .. }) => {
                let Some(key) = self.keys.get(&target).cloned() else {
                    return;
                };
                self.refs.insert(routine_ref(&key, &from, &at));
                if self.routine_rel(target).is_none() {
                    self.missing
                        .insert(miss(&format!("routine:{key}"), &from, &at));
                }
                queue.push_back(target);
            }
            Some(Node::Unresolved { raw_expr }) => {
                let name = crate::verify::normalize(raw_expr).to_ascii_lowercase();
                self.missing
                    .insert(miss(&format!("routine:{name}"), &from, &at));
            }
            Some(Node::Builtin) | None => {}
            // direct 只记调用。表、视图、序列走各自的边类型，不在这里当缺件。
            Some(
                Node::Package { .. }
                | Node::Table { .. }
                | Node::View { .. }
                | Node::MView { .. }
                | Node::Sequence { .. },
            ) => {}
        }
    }

    fn on_relation(
        &mut self,
        src: usize,
        target: usize,
        edge: RelEdge<'_>,
        queue: &mut VecDeque<usize>,
    ) {
        let Some(rel) = relation(self.graph, target) else {
            return;
        };
        let at = at_text(self.ctx.project_root, edge.file, edge.line);
        let from = self.label(src);
        for role in edge.roles {
            self.refs.insert(ClosureRef {
                object: rel.name.clone(),
                kind: rel.kind.to_string(),
                role: (*role).to_string(),
                from: from.clone(),
                at: at.clone(),
            });
        }
        self.note_ddl("relation", &rel.name, rel.system, &from, &at);
        if edge.enqueue_view && rel.walk {
            queue.push_back(target);
        }
    }

    fn on_sequence(&mut self, src: usize, target: usize, file: &str, line: u64) {
        let Some(Node::Sequence { schema, name }) = self.graph.nodes.get(&target) else {
            return;
        };
        let object = qualified(schema, name);
        let at = at_text(self.ctx.project_root, file, line);
        let from = self.label(src);
        self.refs.insert(ClosureRef {
            object: object.clone(),
            kind: "sequence".to_string(),
            role: "read".to_string(),
            from: from.clone(),
            at: at.clone(),
        });
        self.note_ddl("sequence", &object, false, &from, &at);
    }

    fn note_ddl(&mut self, prefix: &str, name: &str, system: bool, from: &str, at: &str) {
        if system || (prefix == "relation" && engine_provided(name)) {
            return;
        }
        if let Some(file) = lookup_ddl(self.ctx.ddl_files, name) {
            self.ddl_tables.insert(file);
            return;
        }
        self.missing
            .insert(miss(&format!("{prefix}:{name}"), from, at));
    }

    fn note_routine_file(&mut self, id: usize) {
        if let Some(rel) = self.routine_rel(id) {
            self.ddl_routines.insert(rel);
        }
    }

    fn routine_rel(&self, id: usize) -> Option<String> {
        match self.graph.nodes.get(&id) {
            Some(Node::Routine { file, .. }) => project_rel(self.ctx.project_root, file),
            Some(
                Node::Package { .. }
                | Node::Table { .. }
                | Node::View { .. }
                | Node::MView { .. }
                | Node::Sequence { .. }
                | Node::Unresolved { .. }
                | Node::Builtin,
            )
            | None => None,
        }
    }

    fn label(&self, id: usize) -> String {
        if let Some(key) = self.keys.get(&id) {
            return key.clone();
        }
        match self.graph.nodes.get(&id) {
            Some(Node::Table { schema, name, .. })
            | Some(Node::View { schema, name, .. })
            | Some(Node::MView { schema, name })
            | Some(Node::Sequence { schema, name }) => qualified(schema, name),
            Some(
                Node::Routine { .. }
                | Node::Package { .. }
                | Node::Unresolved { .. }
                | Node::Builtin,
            )
            | None => String::new(),
        }
    }
}

struct RelEdge<'a> {
    roles: &'a [&'a str],
    enqueue_view: bool,
    file: &'a str,
    line: u64,
}

fn rel_edge<'a>(roles: &'a [&'a str], enqueue_view: bool, file: &'a str, line: u64) -> RelEdge<'a> {
    RelEdge {
        roles,
        enqueue_view,
        file,
        line,
    }
}

struct RelInfo {
    kind: &'static str,
    name: String,
    system: bool,
    walk: bool,
}

fn relation(graph: &CodewebGraph, id: usize) -> Option<RelInfo> {
    Some(match graph.nodes.get(&id)? {
        Node::Table {
            schema,
            name,
            system,
        } => RelInfo {
            kind: "table",
            name: qualified(schema, name),
            system: *system,
            walk: false,
        },
        Node::View {
            schema,
            name,
            system,
        } => RelInfo {
            kind: "view",
            name: qualified(schema, name),
            system: *system,
            walk: true,
        },
        Node::MView { schema, name } => RelInfo {
            kind: "view",
            name: qualified(schema, name),
            system: false,
            walk: true,
        },
        Node::Routine { .. }
        | Node::Package { .. }
        | Node::Sequence { .. }
        | Node::Unresolved { .. }
        | Node::Builtin => return None,
    })
}

fn routine_ref(object: &str, from: &str, at: &str) -> ClosureRef {
    ClosureRef {
        object: object.to_string(),
        kind: "routine".to_string(),
        role: "call".to_string(),
        from: from.to_string(),
        at: at.to_string(),
    }
}

fn miss(object: &str, from: &str, at: &str) -> ClosureMissing {
    ClosureMissing {
        object: object.to_string(),
        from: from.to_string(),
        at: at.to_string(),
    }
}

/// `write` 只在 modes 含 write 或 write_kinds 非空时出现。其余模式（含锁）是 read。
/// 同一条边两边都成立时读、写各一行。
fn access_roles(modes: &[String], write_kinds: &[String]) -> Vec<&'static str> {
    let is_write = !write_kinds.is_empty() || modes.iter().any(|m| m.eq_ignore_ascii_case("write"));
    let read_or_lock = modes.iter().any(|m| !m.eq_ignore_ascii_case("write"));
    let mut roles = Vec::new();
    if read_or_lock || !is_write {
        roles.push("read");
    }
    if is_write {
        roles.push("write");
    }
    roles
}

fn qualified(schema: &Option<String>, name: &str) -> String {
    let raw = match schema {
        Some(s) if !s.is_empty() => format!("{s}.{name}"),
        _ => name.to_string(),
    };
    crate::verify::normalize(&raw).to_ascii_lowercase()
}

fn engine_provided(name: &str) -> bool {
    let tail = match name.rsplit('.').next() {
        Some(t) => t,
        None => name,
    };
    crate::verify::ENGINE_PROVIDED_TABLES
        .iter()
        .any(|t| t.eq_ignore_ascii_case(tail))
}

fn lookup_ddl(ddl: &BTreeMap<String, String>, name: &str) -> Option<String> {
    if let Some(file) = ddl.get(name) {
        return Some(file.clone());
    }
    let tail = name.rsplit('.').next()?;
    if tail == name {
        return None;
    }
    ddl.get(tail).cloned()
}

fn catalog_keys(graph: &CodewebGraph) -> Result<BTreeMap<usize, String>, EngineError> {
    let packages = package_endpoints(graph)?;
    let mut keys = BTreeMap::new();
    for (id, node) in &graph.nodes {
        let Node::Routine { name, schema, .. } = node else {
            continue;
        };
        let key = match packages.get(id) {
            Some(pkg) => member_key(&pkg.schema, &pkg.name, name),
            None => member_key(schema, name, ""),
        };
        keys.insert(*id, key);
    }
    Ok(keys)
}

struct Pkg {
    schema: Option<String>,
    name: String,
}

/// 无包时 `member` 传空串，`name` 是例程自己的名字（可含 `.`）。
fn member_key(schema: &Option<String>, name: &str, member: &str) -> String {
    let key = match (schema, member) {
        (Some(s), m) if !s.is_empty() && !m.is_empty() => format!("{s}.{name}.{m}"),
        (_, m) if !m.is_empty() => format!("{name}.{m}"),
        (Some(s), _) if !s.is_empty() => format!("{s}.{name}"),
        _ => name.to_string(),
    };
    key.to_ascii_lowercase()
}

fn package_endpoints(graph: &CodewebGraph) -> Result<BTreeMap<usize, Pkg>, EngineError> {
    let mut idents: BTreeMap<usize, BTreeMap<(String, String), Pkg>> = BTreeMap::new();
    for &(src, dst) in &graph.contains {
        let Some(Node::Package { schema, name }) = graph.nodes.get(&src) else {
            continue;
        };
        if !matches!(graph.nodes.get(&dst), Some(Node::Routine { .. })) {
            continue;
        }
        let ident = (
            schema.as_deref().unwrap_or("").to_ascii_lowercase(),
            name.to_ascii_lowercase(),
        );
        idents.entry(dst).or_default().entry(ident).or_insert(Pkg {
            schema: schema.clone(),
            name: name.clone(),
        });
    }
    let mut out = BTreeMap::new();
    for (routine, set) in idents {
        if set.len() > 1 {
            let member = routine_name(graph, routine);
            return Err(EngineError::env(
                30,
                format!(
                    "codeweb 图不自洽: 例程 {member} 连着 {} 个 package 端点",
                    set.len()
                ),
            ));
        }
        if let Some((_, pkg)) = set.into_iter().next() {
            out.insert(routine, pkg);
        }
    }
    Ok(out)
}

fn routine_name(graph: &CodewebGraph, id: usize) -> String {
    match graph.nodes.get(&id) {
        Some(Node::Routine { name, .. }) => name.clone(),
        Some(
            Node::Package { .. }
            | Node::Table { .. }
            | Node::View { .. }
            | Node::MView { .. }
            | Node::Sequence { .. }
            | Node::Unresolved { .. }
            | Node::Builtin,
        )
        | None => id.to_string(),
    }
}

fn match_entry(
    keys: &BTreeMap<usize, String>,
    entry: &str,
) -> Result<(usize, String), EngineError> {
    let want = entry.trim().to_ascii_lowercase();
    let mut hits: Vec<usize> = keys
        .iter()
        .filter(|(_, key)| *key == &want)
        .map(|(id, _)| *id)
        .collect();
    hits.sort_unstable();
    match hits.as_slice() {
        [] => Err(EngineError::business(
            150,
            format!("计划目标例程 '{want}' 未在 codeweb 图中登记"),
        )),
        [id] => match keys.get(id) {
            Some(key) => Ok((*id, key.clone())),
            None => Err(EngineError::invariant(30, "入口命中的节点没有目录键")),
        },
        _ => Err(EngineError::env(
            30,
            format!("入口在 codeweb 图中不唯一: {want}"),
        )),
    }
}

fn ensure_entry_under_root(
    graph: &CodewebGraph,
    id: usize,
    ctx: &ClosureContext<'_>,
) -> Result<(), EngineError> {
    let file = match graph.nodes.get(&id) {
        Some(Node::Routine { file, .. }) => file.as_str(),
        _ => "",
    };
    if project_rel(ctx.project_root, file).is_none() {
        return Err(EngineError::env(
            30,
            format!("入口源文件不在项目根下: {file}"),
        ));
    }
    Ok(())
}
