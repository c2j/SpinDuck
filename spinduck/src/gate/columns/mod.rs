//! Gate 1B 列级检查（设计 V4 §4.3 Stage 3A）：在 `SchemaSnapshot` 上校验
//! SQL 语句中的列引用——限定/非限定/歧义。纯函数；诊断位置与
//! `BIZ-0152` 归一由编排层负责。
//!
//! V1 保守口径（设计评审记录）：
//! - 视图 `Unknown`、CTAS 延迟表、含 `*`/无别名投影的子查询/CTE：
//!   列集合不可静态确定，对来源该表的列引用一律不判缺失；
//! - 标识符若命中例程变量名（参数 + DECLARE 变量/记录/游标/嵌套
//!   例程参数，整条语句级并集），按 PL 变量解释跳过——内嵌 SQL 与
//!   游标/EXECUTE/FOR 查询共用此口径；
//! - 记录字段、三段以上限定名、`agg_from` 表函数引用、MERGE
//!   语句不在本切片判定。

mod dml;
mod expr;
mod select;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use ogsql_parser::ast::plpgsql::PlDeclaration;
use ogsql_parser::ast::visitor::{walk_statement, Visitor, VisitorResult};
use ogsql_parser::ast::{
    DeleteStatement, Ident, InsertStatement, ObjectName, SelectStatement, SelectTarget, Statement,
    UpdateStatement, WithClause,
};

use super::schema::{object_key, SchemaSnapshot};

/// 单个列级缺失/歧义诊断（`BIZ-0152` 前的中间形态）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MissingColumn {
    /// 限定符（`alias.col` 的 alias；非限定引用为 `None`）。
    pub qualifier: Option<String>,
    /// 列名（归一化小写）。
    pub column: String,
    /// 问题类别：`"missing"` / `"ambiguous"` / `"missing_table"`
    /// （`%TYPE`/`%ROWTYPE` 锚点对象不存在）。
    pub problem: &'static str,
}

/// 列集合解析结果：`Unknown` = 静态不可确定（视图 Unknown / CTAS
/// 延迟 / 含 `*` 投影），对来源该表的引用一律不判缺失。
#[derive(Debug, Clone)]
enum Cols {
    Known(BTreeSet<String>),
    Unknown,
}

/// FROM 作用域条目：`alias` → 列集合。作用域向量头部为最内层，
/// 查找自前向后即内→外遮蔽序。
#[derive(Debug, Clone)]
struct ScopeEntry {
    alias: String,
    cols: Cols,
}

type Scope = Vec<ScopeEntry>;

/// CTE 名 → 输出列集合（随 select 嵌套继承）。
type CteCols = BTreeMap<String, Cols>;

/// 检查一条顶层语句（含 PL 体内嵌 SQL、游标/FOR/EXECUTE 解析查询）
/// 的列引用；返回去重排序结果。例程体内 `CREATE [TEMP] TABLE`
/// 声明的会话级对象按"单语句作用域 + 声明顺序"累积（walk 到声明
/// 才可见）：对象视为已知、列集合 `Cols::Unknown` 延迟 Gate 2B。
pub fn check_columns(st: &Statement, snap: &SchemaSnapshot) -> Vec<MissingColumn> {
    // Pass 1：收集例程变量名（参数 + DECLARE 变量/记录/游标/嵌套例程
    // 参数），供标识符消歧。超集收集是安全方向（只少报不误报）。
    let mut vars = PlNames::default();
    walk_statement(&mut vars, st);
    let mut gate = Gate {
        snap,
        vars: &vars.0,
        local: BTreeSet::new(),
        out: Vec::new(),
    };
    walk_statement(&mut gate, st);
    gate.out.sort();
    gate.out.dedup();
    gate.out
}

/// Pass 1：PL 变量名收集器（语句级并集，跨块作用域不做精确遮蔽）。
#[derive(Default)]
struct PlNames(BTreeSet<String>);

impl PlNames {
    fn add_params(&mut self, params: &[ogsql_parser::ast::RoutineParam]) {
        for p in params {
            if !p.name.is_empty() {
                self.0.insert(p.name.to_ascii_lowercase());
            }
        }
    }

    fn add_package_items(&mut self, items: &[ogsql_parser::ast::PackageItem]) {
        use ogsql_parser::ast::PackageItem;
        for item in items {
            match item {
                PackageItem::Procedure(p) => self.add_params(&p.parameters),
                PackageItem::Function(f) => self.add_params(&f.parameters),
                _ => {}
            }
        }
    }
}

impl Visitor for PlNames {
    fn visit_statement(&mut self, st: &Statement) -> VisitorResult {
        use ogsql_parser::ast::Statement as S;
        match st {
            S::CreateProcedure(p) => self.add_params(&p.parameters),
            S::CreateFunction(f) => self.add_params(&f.parameters),
            S::CreatePackage(p) => self.add_package_items(&p.items),
            S::CreatePackageBody(p) => self.add_package_items(&p.items),
            _ => {}
        }
        VisitorResult::Continue
    }

    fn visit_pl_declaration(
        &mut self,
        decl: &ogsql_parser::ast::plpgsql::PlDeclaration,
    ) -> VisitorResult {
        use ogsql_parser::ast::plpgsql::PlDeclaration;
        match decl {
            PlDeclaration::Variable(v) => {
                self.0.insert(v.name.to_ascii_lowercase());
            }
            PlDeclaration::Record(r) => {
                self.0.insert(r.name.to_ascii_lowercase());
            }
            PlDeclaration::Cursor(c) => {
                self.0.insert(c.name.to_ascii_lowercase());
                for a in &c.arguments {
                    self.0.insert(a.name.to_ascii_lowercase());
                }
            }
            PlDeclaration::Type(_) => {}
            PlDeclaration::NestedProcedure(p) => self.add_params(&p.parameters),
            PlDeclaration::NestedFunction(f) => self.add_params(&f.parameters),
            PlDeclaration::Pragma { .. } => {}
        }
        VisitorResult::Continue
    }
}

/// Pass 2：列引用检查器（SELECT 及后续 DML/锚点切片共用）。
struct Gate<'a> {
    snap: &'a SchemaSnapshot,
    vars: &'a BTreeSet<String>,
    /// 本语句内已声明的局部表（归一化限定名）：walk 顺序累积，
    /// 声明之后的引用视为 `Cols::Unknown`（列延迟 Gate 2B）。
    local: BTreeSet<String>,
    out: Vec<MissingColumn>,
}

impl Visitor for Gate<'_> {
    fn visit_statement(&mut self, st: &Statement) -> VisitorResult {
        // 例程体内（SqlStatement 下钻）与顶层的 CREATE [TEMP] TABLE：
        // 声明名按 walk 顺序纳入本地集合，供其后引用判定。
        if let Some(key) = super::declared_table_key(st) {
            self.local.insert(key);
        }
        VisitorResult::Continue
    }

    fn visit_select(&mut self, sel: &SelectStatement) -> VisitorResult {
        // select() 自行递归全部子查询；SkipChildren 防止 walk 重复进入。
        self.select(sel, &Scope::new(), &CteCols::new());
        VisitorResult::SkipChildren
    }

    fn visit_insert(&mut self, i: &InsertStatement) -> VisitorResult {
        self.dml_insert(i);
        VisitorResult::SkipChildren
    }

    fn visit_update(&mut self, u: &UpdateStatement) -> VisitorResult {
        self.dml_update(u);
        VisitorResult::SkipChildren
    }

    fn visit_delete(&mut self, d: &DeleteStatement) -> VisitorResult {
        self.dml_delete(d);
        VisitorResult::SkipChildren
    }

    fn visit_pl_declaration(&mut self, decl: &PlDeclaration) -> VisitorResult {
        // %TYPE/%ROWTYPE 锚点校验；返回 Continue 让游标 parsed_query
        // 等子节点继续走正常 walk（内嵌 SQL 由各 visit_* 钩子覆盖）。
        self.pl_decl(decl);
        VisitorResult::Continue
    }
}

impl Gate<'_> {
    /// 对象名 → 列集合。解析顺序：CTE → 文件级 PL 临时表 → 快照精确键
    /// → 快照后缀（单段名命中 `schema.obj`）。对象不存在返回
    /// `Cols::Unknown`（存在性归 Gate 1B 对象级切片，此处只关心列）。
    fn cols_of_name(&self, name: &ObjectName, ctes: &CteCols) -> Cols {
        if name.len() == 1 {
            let key = name[0].value.to_ascii_lowercase();
            if let Some(c) = ctes.get(&key) {
                return c.clone();
            }
        }
        let key = object_key(name);
        // 文件级 PL 临时表：对象已知、列集合延迟（CTAS 口径）
        if self.local_table(name, &key) {
            return Cols::Unknown;
        }
        if let Some(def) = self.snap.tables.get(&key) {
            return table_cols(def);
        }
        if let Some(v) = self.snap.views.get(&key) {
            return view_cols(v);
        }
        // 引擎自建伪表/Oracle dual：存在性成立；列集合不可静态
        // 确定（sys_dummy 为 CTAS）→ Unknown 不判；dual 按 Oracle
        // 口径仅 DUMMY 列。
        if name.len() == 1 {
            let want = name[0].value.to_ascii_lowercase();
            if crate::verify::ENGINE_PROVIDED_TABLES
                .iter()
                .any(|t| *t == want)
            {
                return Cols::Unknown;
            }
            if want == "dual" {
                return Cols::Known(BTreeSet::from(["dummy".to_string()]));
            }
        }
        // 单段未限定名后缀匹配（与对象级检查同口径；局部表仅精确键）
        if name.len() == 1 {
            let want = name[0].value.to_ascii_lowercase();
            let hit = |k: &String| k.rsplit('.').next() == Some(want.as_str());
            if let Some(k) = self.snap.tables.keys().find(|k| hit(k)) {
                return table_cols(&self.snap.tables[k]);
            }
            if let Some(k) = self.snap.views.keys().find(|k| hit(k)) {
                return view_cols(&self.snap.views[k]);
            }
        }
        Cols::Unknown
    }

    /// 局部表判定：仅精确键命中（与对象级 `local_known` 同口径，
    /// 后缀匹配会让无关同叶对象被误当已知）。
    fn local_table(&self, _name: &ObjectName, key: &str) -> bool {
        self.local.contains(key)
    }
}

fn table_cols(def: &super::schema::TableDef) -> Cols {
    if def.columns_deferred {
        Cols::Unknown
    } else {
        Cols::Known(def.columns.keys().cloned().collect())
    }
}

fn view_cols(v: &super::schema::ViewDef) -> Cols {
    match v {
        super::schema::ViewDef::Known(cols) => Cols::Known(cols.iter().cloned().collect()),
        super::schema::ViewDef::Unknown => Cols::Unknown,
    }
}

/// `WITH` 子句 → CTE 列映射（叠加在继承映射上；后者同名遮蔽）。
fn with_ctes(with: &Option<WithClause>, base: &CteCols) -> CteCols {
    let mut m = base.clone();
    if let Some(w) = with {
        for c in &w.ctes {
            let cols = if !c.columns.is_empty() {
                Cols::Known(c.columns.iter().map(|n| n.to_ascii_lowercase()).collect())
            } else {
                projection_cols(&c.query)
                    .map(Cols::Known)
                    .unwrap_or(Cols::Unknown)
            };
            m.insert(c.name.to_ascii_lowercase(), cols);
        }
    }
    m
}

/// 子查询/CTE 的输出列名：全部投影项带显式别名才可静态确定；
/// 含 `*` 或无别名表达式 → `None`（Unknown）。
fn projection_cols(sel: &SelectStatement) -> Option<BTreeSet<String>> {
    let mut cols = BTreeSet::new();
    for t in &sel.targets {
        match t {
            SelectTarget::Expr(_, Some(alias)) => {
                cols.insert(alias.value.to_ascii_lowercase());
            }
            _ => return None,
        }
    }
    Some(cols)
}

/// FROM 别名：显式 alias 优先，否则取对象名末段。
fn scope_alias(alias: &Option<Ident>, name: &ObjectName) -> Option<String> {
    if let Some(a) = alias {
        return Some(ident_key(a));
    }
    name.last().map(ident_seg)
}

fn ident_key(i: &Ident) -> String {
    if i.quote_style.is_some() {
        i.value.clone()
    } else {
        i.value.to_ascii_lowercase()
    }
}

fn ident_seg(i: &Ident) -> String {
    i.value.to_ascii_lowercase()
}
