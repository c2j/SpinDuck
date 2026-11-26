//! 站点准入证明（issue #65）：SELECT 形态修饰符、WHERE 合取项切分
//! 与等值键识别、会话/DDL 排序规则安全。
//!
//! 这些检查独立于 `site` 的类型证据链，但同属"不可证即禁用"的准入
//! 门槛：`shape::select` 不识别 DISTINCT/ALL（R4-2）、非二元排序
//! 规则下 GROUP BY 键折叠与探测 map 精确匹配口径不一致（R4-1）。

use super::super::shape::{self, Name, Select};
use super::site::is_ident;
use crate::exec::SqlExecutor;
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{BindSrc, VarName};
use std::collections::HashMap;

/// `SELECT DISTINCT/ALL` 修饰符判定（R4-2）：词法首词 SELECT、
/// 次词 distinct/all 即拒绝；词法化失败同样拒绝（保守）。
pub(super) fn select_has_modifier(sql: &str) -> bool {
    let Some(t) = shape::Tokens::new(sql) else {
        return true;
    };
    let w = t.words();
    w.first().is_some_and(|x| x.eq_ignore_ascii_case("select"))
        && w.get(1)
            .is_some_and(|x| x.eq_ignore_ascii_case("distinct") || x.eq_ignore_ascii_case("all"))
}

/// 排序规则安全证明（R4-1）：会话 `default_collation` 必须为二元
/// 安全值（空=默认/C/POSIX/BINARY），且维度表/游标源表 DDL 不得含
/// COLLATE——非二元排序下 GROUP BY 键折叠与探测 map 精确匹配
/// 口径不一致，缓存会合成错误 Missing。任一查询失败按不可证禁用。
pub(super) fn collation_safe(ex: &dyn SqlExecutor, tables: &[&Name]) -> bool {
    default_collation_binary(ex) && tables.iter().all(|t| !table_ddl_has_collate(ex, t))
}

/// 会话 `default_collation` 是否二元安全；查询失败/非字符串 → false。
/// 注意必须走 `duckdb_settings()` 表函数：`current_setting()` 会被
/// `prepare_cached` 语句缓存在 prepare 期常量折叠，SET 后仍返回旧值。
fn default_collation_binary(ex: &dyn SqlExecutor) -> bool {
    let Ok(mut s) = ex.query(
        "SELECT value FROM duckdb_settings() WHERE name = 'default_collation'",
        &crate::exec::BindMap::new(),
    ) else {
        return false;
    };
    let rows = s.fetch_many(1);
    let Some(crate::value::SpinValue::Str(v)) = rows.first().and_then(|r| r.first()) else {
        return false;
    };
    matches!(v.to_ascii_lowercase().trim(), "" | "c" | "posix" | "binary")
}

/// 表 DDL 是否含 COLLATE（列级排序规则独立于会话设置）；
/// `duckdb_tables().sql` 为建表原文，子串命中即保守判不安全。
fn table_ddl_has_collate(ex: &dyn SqlExecutor, table: &Name) -> bool {
    let binds = crate::exec::BindMap::from([(
        "$t".to_string(),
        crate::value::SpinValue::Str(table.0.clone()),
    )]);
    let Ok(mut s) = ex.query(
        "SELECT sql FROM duckdb_tables() WHERE lower(table_name) = lower($t)",
        &binds,
    ) else {
        return true;
    };
    s.fetch_many(64).iter().any(|row| {
        matches!(
            row.first(),
            Some(crate::value::SpinValue::Str(sql))
                if sql.to_ascii_lowercase().contains("collate")
        )
    })
}

/// WHERE 顶层合取项切分：`BETWEEN a AND b` 的 AND 不属于合取边界。
pub(super) fn split_conjuncts(filter: &str) -> Vec<Vec<String>> {
    let Some(t) = shape::Tokens::new(filter) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut between_open = false;
    for w in t.words() {
        match w.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "between" if depth == 0 => between_open = true,
            "and" if depth == 0 => {
                if between_open {
                    between_open = false;
                } else {
                    out.push(std::mem::take(&mut cur));
                    continue;
                }
            }
            _ => {}
        }
        cur.push(w.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 等值合取项 → (维度列, 占位符, 记录变量, 字段名)；非键形态返回 None。
/// 限定符必须是表名或别名，否则原查询本身不可证，按非键处理（残余侧
/// 会因字段绑定拒绝整站点）。
pub(super) fn key_conjunct(
    conj: &[String],
    bind_map: &HashMap<&str, &BindSrc>,
    region: &CursorRegion,
    sel: &Select,
) -> Option<(Name, String, VarName, String)> {
    let (col, ph, qual) = match conj {
        [c, eq, p] if eq == "=" => (c, p, None),
        [q, dot, c, eq, p] if dot == "." && eq == "=" => (c, p, Some(q)),
        [p, eq, q, dot, c] if eq == "=" && dot == "." => (c, p, Some(q)),
        _ => return None,
    };
    let (col, ph) = if bind_map.contains_key(ph.as_str()) && is_ident(col) {
        (col, ph)
    } else if bind_map.contains_key(col.as_str()) && is_ident(ph) {
        (ph, col)
    } else {
        return None;
    };
    if let Some(q) = qual {
        let ok = q == &sel.table.0 || sel.alias.as_ref().is_some_and(|a| &a.0 == q);
        if !ok {
            return None;
        }
    }
    match bind_map.get(ph.as_str()) {
        Some(BindSrc::Field { rec, field }) if rec == &region.record => Some((
            Name(col.clone()),
            ph.clone(),
            rec.clone(),
            field.to_ascii_lowercase(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        split_conjuncts(s)
            .into_iter()
            .map(|c| c.join(" "))
            .collect()
    }

    #[test]
    fn should_split_top_level_and_but_keep_between_range() {
        assert_eq!(
            words("a = 1 and pri between 1 and 9 and b > 2"),
            vec!["a = 1", "pri between 1 and 9", "b > 2"]
        );
    }

    #[test]
    fn should_keep_or_and_parens_inside_single_conjunct() {
        assert_eq!(
            words("(a = 1 or a = 2) and b = 3"),
            vec!["( a = 1 or a = 2 )", "b = 3"]
        );
    }

    #[test]
    fn should_handle_not_between_and_multiple_ranges() {
        assert_eq!(
            words("x not between 1 and 2 and y between 3 and 4"),
            vec!["x not between 1 and 2", "y between 3 and 4"]
        );
    }

    #[test]
    fn should_classify_field_equality_as_key_and_var_equality_as_residual() {
        let record = VarName("r".into());
        let sel = Select {
            table: Name("t".into()),
            alias: None,
            columns: vec![Name("k".into())],
            sources: vec![Name("k".into())],
            filter: None,
            verbatim: None,
        };
        let binds = [
            (
                "$v_r__a".to_string(),
                BindSrc::Field {
                    rec: record.clone(),
                    field: "a".into(),
                },
            ),
            ("$v_x".to_string(), BindSrc::Var(VarName("x".into()))),
        ];
        let bind_map: HashMap<&str, &BindSrc> =
            binds.iter().map(|(p, s)| (p.as_str(), s)).collect();
        let region = CursorRegion {
            open_pc: 0,
            fetch_pc: 0,
            close_pc: 0,
            cursor: VarName("c".into()),
            record: record.clone(),
        };
        // 字段等值 → 键
        let hit = key_conjunct(
            &["a".into(), "=".into(), "$v_r__a".into()],
            &bind_map,
            &region,
            &sel,
        );
        assert!(hit.is_some(), "field equality should be a key");
        // 变量等值 → 非键（残余侧按不变量谓词处理）
        let miss = key_conjunct(
            &["a".into(), "=".into(), "$v_x".into()],
            &bind_map,
            &region,
            &sel,
        );
        assert!(miss.is_none(), "var equality is not a row-variant key");
        // 反向书写同样识别
        let rev = key_conjunct(
            &["$v_r__a".into(), "=".into(), "a".into()],
            &bind_map,
            &region,
            &sel,
        );
        assert!(rev.is_some(), "reversed equality should be a key");
    }
}
