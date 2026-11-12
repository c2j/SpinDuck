//! 闭包求值（design §6.1 / R3）：目标例程 → 源文件清单 + 链路对象清单 + 缺件表。
//!
//! `build` 只走一次 codeweb 导出。词法 BFS 只留给旧计划与裸 `call` 的内核缺件门。

use crate::catalog::CompiledRoutine;
use crate::cli::read_source_strict;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::plan::codeweb::{plans_from_export, CodewebExport};
use crate::plan::{MissingObject, PlanRef, RunPlan};
use crate::project::ProjectConfig;
use crate::verify::{
    chain_object_refs, expr_func_names, is_object_name, normalize, routine_refs, tokens,
    ObjectKind, ObjectRef, Tok,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// 从目标例程出发求闭包并落成计划。一次导出，一个入口。
///
/// `schema` 只写入计划：整段 `trim` + ASCII 小写，不经 `normalize`（不截段）。
/// 导出失败即返回，不回退词法闭包。
pub fn build(
    project: &ProjectConfig,
    target: &str,
    schema: Option<String>,
    export: &impl CodewebExport,
) -> Result<RunPlan, EngineError> {
    let entry = target.to_string();
    let mut plans = plans_from_export(project, std::slice::from_ref(&entry), schema, export)?;
    plans
        .pop()
        .ok_or_else(|| EngineError::invariant(30, "单入口没有产出计划"))
}

/// 旧计划与裸 `call` 的词法 BFS。`build` 不再调用它。
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

/// `--plan` 且 `generator` 以 `codeweb ` 开头时的缺件门。不调用 [`walk_chain`]。
///
/// 只把 `table` / `view` / `sequence` 的 `[[ref]]`（含 `role=type`）变成
/// `relation:` / `sequence:` 再问 [`kernel_has`]。`[[dynamic]]` 与 `kind=routine` 不进这道门。
/// 目标例程未登记返回 `None`，留给 `Engine::call` 的 `BIZ-0150`。
pub(crate) fn kernel_gaps_from_refs(
    eng: &Engine,
    target: &str,
    objects: &BTreeSet<ObjectRef>,
    refs: &[PlanRef],
) -> Option<(String, Vec<MissingObject>)> {
    let entry = eng.catalog.lookup(target)?;
    let name = entry.name.to_ascii_lowercase();
    let mut missing: BTreeSet<MissingObject> = BTreeSet::new();
    for r in refs {
        let Some(id) = ref_object_id(&r.kind, &r.object) else {
            continue;
        };
        if !kernel_has(objects, &id) {
            missing.insert(gap(id, &r.from));
        }
    }
    Some((name, missing.into_iter().collect()))
}

fn ref_object_id(kind: &str, object: &str) -> Option<String> {
    let prefix = match kind {
        "table" | "view" => "relation",
        "sequence" => "sequence",
        _ => return None,
    };
    Some(format!("{prefix}:{object}"))
}

/// 内核目录查名：全名精确优先，其次尾段（口径同 `verify::run::present`）。
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

/// 计划文件的固定落盘子目录：`<home>/plan/`（§11.1 运行态分区，`home` 缺省 `.spinduck`）。
pub fn plan_dir(project: &ProjectConfig) -> PathBuf {
    project.home_dir.join("plan")
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

/// 缺件与词法门共用的对象标识：`relation:<归一名>` / `sequence:<归一名>`。
fn object_identity(kind: ObjectKind, name: &str) -> String {
    let prefix = match kind {
        ObjectKind::Relation => "relation",
        ObjectKind::Sequence => "sequence",
    };
    format!("{prefix}:{name}")
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

#[cfg(test)]
mod tests {
    use super::kernel_gaps_from_refs;
    use crate::cli::NoopExecutor;
    use crate::engine::Engine;
    use crate::plan::PlanRef;
    use crate::rules::RuleRegistry;
    use crate::verify::{ObjectKind, ObjectRef};
    use std::collections::BTreeSet;

    fn row(object: &str, kind: &str, role: &str) -> PlanRef {
        PlanRef {
            object: object.to_string(),
            kind: kind.to_string(),
            role: role.to_string(),
            from: "prc_main".to_string(),
            at: "sql/r.sql:1".to_string(),
        }
    }

    #[test]
    fn should_check_codeweb_refs_against_the_kernel_without_reading_the_body() {
        // 过程体是 NULL。词法门看不到任何表；这道门只看 ref。
        let mut eng = Engine::new(Box::new(NoopExecutor), RuleRegistry::default());
        eng.register("CREATE OR REPLACE PROCEDURE prc_main IS BEGIN NULL; END;")
            .unwrap();
        let objects = BTreeSet::from([
            ObjectRef {
                kind: ObjectKind::Relation,
                name: "main.t_present".to_string(),
            },
            ObjectRef {
                kind: ObjectKind::Relation,
                name: "main.v_anchor".to_string(),
            },
        ]);
        let refs = vec![
            row("t_absent", "table", "read"),
            row("t_present", "table", "write"),
            row("v_anchor", "view", "type"),
            row("seq_absent", "sequence", "read"),
            row("prc_other", "routine", "call"),
        ];
        let (name, gaps) = kernel_gaps_from_refs(&eng, "prc_main", &objects, &refs).unwrap();
        assert_eq!(name, "prc_main");
        assert_eq!(
            gaps.iter().map(|g| g.object.as_str()).collect::<Vec<_>>(),
            vec!["relation:t_absent", "sequence:seq_absent"]
        );
        assert!(gaps.iter().all(|g| g.from == "prc_main"));
        assert!(kernel_gaps_from_refs(&eng, "prc_missing", &objects, &refs).is_none());
    }
}
