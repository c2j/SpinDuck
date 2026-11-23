//! §3.5-4 非 NoOp INSERT 处理器的坏行入口守卫：`bad_rows == 0` 准入。

use super::{shape, Table};

/// `sd_cur` CTE 定义（批写主查询与坏行守卫共用，保证行集口径一致）。
/// verbatim 游标原文已含 WHERE/GROUP BY，直接内嵌不再追加过滤。
/// `rn` = 混合调度：外包 `ROW_NUMBER()` 投影 0 基 `__sd_rn`（与游标流
/// 序同口径，依赖 R1 行序稳定假设），供段谓词过滤。
pub(super) fn sd_cur_cte(select: &shape::Select, source: &Table, rn: bool) -> String {
    let inner = if let Some(v) = &select.verbatim {
        v.clone()
    } else {
        let where_clause = match &select.filter {
            Some(f) => format!(" WHERE {f}"),
            None => String::new(),
        };
        format!("{}{where_clause}", super::sd_cur_scan(select, source))
    };
    if rn {
        format!("sd_cur AS (SELECT *, ROW_NUMBER() OVER () - 1 AS __sd_rn FROM ({inner}) _rn)")
    } else {
        format!("sd_cur AS ({inner})")
    }
}

/// 内核域坏行守卫：`COUNT(*)` 统计 DECIMAL 算术值域守卫不成立的行。
/// 守卫以 DOUBLE 域求值、永不抛错；必须先于 [`bad_row_guard`] 下发——
/// `entry_ok` 按序执行、首个非零即回退，保证写守卫 COUNT 不会对
/// 含溢出算术的行求值（那样守卫查询自身会内核报错）。
pub(super) fn kernel_row_guard(
    kernel_guards: &[String],
    select: &shape::Select,
    source: &Table,
    parts: (&[String], &[String]),
    expr_guards: &mut Vec<String>,
) {
    if kernel_guards.is_empty() {
        return;
    }
    let bad = kernel_guards
        .iter()
        .map(|g| format!("({g}) IS NOT TRUE"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let (ctes, joins) = parts;
    let with = std::iter::once(sd_cur_cte(select, source, false))
        .chain(ctes.iter().cloned())
        .collect::<Vec<_>>()
        .join(", ");
    let join_sql = if joins.is_empty() {
        String::new()
    } else {
        format!(" {}", joins.join(" "))
    };
    expr_guards.push(format!(
        "WITH {with} SELECT COUNT(*) FROM sd_cur{join_sql} WHERE {bad}"
    ));
}

/// 坏行守卫：`COUNT(*)` 统计写列守卫不成立（FALSE 或 NULL）的行。
/// 复用批写同一 `sd_cur`/CTE/join 口径；结果非 0 时 `entry_ok` 退回。
/// 守卫只覆盖本条 INSERT 显式写入列（默认值填充列不在内）。
pub(super) fn bad_row_guard(
    write_guards: &[String],
    select: &shape::Select,
    source: &Table,
    parts: (&[String], &[String]),
    expr_guards: &mut Vec<String>,
) {
    if write_guards.is_empty() {
        return;
    }
    let bad = write_guards
        .iter()
        .map(|g| format!("({g}) IS NOT TRUE"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let (ctes, joins) = parts;
    let with = std::iter::once(sd_cur_cte(select, source, false))
        .chain(ctes.iter().cloned())
        .collect::<Vec<_>>()
        .join(", ");
    let join_sql = if joins.is_empty() {
        String::new()
    } else {
        format!(" {}", joins.join(" "))
    };
    expr_guards.push(format!(
        "WITH {with} SELECT COUNT(*) FROM sd_cur{join_sql} WHERE {bad}"
    ));
}
