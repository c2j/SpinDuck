//! Gate 1B 列级检查——DML（INSERT/UPDATE/DELETE）作用域与
//! `%TYPE`/`%ROWTYPE` 锚点校验（`columns` 模块子文件，AGENTS §3
//! 单文件 ≤500 行拆分）。

use ogsql_parser::ast::plpgsql::{PlDataType, PlDeclaration, PlTypeDecl};
use ogsql_parser::ast::{
    ConflictAction, ConflictTarget, DeleteStatement, Ident, InsertSource, InsertStatement,
    ObjectName, OnDuplicateKeyUpdate, SelectTarget, UpdateStatement,
};

use crate::gate::schema::object_key;

use super::{scope_alias, with_ctes, Cols, CteCols, Gate, Scope, ScopeEntry};

impl Gate<'_> {
    /// INSERT：列清单校验 + RETURNING/ON CONFLICT 作用域（目标表）+
    /// source 各形态下钻。
    pub(super) fn dml_insert(&mut self, i: &InsertStatement) {
        let ctes = with_ctes(&i.with, &CteCols::new());
        let tgt = self.cols_of_name(&i.table, &ctes);
        let tkey = object_key(&i.table);
        // INSERT 列清单：目标列集合可知时逐一校验
        if let Cols::Known(cols) = &tgt {
            for c in &i.columns {
                if !cols.contains(&c.to_ascii_lowercase()) {
                    self.missing(Some(tkey.clone()), c.to_ascii_lowercase(), "missing");
                }
            }
        }
        // RETURNING/ON CONFLICT/ON DUPLICATE 的列引用作用域 = 目标表
        let mut vis = Scope::new();
        if let Some(a) = scope_alias(&i.alias, &i.table) {
            vis.push(ScopeEntry {
                alias: a,
                cols: tgt.clone(),
            });
        }
        match &i.source {
            // INSERT..SELECT 的子查询不看到目标表（标准 SQL）
            InsertSource::Select(s) => self.select(s, &Scope::new(), &ctes),
            InsertSource::Values(rows) => {
                let empty = Scope::new();
                for row in rows {
                    for e in row {
                        self.expr(e, &empty, &ctes);
                    }
                }
            }
            InsertSource::DefaultValues => {}
            InsertSource::Set(assigns) => {
                let empty = Scope::new();
                for a in assigns {
                    self.expr(&a.value, &empty, &ctes);
                }
            }
            InsertSource::RecordVariable(e) => self.expr(e, &Scope::new(), &ctes),
        }
        if let Some(OnDuplicateKeyUpdate::Update {
            assignments,
            where_clause,
        }) = &i.on_duplicate_key
        {
            for a in assignments {
                self.expr(&a.value, &vis, &ctes);
            }
            if let Some(w) = where_clause {
                self.expr(w, &vis, &ctes);
            }
        }
        if let Some(oc) = &i.on_conflict {
            if let Some(ConflictTarget::Columns(cols)) = &oc.target {
                if let Cols::Known(tcols) = &tgt {
                    for c in cols {
                        if !tcols.contains(&c.to_ascii_lowercase()) {
                            self.missing(Some(tkey.clone()), c.to_ascii_lowercase(), "missing");
                        }
                    }
                }
            }
            if let ConflictAction::DoUpdate {
                assignments,
                where_clause,
            } = &oc.action
            {
                for a in assignments {
                    self.expr(&a.value, &vis, &ctes);
                }
                if let Some(w) = where_clause {
                    self.expr(w, &vis, &ctes);
                }
            }
        }
        let into = i.into_targets.iter().flat_map(|v| v.iter());
        for t in i.returning.iter().chain(into) {
            if let SelectTarget::Expr(e, _) = t {
                self.expr(e, &vis, &ctes);
            }
        }
    }

    /// UPDATE：作用域 = 目标表 + FROM 源；SET 右值/WHERE/ORDER/LIMIT/
    /// RETURNING 表达式与 SET 左列名均按该作用域判定。
    pub(super) fn dml_update(&mut self, u: &UpdateStatement) {
        let ctes = with_ctes(&u.with, &CteCols::new());
        let mut vis = self.scope_from_refs(&u.tables, &ctes);
        vis.extend(self.scope_from_refs(&u.from, &ctes));
        for a in &u.assignments {
            for c in &a.columns {
                self.assign_column(c, &vis);
            }
            self.expr(&a.value, &vis, &ctes);
        }
        if let Some(w) = &u.where_clause {
            self.expr(w, &vis, &ctes);
        }
        if let Some(obs) = &u.order_by {
            for o in obs {
                self.order_item(o, &vis, &ctes);
            }
        }
        if let Some(l) = &u.limit {
            self.expr(l, &vis, &ctes);
        }
        let into = u.into_targets.iter().flat_map(|v| v.iter());
        for t in u.returning.iter().chain(into) {
            if let SelectTarget::Expr(e, _) = t {
                self.expr(e, &vis, &ctes);
            }
        }
    }

    /// DELETE：作用域 = 目标表 + USING 源。
    pub(super) fn dml_delete(&mut self, d: &DeleteStatement) {
        let ctes = with_ctes(&d.with, &CteCols::new());
        let mut vis = self.scope_from_refs(&d.tables, &ctes);
        vis.extend(self.scope_from_refs(&d.using, &ctes));
        if let Some(w) = &d.where_clause {
            self.expr(w, &vis, &ctes);
        }
        if let Some(obs) = &d.order_by {
            for o in obs {
                self.order_item(o, &vis, &ctes);
            }
        }
        if let Some(l) = &d.limit {
            self.expr(l, &vis, &ctes);
        }
        let into = d.into_targets.iter().flat_map(|v| v.iter());
        for t in d.returning.iter().chain(into) {
            if let SelectTarget::Expr(e, _) = t {
                self.expr(e, &vis, &ctes);
            }
        }
    }

    /// SET 左列名（`ObjectName`，可限定 `t.col`）：按非限定列
    /// 规则在全作用域求命中。
    fn assign_column(&mut self, name: &ObjectName, vis: &Scope) {
        let segs: Vec<String> = name.iter().map(super::ident_seg).collect();
        match segs.len() {
            1 => self.unqualified(&segs[0], vis),
            2 => self.qualified(&segs[0], &segs[1], vis),
            _ => {}
        }
    }

    /// `%TYPE`/`%ROWTYPE` 锚点与声明内嵌类型递归校验。
    pub(super) fn pl_decl(&mut self, decl: &PlDeclaration) {
        match decl {
            PlDeclaration::Variable(v) => self.check_pl_type(&v.data_type),
            PlDeclaration::Cursor(c) => {
                if let Some(rt) = &c.return_type {
                    self.check_pl_type(rt);
                }
            }
            PlDeclaration::Type(t) => match t {
                PlTypeDecl::Record { fields, .. } => {
                    for f in fields {
                        self.check_pl_type(&f.data_type);
                    }
                }
                PlTypeDecl::TableOf { elem_type, .. } | PlTypeDecl::VarrayOf { elem_type, .. } => {
                    self.check_pl_type(elem_type)
                }
                PlTypeDecl::RefCursor { .. } => {}
            },
            _ => {}
        }
    }

    fn check_pl_type(&mut self, t: &PlDataType) {
        match t {
            PlDataType::PercentType { table, column } => {
                // 单段锚命中例程变量/游标/记录名 = PL 局部锚（`rec%TYPE`），
                // 不是 schema 对象——按 V1 保守口径跳过（只少报不误报）。
                if self.pl_local_anchor(table) {
                    return;
                }
                let name = anchor_name(table);
                if !self.object_known(&name) {
                    self.missing(
                        Some(table.clone()),
                        column.to_ascii_lowercase(),
                        "missing_table",
                    );
                    return;
                }
                if let Cols::Known(cols) = self.cols_of_name(&name, &CteCols::new()) {
                    if !cols.contains(&column.to_ascii_lowercase()) {
                        self.missing(Some(table.clone()), column.to_ascii_lowercase(), "missing");
                    }
                }
            }
            PlDataType::PercentRowType(table) => {
                // `cursor%ROWTYPE`/`rec%ROWTYPE`：锚是例程局部游标或记录，
                // 不是 schema 对象（考题语料 `r_get_data c_get_data%ROWTYPE`
                // 形态实测误报）；表锚不受影响。
                if self.pl_local_anchor(table) {
                    return;
                }
                let name = anchor_name(table);
                if !self.object_known(&name) {
                    self.missing(Some(table.clone()), "%rowtype".to_string(), "missing_table");
                }
            }
            PlDataType::TypeName(_)
            | PlDataType::Record
            | PlDataType::Cursor
            | PlDataType::RefCursor => {}
        }
    }

    /// 锚是例程局部名（单段且命中 Pass 1 收集的变量/记录/游标名并集）：
    /// PL 声明先于 SQL 可见，同名表按 PL 遮蔽口径归局部锚。
    fn pl_local_anchor(&self, table: &str) -> bool {
        !table.contains('.') && self.vars.contains(&table.to_ascii_lowercase())
    }

    /// 锚点对象存在性：表/视图（精确键或单段后缀）+ 文件级 PL
    /// 临时表 + 引擎自建伪表与 `dual`；不含 CTE/序列。
    fn object_known(&self, name: &ObjectName) -> bool {
        let key = object_key(name);
        if self.local_table(name, &key) {
            return true;
        }
        if self.snap.tables.contains_key(&key) || self.snap.views.contains_key(&key) {
            return true;
        }
        if name.len() == 1 {
            let want = name[0].value.to_ascii_lowercase();
            if want == "dual"
                || crate::verify::ENGINE_PROVIDED_TABLES
                    .iter()
                    .any(|t| *t == want)
            {
                return true;
            }
            let hit = |k: &String| k.rsplit('.').next() == Some(want.as_str());
            return self.snap.tables.keys().any(hit) || self.snap.views.keys().any(hit);
        }
        false
    }
}

/// 锚点字符串（`table`/`schema.table`）→ ObjectName；统一小写
/// unquoted 口径（与 `object_key` 一致）。
fn anchor_name(s: &str) -> ObjectName {
    s.split('.')
        .map(|p| Ident {
            value: p.to_ascii_lowercase(),
            quote_style: None,
        })
        .collect()
}
