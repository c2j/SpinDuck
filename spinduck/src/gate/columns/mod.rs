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

use std::collections::{BTreeMap, BTreeSet};

use ogsql_parser::ast::plpgsql::PlDeclaration;
use ogsql_parser::ast::visitor::{walk_statement, Visitor, VisitorResult};
use ogsql_parser::ast::{
    DeleteStatement, Ident, InsertStatement, ObjectName, SelectStatement, SelectTarget, Statement,
    TableRef, UpdateStatement, WithClause,
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
/// 的列引用；返回去重排序结果。
pub fn check_columns(st: &Statement, snap: &SchemaSnapshot) -> Vec<MissingColumn> {
    // Pass 1：收集例程变量名（参数 + DECLARE 变量/记录/游标/嵌套例程
    // 参数），供标识符消歧。超集收集是安全方向（只少报不误报）。
    let mut vars = PlNames::default();
    walk_statement(&mut vars, st);
    let mut gate = Gate {
        snap,
        vars: &vars.0,
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
    out: Vec<MissingColumn>,
}

impl Visitor for Gate<'_> {
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
    /// 检查一个 SELECT 及其内部全部嵌套查询。
    /// `parent` = 本 select 可见的外层作用域（相关子查询用）；
    /// `ctes` = 继承的 CTE 列映射。
    fn select(&mut self, sel: &SelectStatement, parent: &Scope, ctes: &CteCols) {
        // 1. 本层 CTE：名称 → 输出列（显式列清单优先，否则投影推导）。
        let local_ctes = with_ctes(&sel.with, ctes);
        // 2. FROM 作用域
        let local = self.scope_from_refs(&sel.from, &local_ctes);
        let mut vis = local;
        vis.extend(parent.iter().cloned());
        // 3. FROM 内子查询/VALUES/FUNCTION 参数表达式：子查询父作用域
        //    = 本 select 的外层 parent（兄弟 FROM 项不可见）；JOIN 条件
        //    与 VALUES/FUNCTION 参数表达式可见 vis。
        self.check_from_refs(&sel.from, parent, &vis, &local_ctes);
        // 4. 本层表达式
        for t in &sel.targets {
            if let SelectTarget::Expr(e, _) = t {
                self.expr(e, &vis, &local_ctes);
            }
        }
        if let Some(into) = &sel.into_targets {
            for t in into {
                if let SelectTarget::Expr(e, _) = t {
                    self.expr(e, &vis, &local_ctes);
                }
            }
        }
        for e in &sel.distinct_on {
            self.expr(e, &vis, &local_ctes);
        }
        if let Some(w) = &sel.where_clause {
            self.expr(w, &vis, &local_ctes);
        }
        if let Some(c) = &sel.connect_by {
            self.expr(&c.condition, &vis, &local_ctes);
            if let Some(s) = &c.start_with {
                self.expr(s, &vis, &local_ctes);
            }
        }
        for g in &sel.group_by {
            self.group_item(g, &vis, &local_ctes);
        }
        if let Some(h) = &sel.having {
            self.expr(h, &vis, &local_ctes);
        }
        for o in &sel.order_by {
            self.order_item(o, &vis, &local_ctes);
        }
        if let Some(l) = &sel.limit {
            self.expr(l, &vis, &local_ctes);
        }
        if let Some(o) = &sel.offset {
            self.expr(o, &vis, &local_ctes);
        }
        if let Some(f) = &sel.fetch {
            if let Some(c) = &f.count {
                self.expr(c, &vis, &local_ctes);
            }
        }
        for nw in &sel.window_clause {
            self.window_spec(&nw.spec, &vis, &local_ctes);
        }
        // 5. 集合操作右支：继承本层 CTE 与外层作用域
        if let Some(op) = &sel.set_operation {
            let right = match op {
                ogsql_parser::ast::SetOperation::Union { right, .. }
                | ogsql_parser::ast::SetOperation::Intersect { right, .. }
                | ogsql_parser::ast::SetOperation::Except { right, .. } => right,
            };
            self.select(right, parent, &local_ctes);
        }
    }

    fn group_item(&mut self, g: &ogsql_parser::ast::GroupByItem, vis: &Scope, ctes: &CteCols) {
        use ogsql_parser::ast::GroupByItem as G;
        match g {
            G::Expr(e) => self.expr(e, vis, ctes),
            G::GroupingSets(sets) => {
                for e in sets.iter().flatten() {
                    self.expr(e, vis, ctes);
                }
            }
            G::Rollup(items) | G::Cube(items) => {
                for e in items {
                    self.expr(e, vis, ctes);
                }
            }
        }
    }

    fn order_item(&mut self, o: &ogsql_parser::ast::OrderByItem, vis: &Scope, ctes: &CteCols) {
        self.expr(&o.expr, vis, ctes);
        if let Some(u) = &o.using {
            self.expr(u, vis, ctes);
        }
    }

    fn window_spec(&mut self, w: &ogsql_parser::ast::WindowSpec, vis: &Scope, ctes: &CteCols) {
        for e in &w.partition_by {
            self.expr(e, vis, ctes);
        }
        for o in &w.order_by {
            self.order_item(o, vis, ctes);
        }
    }

    /// FROM 引用树的列作用域收集（不递归检查内部查询——那是
    /// `check_from_refs` 的职责）。
    fn scope_from_refs(&self, from: &[TableRef], ctes: &CteCols) -> Scope {
        let mut out = Scope::new();
        for tr in from {
            self.scope_of_ref(tr, &mut out, ctes);
        }
        out
    }

    fn scope_of_ref(&self, tr: &TableRef, out: &mut Scope, ctes: &CteCols) {
        match tr {
            TableRef::Table {
                name,
                alias,
                column_aliases,
                ..
            } => {
                // 列别名是位置重命名；TableDef.columns 为无序 map，
                // 无法按位映射 → 保守 Unknown（V1）。
                let cols = if column_aliases.is_empty() {
                    self.cols_of_name(name, ctes)
                } else {
                    Cols::Unknown
                };
                if let Some(a) = scope_alias(alias, name) {
                    out.push(ScopeEntry { alias: a, cols });
                }
            }
            TableRef::Subquery { query, alias, .. } => {
                if let Some(a) = alias {
                    out.push(ScopeEntry {
                        alias: ident_key(a),
                        cols: projection_cols(query)
                            .map(Cols::Known)
                            .unwrap_or(Cols::Unknown),
                    });
                }
            }
            TableRef::Values {
                alias,
                column_names,
                ..
            } => {
                if let Some(a) = alias {
                    let cols = if column_names.is_empty() {
                        Cols::Unknown
                    } else {
                        Cols::Known(
                            column_names
                                .iter()
                                .map(|n| n.to_ascii_lowercase())
                                .collect(),
                        )
                    };
                    out.push(ScopeEntry {
                        alias: ident_key(a),
                        cols,
                    });
                }
            }
            TableRef::FunctionCall {
                column_defs, alias, ..
            } => {
                if let Some(a) = alias {
                    let cols = if column_defs.is_empty() {
                        Cols::Unknown
                    } else {
                        Cols::Known(
                            column_defs
                                .iter()
                                .map(|c| c.name.to_ascii_lowercase())
                                .collect(),
                        )
                    };
                    out.push(ScopeEntry {
                        alias: ident_key(a),
                        cols,
                    });
                }
            }
            TableRef::Join { left, right, .. } => {
                self.scope_of_ref(left, out, ctes);
                self.scope_of_ref(right, out, ctes);
            }
            // PIVOT/UNPIVOT 输出列与源不同，保守不贡献作用域；
            // 其内部查询由 check_from_refs 递归检查。
            TableRef::Pivot { .. } | TableRef::Unpivot { .. } => {}
        }
    }

    /// FROM 树内嵌查询与表达式的递归检查。`parent` = FROM 子查询的
    /// 父作用域；`vis` = JOIN 条件/VALUES/FUNCTION 参数可见的全作用域。
    fn check_from_refs(&mut self, from: &[TableRef], parent: &Scope, vis: &Scope, ctes: &CteCols) {
        for tr in from {
            self.check_from_ref(tr, parent, vis, ctes);
        }
    }

    fn check_from_ref(&mut self, tr: &TableRef, parent: &Scope, vis: &Scope, ctes: &CteCols) {
        match tr {
            TableRef::Join {
                left,
                right,
                condition,
                ..
            } => {
                self.check_from_ref(left, parent, vis, ctes);
                self.check_from_ref(right, parent, vis, ctes);
                if let Some(c) = condition {
                    self.expr(c, vis, ctes);
                }
            }
            TableRef::Subquery { query, .. } => {
                self.select(query, parent, ctes);
            }
            TableRef::Pivot { source, .. } | TableRef::Unpivot { source, .. } => {
                self.check_from_ref(source, parent, vis, ctes);
            }
            TableRef::Values { values, .. } => {
                for row in &values.rows {
                    for e in row {
                        self.expr(e, vis, ctes);
                    }
                }
                for o in &values.order_by {
                    self.order_item(o, vis, ctes);
                }
                if let Some(l) = &values.limit {
                    self.expr(l, vis, ctes);
                }
                if let Some(o) = &values.offset {
                    self.expr(o, vis, ctes);
                }
            }
            TableRef::FunctionCall { args, .. } => {
                for a in args {
                    self.expr(a, vis, ctes);
                }
            }
            TableRef::Table { .. } => {}
        }
    }

    /// 对象名 → 列集合。解析顺序：CTE → 快照精确键 → 快照后缀
    /// （单段名命中 `schema.obj`）。对象不存在返回 `Cols::Unknown`
    /// （存在性归 Gate 1B 对象级切片，此处只关心列）。
    fn cols_of_name(&self, name: &ObjectName, ctes: &CteCols) -> Cols {
        if name.len() == 1 {
            let key = name[0].value.to_ascii_lowercase();
            if let Some(c) = ctes.get(&key) {
                return c.clone();
            }
        }
        let key = object_key(name);
        if let Some(def) = self.snap.tables.get(&key) {
            return table_cols(def);
        }
        if let Some(v) = self.snap.views.get(&key) {
            return view_cols(v);
        }
        // 单段未限定名后缀匹配（与对象级检查同口径）
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

#[cfg(test)]
mod tests {
    use super::*;
    use ogsql_parser::token::tokenizer::Tokenizer;

    fn snap_of(ddl: &str) -> SchemaSnapshot {
        let tokens = Tokenizer::new(ddl).tokenize().unwrap();
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        SchemaSnapshot::from_statements(&stmts).unwrap()
    }

    fn stmt_of(sql: &str) -> Statement {
        let tokens = Tokenizer::new(sql).tokenize().unwrap();
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        assert_eq!(stmts.len(), 1, "{sql}");
        stmts.into_iter().next().unwrap()
    }

    fn missing(sql: &str, snap: &SchemaSnapshot) -> Vec<(Option<String>, String, &'static str)> {
        check_columns(&stmt_of(sql), snap)
            .into_iter()
            .map(|m| (m.qualifier, m.column, m.problem))
            .collect()
    }

    #[test]
    fn should_flag_unqualified_missing_column() {
        let snap = snap_of("CREATE TABLE t1(a INT, b VARCHAR2(10))");
        assert_eq!(
            missing("SELECT typo_col FROM t1", &snap),
            [(None, "typo_col".to_string(), "missing")]
        );
        assert!(missing("SELECT a, b FROM t1", &snap).is_empty());
    }

    #[test]
    fn should_flag_qualified_missing_column() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        assert_eq!(
            missing("SELECT t1.nope FROM t1", &snap),
            [(Some("t1".to_string()), "nope".to_string(), "missing")]
        );
        // 别名 x 下 a 存在 → 不误报
        assert!(missing("SELECT x.a FROM t1 x", &snap).is_empty());
    }

    #[test]
    fn should_flag_ambiguous_unqualified_column() {
        let snap = snap_of("CREATE TABLE t1(id INT); CREATE TABLE t2(id INT)");
        assert_eq!(
            missing("SELECT id FROM t1, t2", &snap),
            [(None, "id".to_string(), "ambiguous")]
        );
        // 限定引用消歧
        assert!(missing("SELECT t1.id FROM t1, t2", &snap).is_empty());
    }

    #[test]
    fn should_resolve_cte_and_subquery_columns() {
        let snap = snap_of("CREATE TABLE t1(a INT, b INT)");
        // CTE 显式列清单
        assert_eq!(
            missing(
                "WITH c(x, y) AS (SELECT a, b FROM t1) SELECT z FROM c",
                &snap
            ),
            [(None, "z".to_string(), "missing")]
        );
        assert!(missing(
            "WITH c(x, y) AS (SELECT a, b FROM t1) SELECT x FROM c",
            &snap
        )
        .is_empty());
        // 子查询别名投影列
        assert_eq!(
            missing("SELECT s.nope FROM (SELECT a AS k FROM t1) s", &snap),
            [(Some("s".to_string()), "nope".to_string(), "missing")]
        );
        assert!(missing("SELECT s.k FROM (SELECT a AS k FROM t1) s", &snap).is_empty());
        // 含 * 投影 → Unknown，不判缺失
        assert!(missing("SELECT s.anything FROM (SELECT * FROM t1) s", &snap).is_empty());
    }

    #[test]
    fn should_skip_unknown_view_and_deferred_ctas() {
        let snap =
            snap_of("CREATE VIEW v_unk AS SELECT 1; CREATE TEMP TABLE tmp AS SELECT a FROM t1");
        // 视图 Unknown：列引用不判
        assert!(missing("SELECT v.anything FROM v_unk v", &snap).is_empty());
        // CTAS 延迟表：列引用不判（归 Gate 2B）
        assert!(missing("SELECT tmp.whatever FROM tmp", &snap).is_empty());
    }

    #[test]
    fn should_not_flag_pl_variable_as_column() {
        // 例程参数/变量在内嵌 SQL 中不算列引用
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let sql = "CREATE OR REPLACE PROCEDURE p(v_x INT) IS BEGIN \
                   INSERT INTO t1 VALUES (v_x); \
                   FOR r IN SELECT v_x AS k FROM t1 LOOP NULL; END LOOP; \
                   END;";
        assert!(missing(sql, &snap).is_empty(), "{:?}", missing(sql, &snap));
    }

    #[test]
    fn should_check_columns_inside_embedded_sql() {
        let snap = snap_of("CREATE TABLE t1(a INT)");
        let sql = "CREATE OR REPLACE PROCEDURE p IS BEGIN \
                   INSERT INTO t1 SELECT bad_col FROM t1; END;";
        assert_eq!(
            missing(sql, &snap),
            [(None, "bad_col".to_string(), "missing")]
        );
    }

    #[test]
    fn should_flag_insert_column_list_missing() {
        let snap = snap_of("CREATE TABLE t1(a INT, b INT)");
        assert_eq!(
            missing("INSERT INTO t1(nope) SELECT a FROM t1", &snap),
            [(Some("t1".to_string()), "nope".to_string(), "missing")]
        );
        assert!(missing("INSERT INTO t1(a, b) SELECT a, b FROM t1", &snap).is_empty());
    }

    #[test]
    fn should_check_update_and_delete_column_scopes() {
        let snap = snap_of("CREATE TABLE t1(a INT, b INT); CREATE TABLE t2(c INT)");
        // UPDATE 右值/WHERE/SET 左列
        assert_eq!(
            missing("UPDATE t1 SET a = bad_rhs WHERE b = 1", &snap),
            [(None, "bad_rhs".to_string(), "missing")]
        );
        assert_eq!(
            missing("UPDATE t1 SET nope = 1", &snap),
            [(None, "nope".to_string(), "missing")]
        );
        assert_eq!(
            missing("DELETE FROM t1 WHERE nope = 1", &snap),
            [(None, "nope".to_string(), "missing")]
        );
        assert!(missing("UPDATE t1 SET a = 1 WHERE b = 2", &snap).is_empty());
        assert!(missing("DELETE FROM t1 WHERE a = 1", &snap).is_empty());
    }

    #[test]
    fn should_check_update_from_scope() {
        let snap = snap_of("CREATE TABLE t1(a INT); CREATE TABLE t2(b INT)");
        assert!(missing("UPDATE t1 SET a = t2.b FROM t2", &snap).is_empty());
        assert_eq!(
            missing("UPDATE t1 SET a = t2.nope FROM t2", &snap),
            [(Some("t2".to_string()), "nope".to_string(), "missing")]
        );
    }

    #[test]
    fn should_check_percent_type_and_rowtype_anchors() {
        let snap = snap_of("CREATE TABLE t1(a INT, b INT)");
        // %TYPE 列存在 → 放行；列缺失/表缺失 → 上报
        let ok = "CREATE PROCEDURE p IS v_a t1.a%TYPE; BEGIN NULL; END;";
        assert!(missing(ok, &snap).is_empty(), "{:?}", missing(ok, &snap));
        let bad_col = "CREATE PROCEDURE p IS v_a t1.nope%TYPE; BEGIN NULL; END;";
        assert_eq!(
            missing(bad_col, &snap),
            [(Some("t1".to_string()), "nope".to_string(), "missing")]
        );
        let bad_tbl = "CREATE PROCEDURE p IS v_a ghost.x%TYPE; BEGIN NULL; END;";
        assert_eq!(
            missing(bad_tbl, &snap),
            [(Some("ghost".to_string()), "x".to_string(), "missing_table")]
        );
        let bad_row = "CREATE PROCEDURE p IS r t1%ROWTYPE; v_g ghost%ROWTYPE; BEGIN NULL; END;";
        assert_eq!(
            missing(bad_row, &snap),
            [(
                Some("ghost".to_string()),
                "%rowtype".to_string(),
                "missing_table"
            )]
        );
    }
}
