//! 瘦图上的一次 BFS。边只来自 codeweb，没有词法 `walk_chain`。

use super::parse::{CodewebGraph, EdgeKind, Node, OutEdge};
use super::{
    at_text, project_rel, ClosureContext, ClosureDynamic, ClosureMissing, ClosureRef, GraphClosure,
};
use crate::error::EngineError;
use ogsql_parser::ast::plpgsql::PlProcedureCall;
use ogsql_parser::ast::visitor::{walk_statement, Visitor, VisitorResult};
use ogsql_parser::ast::{CallFuncStatement, Expr, ObjectName};
use ogsql_parser::parser::Parser;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

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
    walk.complete_from_ast();
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
        self.drain(&mut queue);
    }

    fn drain(&mut self, queue: &mut VecDeque<usize>) {
        while let Some(id) = queue.pop_front() {
            if !self.seen.insert(id) {
                continue;
            }
            self.note_routine_file(id);
            let edges = self.graph.out.get(&id).cloned().unwrap_or_default();
            for edge in edges {
                self.apply(id, &edge, queue);
            }
        }
    }

    /// codeweb 提取器的已知盲区：IF/WHILE 条件、CASE、动态 SQL 字符串
    /// 等位置中的例程调用不产生 `calls_procedure` 边（extractor 的 IF
    /// 分支只走分支体，语料 `fnc_get_split_date`、`v_sql_txt` 内嵌的
    /// `fnc_com_get_seat_fee_type` 即此形态）。闭包定稿前以 AST 补全：
    /// 只扫描**已入闭包例程**的代码（同文件未入闭包成员不贡献种子，
    /// 避免无关例程的依赖污染入口闭包），命中图中例程节点而尚未入闭包
    /// 的例程补入并重走 BFS，直至不动点；文件读不出/解析失败的保持现状
    /// （与 codeweb 同样无法证明，交由执行期缺件门兜底，不静默扩大闭包）。
    fn complete_from_ast(&mut self) {
        loop {
            // 文件 → {成员名小写 → 节点 id}：只有这些例程的代码可产种子。
            let mut by_file: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
            for id in &self.seen {
                let Some(rel) = self.routine_rel(*id) else {
                    continue;
                };
                let Some(Node::Routine { name, .. }) = self.graph.nodes.get(id) else {
                    continue;
                };
                by_file
                    .entry(rel)
                    .or_default()
                    .insert(name.to_ascii_lowercase(), *id);
            }
            let mut seeds = BTreeSet::new();
            for (rel, wanted) in by_file {
                for name in routine_call_names(&self.ctx.project_root.join(&rel), &wanted) {
                    for id in self.matching_routines(&name) {
                        if !self.seen.contains(&id) {
                            seeds.insert(id);
                        }
                    }
                }
            }
            if seeds.is_empty() {
                return;
            }
            let mut queue = VecDeque::from_iter(seeds);
            self.drain(&mut queue);
        }
    }

    /// 调用名 → 图中例程节点：优先目录键逐字匹配；无点分限定名在
    /// 无精确命中时按尾段唯一匹配（包内成员的非限定互调）。多义不取。
    fn matching_routines(&self, name: &str) -> Vec<usize> {
        let exact: Vec<usize> = self
            .keys
            .iter()
            .filter(|(_, key)| key.as_str() == name)
            .map(|(id, _)| *id)
            .collect();
        if !exact.is_empty() || name.contains('.') {
            return exact;
        }
        let tail: Vec<usize> = self
            .keys
            .iter()
            .filter(|(_, key)| key.rsplit('.').next() == Some(name))
            .map(|(id, _)| *id)
            .collect();
        if tail.len() == 1 {
            tail
        } else {
            Vec::new()
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

/// 例程源文件中**指定例程**的调用名（点分小写，与 `catalog_keys` 同口径）。
/// `wanted` = 本闭包内该文件的例程成员名（小写）→ 节点 id：同文件未入
/// 闭包的例程（包的其他成员、独立例程）不贡献种子。调用名来自两类位置：
/// 1. AST 表达式/过程调用/CALL 语句（extractor 盲区补全）；
/// 2. 字符串字面量内的 `name(` 形态——动态 SQL（`OPEN c FOR v_sql_txt`、
///    `EXECUTE IMMEDIATE`）的调用对 AST 不可见，语料
///    `fnc_com_get_seat_fee_type` 即此形态。
///
/// 读/解析失败返回空集——补全是尽力而为的完备性修复，失败不扩大闭包。
fn routine_call_names(path: &Path, wanted: &BTreeMap<String, usize>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(sql) = std::fs::read_to_string(path) else {
        return names;
    };
    let (infos, errors) = Parser::parse_sql(&sql);
    if !errors.is_empty() {
        return names;
    }
    for info in &infos {
        use ogsql_parser::ast::Statement as St;
        match &info.statement {
            St::CreateProcedure(p) => {
                if routine_wanted(&object_tail(&p.name), wanted) {
                    let mut collector = CallNames(&mut names);
                    walk_statement(&mut collector, &info.statement);
                }
            }
            St::CreateFunction(f) => {
                if routine_wanted(&object_tail(&f.name), wanted) {
                    let mut collector = CallNames(&mut names);
                    walk_statement(&mut collector, &info.statement);
                }
            }
            St::CreatePackage(p) => scan_package(&p.items, wanted, &mut names),
            St::CreatePackageBody(p) => scan_package(&p.items, wanted, &mut names),
            _ => {}
        }
    }
    names
}

/// 例程名末段（小写）：`schema.name` / `pkg.member` 统一取尾段匹配成员名。
fn object_tail(name: &ObjectName) -> String {
    name.last()
        .map(|i| i.as_str().to_ascii_lowercase())
        .unwrap_or_default()
}

fn routine_wanted(tail: &str, wanted: &BTreeMap<String, usize>) -> bool {
    wanted.contains_key(tail)
}

/// 包项中属于闭包内例程的成员：只走其 `block`（PL 体），不把同包
/// 其他成员的语句计入本例程的调用集。
fn scan_package(
    items: &[ogsql_parser::ast::PackageItem],
    wanted: &BTreeMap<String, usize>,
    names: &mut BTreeSet<String>,
) {
    use ogsql_parser::ast::PackageItem;
    for item in items {
        let (name, block) = match item {
            PackageItem::Procedure(p) => (&p.name, p.block.as_ref()),
            PackageItem::Function(f) => (&f.name, f.block.as_ref()),
            _ => continue,
        };
        if !routine_wanted(&object_tail(name), wanted) {
            continue;
        }
        if let Some(block) = block {
            let mut collector = CallNames(names);
            ogsql_parser::ast::visitor::walk_pl_block(&mut collector, block);
        }
    }
}

/// SQL 文本中的 `name(` 调用候选（动态 SQL 字符串内的例程调用对
/// AST 不可见）：标识符（可含点分）后随可选空白与 `(`。
fn sql_text_call_names(text: &str, out: &mut BTreeSet<String>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            let mut j = i;
            while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'(' {
                let cand = text[start..i].trim_end_matches('.').to_ascii_lowercase();
                if !cand.is_empty() {
                    out.insert(cand);
                }
            }
        } else {
            i += 1;
        }
    }
}

fn object_name(name: &ObjectName) -> String {
    name.iter()
        .map(|i| i.as_str())
        .collect::<Vec<_>>()
        .join(".")
        .to_ascii_lowercase()
}

/// 收集表达式/过程调用/ CALL 语句中的例程名；内建函数同样入集，
/// 由调用方与图中例程键求交集自然过滤。
struct CallNames<'a>(&'a mut BTreeSet<String>);

impl Visitor for CallNames<'_> {
    fn visit_expr(&mut self, expr: &Expr) -> VisitorResult {
        match expr {
            Expr::FunctionCall { name, .. } => {
                self.0.insert(object_name(name));
            }
            // 动态 SQL 片段：字符串字面量内 `name(` 形态的例程调用
            // （OPEN FOR <变量> / EXECUTE IMMEDIATE 的文本来源）。
            Expr::Literal(ogsql_parser::ast::Literal::String(text))
            | Expr::Literal(ogsql_parser::ast::Literal::EscapeString(text))
            | Expr::Literal(ogsql_parser::ast::Literal::DollarString { body: text, .. }) => {
                sql_text_call_names(text, self.0);
            }
            _ => {}
        }
        VisitorResult::Continue
    }

    fn visit_procedure_call(&mut self, call: &PlProcedureCall) -> VisitorResult {
        self.0.insert(object_name(&call.name));
        VisitorResult::Continue
    }

    fn visit_call(&mut self, call: &CallFuncStatement) -> VisitorResult {
        self.0.insert(object_name(&call.func_name));
        VisitorResult::Continue
    }
}
