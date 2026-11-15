//! Gate 1B 列级检查——表达式递归与 ColumnRef 判定（`columns` 模块子文件，
//! 拆分理由：AGENTS §3 单文件 ≤500 行）。

use ogsql_parser::ast::Expr;

use super::{Cols, CteCols, Gate, Scope};

impl Gate<'_> {
    /// 列引用判定：`qualifier=None` 非限定，`Some(q)` 限定。
    pub(super) fn missing(
        &mut self,
        qualifier: Option<String>,
        column: String,
        problem: &'static str,
    ) {
        self.out.push(super::MissingColumn {
            qualifier,
            column,
            problem,
        });
    }

    /// 解析一个 ColumnRef（含 `(+)` 变体）。
    fn column_ref(&mut self, name: &ogsql_parser::ast::ObjectName, vis: &Scope) {
        let segs: Vec<String> = name.iter().map(super::ident_seg).collect();
        match segs.len() {
            1 => self.unqualified(&segs[0], vis),
            2 => self.qualified(&segs[0], &segs[1], vis),
            // 三段以上（库.表.列/记录嵌套）V1 不判
            _ => {}
        }
    }

    pub(super) fn unqualified(&mut self, col: &str, vis: &Scope) {
        if self.vars.contains(col) {
            return;
        }
        let mut hits = 0usize;
        let mut unknown = false;
        for e in vis {
            match &e.cols {
                Cols::Known(cols) if cols.contains(col) => hits += 1,
                Cols::Unknown => unknown = true,
                _ => {}
            }
        }
        if hits > 1 {
            self.missing(None, col.to_string(), "ambiguous");
        } else if hits == 0 && !unknown {
            self.missing(None, col.to_string(), "missing");
        }
    }

    pub(super) fn qualified(&mut self, qual: &str, col: &str, vis: &Scope) {
        if self.vars.contains(qual) {
            // record.field 形态：首段为 PL 变量
            return;
        }
        for e in vis {
            if e.alias != qual {
                continue;
            }
            if let Cols::Known(cols) = &e.cols {
                if !cols.contains(col) {
                    self.missing(Some(qual.to_string()), col.to_string(), "missing");
                }
            }
            return;
        }
        // 限定符不是任何在作用域别名 → 列必然不可解析
        self.missing(Some(qual.to_string()), col.to_string(), "missing");
    }

    /// 表达式递归：ColumnRef 判定 + 全部子表达式/子查询下钻。
    /// `vis` = 当前 select 可见作用域（内→外已展平）。
    pub(super) fn expr(&mut self, e: &Expr, vis: &Scope, ctes: &CteCols) {
        match e {
            Expr::ColumnRef(n) | Expr::ColumnRefOuterJoin(n) => self.column_ref(n, vis),
            Expr::Literal(_)
            | Expr::QualifiedStar(_)
            | Expr::Parameter(_)
            | Expr::MyBatisParam(_)
            | Expr::MyBatisRawExpr(_)
            | Expr::JdbcParam
            | Expr::Default
            | Expr::SysDate
            | Expr::CurrentOf { .. }
            | Expr::PlVariable(_)
            | Expr::SequenceValue { .. } => {}
            Expr::BinaryOp { left, right, .. } => {
                self.expr(left, vis, ctes);
                self.expr(right, vis, ctes);
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                self.expr(expr, vis, ctes);
                self.expr(pattern, vis, ctes);
                if let Some(x) = escape {
                    self.expr(x, vis, ctes);
                }
            }
            Expr::UnaryOp { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::IsBoolean { expr, .. }
            | Expr::Treat { expr, .. }
            | Expr::CollationFor { expr }
            | Expr::Prior(expr)
            | Expr::Parenthesized(expr) => self.expr(expr, vis, ctes),
            Expr::FunctionCall {
                args,
                over,
                filter,
                within_group,
                separator,
                default,
                conversion_format,
                ..
            } => {
                for a in args {
                    self.expr(a, vis, ctes);
                }
                if let Some(w) = over {
                    self.window_spec(w, vis, ctes);
                }
                if let Some(f) = filter {
                    self.expr(f, vis, ctes);
                }
                for o in within_group {
                    self.order_item(o, vis, ctes);
                }
                for x in [separator, default, conversion_format]
                    .into_iter()
                    .flatten()
                {
                    self.expr(x, vis, ctes);
                }
                // agg_from 表函数引用 V1 不判（列集合不可静态确定）
            }
            Expr::SpecialFunction { args, .. } => {
                for a in args {
                    self.expr(a, vis, ctes);
                }
            }
            Expr::Case {
                operand,
                whens,
                else_expr,
            } => {
                if let Some(o) = operand {
                    self.expr(o, vis, ctes);
                }
                for w in whens {
                    self.expr(&w.condition, vis, ctes);
                    self.expr(&w.result, vis, ctes);
                }
                if let Some(x) = else_expr {
                    self.expr(x, vis, ctes);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.expr(expr, vis, ctes);
                self.expr(low, vis, ctes);
                self.expr(high, vis, ctes);
            }
            Expr::InList { expr, list, .. } => {
                self.expr(expr, vis, ctes);
                for i in list {
                    self.expr(i, vis, ctes);
                }
            }
            Expr::InSubquery { expr, subquery, .. } => {
                self.expr(expr, vis, ctes);
                self.select(subquery, vis, ctes);
            }
            Expr::Exists(sub) | Expr::Subquery(sub) => {
                self.select(sub, vis, ctes);
            }
            Expr::ScalarSublink { expr, subquery, .. } => {
                self.expr(expr, vis, ctes);
                self.select(subquery, vis, ctes);
            }
            Expr::TypeCast {
                expr,
                default,
                format,
                ..
            } => {
                self.expr(expr, vis, ctes);
                if let Some(d) = default {
                    self.expr(d, vis, ctes);
                }
                if let Some(f) = format {
                    self.expr(f, vis, ctes);
                }
            }
            Expr::Array(items) | Expr::RowConstructor(items) => {
                for i in items {
                    self.expr(i, vis, ctes);
                }
            }
            Expr::Subscript {
                object,
                lower,
                upper,
                ..
            } => {
                self.expr(object, vis, ctes);
                if let Some(l) = lower {
                    self.expr(l, vis, ctes);
                }
                if let Some(u) = upper {
                    self.expr(u, vis, ctes);
                }
            }
            Expr::FieldAccess { object, .. } => self.expr(object, vis, ctes),
            Expr::XmlElement {
                evalname,
                attributes,
                content,
                ..
            } => {
                if let Some(x) = evalname {
                    self.expr(x, vis, ctes);
                }
                if let Some(a) = attributes {
                    for i in &a.items {
                        self.expr(&i.value, vis, ctes);
                    }
                }
                for c in content {
                    self.expr(&c.expr, vis, ctes);
                }
            }
            Expr::XmlConcat(items) => {
                for i in items {
                    self.expr(i, vis, ctes);
                }
            }
            Expr::XmlForest(items) => {
                for i in items {
                    self.expr(&i.expr, vis, ctes);
                }
            }
            Expr::XmlParse { expr, .. } | Expr::XmlSerialize { expr, .. } => {
                self.expr(expr, vis, ctes);
            }
            Expr::XmlPi { content, .. } => {
                if let Some(c) = content {
                    self.expr(c, vis, ctes);
                }
            }
            Expr::XmlRoot { expr, version, .. } => {
                self.expr(expr, vis, ctes);
                if let Some(v) = version {
                    self.expr(v, vis, ctes);
                }
            }
            Expr::AtTimeZone { expr, zone } => {
                self.expr(expr, vis, ctes);
                self.expr(zone, vis, ctes);
            }
            Expr::PredictBy { features, .. } => {
                for f in features {
                    self.expr(f, vis, ctes);
                }
            }
            Expr::CursorAttribute { cursor, .. } => self.expr(cursor, vis, ctes),
        }
    }
}
