//! 闭包求值（design §6.1 / R3）：目标例程 → 源文件清单 + 链路对象清单 + 缺件表。
//!
//! 全程只做编译（`NoopExecutor`），不下内核：闭包描述的是**静态**引用关系，
//! "内核里此刻有没有这张表"是 `deploy`/`verify` 下一步要证明的事，不在这里猜。
//! 抽取口径一律复用链路核验的纯函数（`crate::verify::chain`），避免计划与核验两套真相。

use crate::catalog::CompiledRoutine;
use crate::cli::{read_source_strict, NoopExecutor};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::{MissingObject, RunPlan, SCHEMA_VERSION};
use crate::project::ProjectConfig;
use crate::rules::RuleRegistry;
use crate::verify::{
    chain_object_refs, expr_func_names, is_object_name, normalize, routine_refs, sql_object_refs,
    tokens, ObjectKind, ObjectRef, Tok,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// 从目标例程出发求闭包并落成计划。
///
/// `schema` 只透传进计划，不参与名字归一，也不参与 `deploy` 下发（对象一律进内核默认
/// 目录 `main`，见 architecture §13.6）：链路抽取的归一名是"截尾两段"，与内核 catalog 的
/// `schema.name` 同口径。
pub fn build(
    project: &ProjectConfig,
    target: &str,
    schema: Option<String>,
) -> Result<RunPlan, EngineError> {
    let (eng, routine_file) = register_routines(project)?;
    let ddl_index = ddl_index(project)?;

    let target = normalize(target.trim()).to_ascii_lowercase();
    let entry = eng.catalog.lookup(&target).ok_or_else(|| {
        EngineError::business(
            150,
            format!(
                "计划目标例程 '{target}' 未在项目例程源中登记（检查 sql/routines 与 [[entry]]）"
            ),
        )
    })?;

    let mut ddl_tables: BTreeSet<String> = BTreeSet::new();
    // 静态参照面：项目里**声明**过该对象即算满足，并把声明它的 DDL 文件收进部署清单
    let (seen, missing) = walk_chain(&eng, entry, |kind, name, _| {
        let id = object_identity(kind, name);
        match ddl_file_for(&ddl_index, &id) {
            Some(file) => {
                ddl_tables.insert(file);
                true
            }
            None => engine_provided(&id),
        }
    });

    // 源文件按集合归并：一个文件常声明多个闭包成员，去重排序后才是文件清单
    let mut routine_sources: BTreeSet<String> = BTreeSet::new();
    for name in &seen {
        if let Some(file) = routine_file.get(name) {
            routine_sources.insert(file.clone());
        }
    }
    expand_view_bodies(&mut ddl_tables, &view_sources(project)?, &ddl_index);
    Ok(RunPlan {
        target,
        generator: generator(),
        project_dir: project_dir_text(project),
        schema: schema.map(|s| normalize(&s).to_ascii_lowercase()),
        ddl_routines: routine_sources.into_iter().collect(),
        ddl_tables: ddl_tables.into_iter().collect(),
        materialize: Vec::new(),
        writeback: Vec::new(),
        refs: Vec::new(),
        dynamic: Vec::new(),
        missing: missing.into_iter().collect(),
    })
}

/// 链路 BFS 的**唯一**一份实现（design D5：`plan` 与 `call` 必须同一条链、同一套归属）。
///
/// `on_object(kind, name, from)` 回答"该对象此刻算不算已满足"：`plan` 侧查项目 DDL 声明
/// （顺便把文件收进清单，返回 true），`call` 侧查内核此刻的对象目录。判否的对象进缺件表，
/// `from` 记引用它的例程。例程引用未登记同样进缺件表——缺件面的两种对象走同一个循环。
fn walk_chain<F>(
    eng: &Engine,
    entry: &CompiledRoutine,
    mut on_object: F,
) -> (BTreeSet<String>, BTreeSet<MissingObject>)
where
    F: FnMut(ObjectKind, &str, &str) -> bool,
{
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut missing: BTreeSet<MissingObject> = BTreeSet::new();
    let mut queue: VecDeque<&CompiledRoutine> = VecDeque::new();
    queue.push_back(entry);
    while let Some(r) = queue.pop_front() {
        let from = r.name.to_ascii_lowercase();
        if !seen.insert(from.clone()) {
            continue;
        }
        for o in chain_object_refs(&r.prog) {
            if !on_object(o.kind, &o.name, &from) {
                missing.insert(gap(object_identity(o.kind, &o.name), &from));
            }
        }
        for callee in routine_refs(&r.prog) {
            let callee = normalize(&callee).to_ascii_lowercase();
            let pkg = from.split('.').next().unwrap_or_default();
            match resolve_routine(&callee, pkg, eng) {
                Some(c) => {
                    if !seen.contains(&c.name.to_ascii_lowercase()) {
                        queue.push_back(c);
                    }
                }
                None => {
                    missing.insert(gap(format!("routine:{callee}"), &from));
                }
            }
        }
        // 表达式级裸名调用（`v := fnc_x()`，design A2-5）：只有例程目录能判它是不是项目
        // 函数，判定口径与运行期分派逐字同源——`eval_funccall` 只做一次
        // `catalog.lookup(name)` 精确命中，未命中即按内建/库函数下内核求值。
        // 所以这里命中才进闭包，**未命中不报 `routine:` 缺件**：门不能比它守护的
        // 那次调用更严，否则每个 `trunc(...)` 都会变成假缺件。
        // 带包限定的表达式调用已由 `routine_refs` 收走（含缺件面），按点号跳过。
        for name in expr_func_names(&r.prog) {
            if name.contains('.') {
                continue;
            }
            if let Some(c) = eng.catalog.lookup(&name) {
                if !seen.contains(&c.name.to_ascii_lowercase()) {
                    queue.push_back(c);
                }
            }
        }
    }
    (seen, missing)
}

/// `call` 的运行时缺件门（design D5 / §7 A2-4）：链路上有对象**内核里没有** ⇒ 红，且红之前不执行。
///
/// 与 `plan` 的缺件同一条 BFS、同一套 `from` 归属，差别只在参照面：计划问"项目里有没有
/// 声明"，这里问"内核里此刻有没有"。引擎自建表（`sys_dummy`）一律算满足，口径与
/// [`engine_provided`] 一致——它由 `deploy`/`verify` 的建表环节负责，不是缺件。
/// 返回 `(目录全名, 缺件表)`；目标例程未登记时返回 `None`，那是 `call` 自己的 `BIZ-0150`
/// 面，不在这里改判故障域（§5）。判定用的登记路径与 `Engine::call` 逐字相同（同一个
/// `catalog.lookup`），所以"门没拦下"与"调用会成功"永远是同一批名字。
pub(crate) fn kernel_gaps(
    eng: &Engine,
    target: &str,
    objects: &BTreeSet<ObjectRef>,
) -> Option<(String, Vec<MissingObject>)> {
    let entry = eng.catalog.lookup(target)?;
    // 目录键是小写；摘要行也用同一口径，与 `plan`/`deploy` 的 `target=` 一致
    let name = entry.name.to_ascii_lowercase();
    let (_, missing) = walk_chain(eng, entry, |kind, obj, _| {
        let id = object_identity(kind, obj);
        engine_provided(&id) || kernel_has(objects, &id)
    });
    Some((name, missing.into_iter().collect()))
}

/// 内核目录查名：全名精确优先，其次尾段（口径同 [`ddl_file_for`] 与 `verify::run::present`）。
///
/// 语料写 `bigfund.t_a`、内核登记的是 `main.t_a`，两侧都截尾比对；尾段歧义取第一个，
/// 与内核侧核验同样的近似度，不开第二套判定。
fn kernel_has(objects: &BTreeSet<ObjectRef>, id: &str) -> bool {
    let (prefix, name) = match id.split_once(':') {
        Some(p) => p,
        None => return false,
    };
    let kind = match prefix {
        "relation" => ObjectKind::Relation,
        "sequence" => ObjectKind::Sequence,
        // routine: 前缀只由例程引用产生，不经这里判存在
        _ => return false,
    };
    let tail = name.rsplit('.').next().unwrap_or(name);
    objects.iter().any(|o| {
        o.kind == kind
            && (o.name == name || o.name.rsplit('.').next().unwrap_or(o.name.as_str()) == tail)
    })
}

/// 视图体的基表补全：计划里出现的视图文件，其视图体引用的表/序列也要进部署清单。
///
/// 为什么算闭包缺陷：下发顺序是"表 → 视图"，视图体引用的表没进清单，内核建视图就失败，
/// `deploy_view` 于是静默退化成 `SELECT 1` 占位视图——对象在、列不在，读到该视图列的
/// 语句在内核里报 Binder Error（真题实测）。
/// 只补**有 DDL 文件**的引用：查不到声明的名字（`dual`、外部同义词）不据此报缺件，
/// 维持"由下发期降级处理"的现状——否则一个语料外的别名就能把整份计划判红。
/// 视图套视图沿队列继续展开，`insert` 的去重即终止条件（自引用与环都不转圈）。
fn expand_view_bodies(
    files: &mut BTreeSet<String>,
    views: &BTreeMap<String, String>,
    index: &BTreeMap<String, String>,
) {
    let mut queue: VecDeque<String> = files
        .iter()
        .filter(|f| views.contains_key(f.as_str()))
        .cloned()
        .collect();
    while let Some(file) = queue.pop_front() {
        let Some(text) = views.get(&file) else {
            continue;
        };
        for o in sql_object_refs(text) {
            let Some(dep) = ddl_file_for(index, &object_identity(o.kind, &o.name)) else {
                continue;
            };
            if files.insert(dep.clone()) && views.contains_key(&dep) {
                queue.push_back(dep);
            }
        }
    }
}

/// 视图源文件 → 文本：只有 `sql/ddl/views/` 下有"体"可展开，表/序列 DDL 没有。
fn view_sources(project: &ProjectConfig) -> Result<BTreeMap<String, String>, EngineError> {
    let mut out = BTreeMap::new();
    for f in project.view_files()? {
        out.insert(rel_text(&project.root, &f), read_source_strict(&f)?);
    }
    Ok(out)
}

/// 计划文件的固定落盘子目录：`<home>/plan/`（§11.1 运行态分区，`home` 缺省 `.spinduck`）。
pub fn plan_dir(project: &ProjectConfig) -> PathBuf {
    project.home_dir.join("plan")
}

/// 产出方标注：版本 + schema 口径，红时先怀疑两边不一致。
fn generator() -> String {
    format!(
        "spinduck {} (plan schema {SCHEMA_VERSION})",
        crate::version()
    )
}

/// `project_dir`：项目根相对**计划文件所在目录**的路径（`../..` 形态，不含绝对路径）。
///
/// 层数由 [`plan_dir`] 相对项目根的分量数推出，不写死字面量——落盘位置变了，
/// 清单里的相对路径必须跟着变，否则换机即失效。
pub(crate) fn project_dir_text(project: &ProjectConfig) -> String {
    let dir = plan_dir(project);
    let rel = dir.strip_prefix(&project.root).unwrap_or(dir.as_path());
    let depth = rel.components().count();
    (0..depth).map(|_| "..").collect::<Vec<_>>().join("/")
}

/// 一条缺件：对象标识 + 引用它的例程。
fn gap(object: String, from: &str) -> MissingObject {
    MissingObject {
        object,
        from: from.to_string(),
    }
}

/// `deploy_schema` 自建的表（无源文件）：闭包按已满足处理，且不进部署清单。
///
/// 清单来自 [`crate::verify::ENGINE_PROVIDED_TABLES`]，与建表语句同一口径——
/// 写死在这里等于两套真相。
fn engine_provided(id: &str) -> bool {
    crate::verify::ENGINE_PROVIDED_TABLES
        .iter()
        .any(|t| id == object_identity(ObjectKind::Relation, t))
}

/// 引用名 → 目录全名：全名直接命中；裸名先按调用方所属包补全，再试裸名。
///
/// 与 `verify::run::resolve_routine` 同一套分派规则（语料里同包兄弟过程一律裸名调用，
/// 补全缺失等于对真实项目报假缺件）。两处实现分开的唯一原因是容器类型不同，
/// 规则改动必须同步另一处。
fn resolve_routine<'e>(callee: &str, pkg: &str, eng: &'e Engine) -> Option<&'e CompiledRoutine> {
    if callee.contains('.') {
        return eng.catalog.lookup(callee);
    }
    eng.catalog
        .lookup(&format!("{pkg}.{callee}"))
        .or_else(|| eng.catalog.lookup(callee))
}

/// DDL 索引查名：全名精确优先，其次尾段。
///
/// 口径同 `verify::run::present`：语料写 `bigfund.t_a`，DDL 声明的是裸名 `t_a`。
/// 尾段歧义（两个 schema 同名表）取索引里的第一个，与内核侧核验同样的近似度。
fn ddl_file_for(index: &BTreeMap<String, String>, id: &str) -> Option<String> {
    if let Some(file) = index.get(id) {
        return Some(file.clone());
    }
    let (prefix, name) = id.split_once(':')?;
    let tail = name.rsplit('.').next()?;
    if tail == name {
        return None;
    }
    index.get(&format!("{prefix}:{tail}")).cloned()
}

/// 缺件与 DDL 索引共用的对象标识：`relation:<归一名>` / `sequence:<归一名>`。
fn object_identity(kind: ObjectKind, name: &str) -> String {
    let prefix = match kind {
        ObjectKind::Relation => "relation",
        ObjectKind::Sequence => "sequence",
    };
    format!("{prefix}:{name}")
}

/// 登记全部例程源（含桩包），返回引擎与"例程名 → 相对源文件"。
///
/// 名字归属由逐文件登记前后的目录差集得出，不解析 `CREATE` 语句文本；
/// 同名跨文件重建（`CREATE OR REPLACE`）按后登记者归属。
fn register_routines(
    project: &ProjectConfig,
) -> Result<(Engine, BTreeMap<String, String>), EngineError> {
    let mut eng = Engine::new(Box::new(NoopExecutor), RuleRegistry::default());
    let mut file_of: BTreeMap<String, String> = BTreeMap::new();
    let files: Vec<PathBuf> = project
        .routine_files()?
        .into_iter()
        .chain(project.stub_files()?)
        .collect();
    for f in files {
        let before = eng.catalog.routine_names();
        let text = read_source_strict(&f)?;
        eng.register(&text)?;
        let rel = rel_text(&project.root, &f);
        for name in eng.catalog.routine_names().difference(&before) {
            file_of.insert(name.clone(), rel.clone());
        }
    }
    Ok((eng, file_of))
}

/// DDL 索引：`relation:`/`sequence:` 标识 → 声明它的相对文件。
fn ddl_index(project: &ProjectConfig) -> Result<BTreeMap<String, String>, EngineError> {
    let mut out = BTreeMap::new();
    let files: Vec<PathBuf> = project
        .ddl_files()?
        .into_iter()
        .chain(project.view_files()?)
        .collect();
    for f in files {
        let text = read_source_strict(&f)?;
        let rel = rel_text(&project.root, &f);
        for (kind, name) in ddl_declared(&text) {
            out.insert(object_identity(kind, &name), rel.clone());
        }
    }
    Ok(out)
}

/// 归一名（无 `relation:` / `sequence:` 前缀）→ 相对 DDL 路径。
///
/// codeweb 闭包按这个键核对项目文件，不复用上面带前缀的词法索引。
pub(crate) fn ddl_name_index(
    project: &ProjectConfig,
) -> Result<BTreeMap<String, String>, EngineError> {
    let mut out = BTreeMap::new();
    let files = project
        .ddl_files()?
        .into_iter()
        .chain(project.view_files()?);
    for f in files {
        let text = read_source_strict(&f)?;
        let rel = rel_text(&project.root, &f);
        for (_kind, name) in ddl_declared(&text) {
            out.entry(name).or_insert_with(|| rel.clone());
        }
    }
    Ok(out)
}

/// DDL 文本里被声明的对象（静态词法扫描，口径同链路抽取：不下内核、不建 AST）。
///
/// 只认 `CREATE [OR REPLACE] [MATERIALIZED] TABLE|VIEW|SEQUENCE [IF NOT EXISTS] <名>`；
/// 视图与表同为 `relation`（链路抽取本就归一类）。索引/触发器/类型不是链路对象，不收。
fn ddl_declared(text: &str) -> BTreeSet<(ObjectKind, String)> {
    let toks = tokens(&text.to_ascii_lowercase());
    let mut out = BTreeSet::new();
    let mut i = 0usize;
    while i < toks.len() {
        if word(&toks, i) != Some("create") {
            i += 1;
            continue;
        }
        let Some((kind, name_at)) = declaration(&toks, i) else {
            i += 1;
            continue;
        };
        if let Some(w) = word(&toks, name_at) {
            if is_object_name(w) {
                out.insert((kind, normalize(w)));
            }
        }
        i = name_at;
    }
    out
}

/// `create` 所在位 → (链路对象类别, 名字所在下标)。
///
/// 返回 `None` = 这条 `CREATE` 声明的不是链路对象（索引、触发器、包、同义词等）。
fn declaration(toks: &[Tok], at: usize) -> Option<(ObjectKind, usize)> {
    let mut j = at + 1;
    j += skip_words(toks, j, &["or", "replace"]);
    let kind = match word(toks, j) {
        Some("table") | Some("view") => ObjectKind::Relation,
        Some("sequence") => ObjectKind::Sequence,
        Some("materialized") if word(toks, j + 1) == Some("view") => {
            j += 1;
            ObjectKind::Relation
        }
        _ => return None,
    };
    j += 1;
    j += skip_words(toks, j, &["if", "not", "exists"]);
    Some((kind, j))
}

/// 连续跳过若干关键字（`or replace` / `if not exists`），返回跳过的个数。
fn skip_words(toks: &[Tok], from: usize, words: &[&str]) -> usize {
    let mut n = 0usize;
    while let Some(w) = word(toks, from + n) {
        if words.contains(&w) {
            n += 1;
        } else {
            break;
        }
    }
    n
}

fn word(toks: &[Tok], i: usize) -> Option<&str> {
    match toks.get(i) {
        Some(Tok::Word(w)) => Some(w.as_str()),
        _ => None,
    }
}

/// 相对项目根的路径文本（`/` 分隔，跨平台可比；口径同 `compile` 报告）。
fn rel_text(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
