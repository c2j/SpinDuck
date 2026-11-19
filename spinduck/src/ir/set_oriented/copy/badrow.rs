//! §3.5-4 非 NoOp INSERT 处理器的坏行入口守卫：`bad_rows == 0` 准入。

use super::{shape, CopyLoop, Table};

/// `sd_cur` CTE 定义（批写主查询与坏行守卫共用，保证行集口径一致）。
pub(super) fn sd_cur_cte(select: &shape::Select, source: &Table) -> String {
    let where_clause = match &select.filter {
        Some(f) => format!(" WHERE {f}"),
        None => String::new(),
    };
    format!(
        "sd_cur AS ({}{where_clause})",
        super::sd_cur_scan(select, source)
    )
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
    let with = std::iter::once(sd_cur_cte(select, source))
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

/// §3.5-4 入口：`insert_log` 区域追加坏行守卫（parts = 写列守卫/CTE/join）。
pub(super) fn push_bad_row_guard(
    region: &CopyLoop<'_>,
    parts: (&[String], &[String], &[String]),
    select: &shape::Select,
    source: &Table,
    expr_guards: &mut Vec<String>,
) {
    if !region.insert_log {
        return;
    }
    bad_row_guard(parts.0, select, source, (parts.1, parts.2), expr_guards);
}
