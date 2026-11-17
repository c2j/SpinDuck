//! Gate 1B 列级检查——SELECT 作用域/CTE 推导与 FROM 树递归
//! （`columns` 模块子文件，AGENTS §3 单文件 ≤500 行拆分）。

use ogsql_parser::ast::{SelectStatement, SelectTarget, TableRef};

use super::{
    ident_key, projection_cols, scope_alias, with_ctes, Cols, CteCols, Gate, Scope, ScopeEntry,
};

impl Gate<'_> {
    /// 检查一个 SELECT 及其内部全部嵌套查询。
    /// `parent` = 本 select 可见的外层作用域（相关子查询用）；
    /// `ctes` = 继承的 CTE 列映射。
    pub(super) fn select(&mut self, sel: &SelectStatement, parent: &Scope, ctes: &CteCols) {
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

    pub(super) fn group_item(
        &mut self,
        g: &ogsql_parser::ast::GroupByItem,
        vis: &Scope,
        ctes: &CteCols,
    ) {
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

    pub(super) fn order_item(
        &mut self,
        o: &ogsql_parser::ast::OrderByItem,
        vis: &Scope,
        ctes: &CteCols,
    ) {
        self.expr(&o.expr, vis, ctes);
        if let Some(u) = &o.using {
            self.expr(u, vis, ctes);
        }
    }

    pub(super) fn window_spec(
        &mut self,
        w: &ogsql_parser::ast::WindowSpec,
        vis: &Scope,
        ctes: &CteCols,
    ) {
        for e in &w.partition_by {
            self.expr(e, vis, ctes);
        }
        for o in &w.order_by {
            self.order_item(o, vis, ctes);
        }
    }

    /// FROM 引用树的列作用域收集（不递归检查内部查询——那是
    /// `check_from_refs` 的职责）。
    pub(super) fn scope_from_refs(&self, from: &[TableRef], ctes: &CteCols) -> Scope {
        let mut out = Scope::new();
        for tr in from {
            self.scope_of_ref(tr, &mut out, ctes);
        }
        out
    }

    pub(super) fn scope_of_ref(&self, tr: &TableRef, out: &mut Scope, ctes: &CteCols) {
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
    pub(super) fn check_from_refs(
        &mut self,
        from: &[TableRef],
        parent: &Scope,
        vis: &Scope,
        ctes: &CteCols,
    ) {
        for tr in from {
            self.check_from_ref(tr, parent, vis, ctes);
        }
    }

    pub(super) fn check_from_ref(
        &mut self,
        tr: &TableRef,
        parent: &Scope,
        vis: &Scope,
        ctes: &CteCols,
    ) {
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
}
