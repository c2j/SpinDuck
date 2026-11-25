//! §3.5-4 非 NoOp INSERT 处理器的坏行入口守卫：`bad_rows == 0` 准入。

use super::{shape, Table};

/// `sd_cur` 行集带 0 基 `__sd_rn` 的物化 SELECT（混合调度单次物化
/// 与 CTE 形态共用同一编号口径）。`ROW_NUMBER()` 行序由**全列规范化
/// `ORDER BY`** 决定：序由内容唯一决定——GROUP BY 游标的分组键是输出
/// 行唯一键即全序；普通游标整行重复互换不可观测。物化与重建
/// （`ensure_hcur` 回灌路径之外的入口物化）共用同一文本；投影列为空
/// 时退回无序形态（调用方 `hcur_order_ok` 仍以冻结行集兜底）。
pub(in crate::ir::set_oriented) fn sd_cur_rows(select: &shape::Select, source: &Table) -> String {
    let inner = if let Some(v) = &select.verbatim {
        v.clone()
    } else {
        let where_clause = match &select.filter {
            Some(f) => format!(" WHERE {f}"),
            None => String::new(),
        };
        format!("{}{where_clause}", super::sd_cur_scan(select, source))
    };
    let order = select
        .columns
        .iter()
        .map(|c| c.sql())
        .collect::<Vec<_>>()
        .join(", ");
    if order.is_empty() {
        return format!("SELECT *, ROW_NUMBER() OVER () - 1 AS __sd_rn FROM ({inner}) _rn");
    }
    format!("SELECT *, ROW_NUMBER() OVER () - 1 AS __sd_rn FROM ({inner}) _rn ORDER BY {order}")
}

/// `sd_cur` CTE 定义（批写主查询与坏行守卫共用，保证行集口径一致）。
/// verbatim 游标原文已含 WHERE/GROUP BY，直接内嵌不再追加过滤。
/// `rn` = 混合调度：`sd_cur` 改读引擎物化的会话临时表
/// （`__SD_HCUR__` 占位符由运行期替换为唯一表名）——`__sd_rn`
/// 只在物化时赋一次，守卫/段批写共用同一编号，消除多次扫描间
/// 的行序漂移（P0-3b）。
pub(super) fn sd_cur_cte(select: &shape::Select, source: &Table, rn: bool) -> String {
    if rn {
        return format!(
            "sd_cur AS (SELECT * FROM {})",
            crate::ir::set_oriented::SD_HCUR_TABLE
        );
    }
    let inner = if let Some(v) = &select.verbatim {
        v.clone()
    } else {
        let where_clause = match &select.filter {
            Some(f) => format!(" WHERE {f}"),
            None => String::new(),
        };
        format!("{}{where_clause}", super::sd_cur_scan(select, source))
    };
    format!("sd_cur AS ({inner})")
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
