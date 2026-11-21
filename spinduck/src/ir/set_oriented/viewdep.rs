//! 视图查找物化前导：普通查找落在视图上时无法走 prove_view 全定义
//! 证明（视图含 UNION/表达式/子查询），改为区域入口物化
//! `CREATE OR REPLACE TEMP TABLE sd_vwN AS SELECT * FROM <视图>`，
//! 批写/守卫中的查找派生表改引用临时表。物化与原解释器逐行求值
//! 视图等价的前提：
//! - 确定性：视图 SQL 不得含易变/非常量函数（内置清单 + 目录例程
//!   须摘要 PureExpr）；now()/current_* 在 DuckDB 为事务时间戳，
//!   物化一次与逐行求值同值。
//! - 基数：UNION ALL 等产形态的重复键由 grouped() 的 sd_cnt=1 命中
//!   判定落兜底，与原 too_many_rows→WHEN OTHERS 一致。
//! - 依赖：底表经 token 扫描 + 对象名清单递归提取（列名/别名同名
//!   只会过近似多登记，安全方向），全部登记漂移复核与写集相交。
//!
//! 物化失败不在本层处理：调用方回退原区域，解释器逐行求值复现
//! 行级 trap 兜底。

use std::collections::{HashMap, HashSet};

use super::catalog_summary::{self, Summary};
use super::entry::ViewPrelude;
use super::schema::{Source, Table};
use super::shape::{Name, Tokens};
use super::SkipReason;
use crate::catalog::RoutineCatalog;
use crate::error::EngineError;

/// 内置易变/非常量函数与非确定窗口/抽样函数（小写）：物化一次与
/// 逐行求值可观测不同 → 保守拒绝。事务时间戳族（now/current_*）
/// 不在列——DuckDB 中其值在事务内恒定。
const VOLATILE_FNS: &[&str] = &[
    "random",
    "setseed",
    "uuid",
    "gen_random_uuid",
    "nextval",
    "currval",
    "setval",
    "lastval",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    "reservoir",
    "bernoulli",
];

/// 视图物化产物：sd_vwN 证据表（渲染/类型证明用）+ 前导契约 +
/// 底表证据（漂移复核与写集相交对象）。
pub(super) struct ViewDep {
    /// sd_vwN 临时表证据：qualified=临时表名，columns=DESCRIBE 探测。
    pub table: Table,
    pub prelude: ViewPrelude,
    /// 底表 (名, 结构证据)：入口漂移复核对象（视图名本身不可 load）。
    pub bases: Vec<(Name, Table)>,
    /// 底表名：写集相交复核对象。
    pub base_names: Vec<Name>,
}

/// 视图物化准入：`view` 非视图 = Ok(None)（交回调用方按缺失处理）；
/// 确定性/探测/依赖任一不可证 = Err(SkipReason)。
pub(super) fn materialize(
    ex: &Source<'_>,
    catalog: &RoutineCatalog,
    view: &Name,
    index: usize,
) -> Result<Result<Option<ViewDep>, SkipReason>, EngineError> {
    let Some(sql) = ex.view_sql(view)? else {
        return Ok(Ok(None));
    };
    if let Err(reason) = deterministic(&sql, catalog) {
        return Ok(Err(reason));
    }
    let Some(columns) = ex.probe_columns(&format!("SELECT * FROM {}", view.sql()))? else {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    };
    let Some((tables, views)) = ex.object_names()? else {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    };
    let mut visited = HashSet::from([view.0.clone()]);
    let mut bases: Vec<(Name, Table)> = Vec::new();
    let mut base_names: Vec<Name> = Vec::new();
    if let Err(reason) =
        collect_bases(ex, catalog, &sql, &tables, &views, &mut visited, &mut bases)?
    {
        return Ok(Err(reason));
    }
    for (name, _) in &bases {
        if !base_names.contains(name) {
            base_names.push(name.clone());
        }
    }
    let table_name = format!("sd_vw{index}");
    Ok(Ok(Some(ViewDep {
        table: Table {
            qualified: table_name.clone(),
            columns,
        },
        prelude: ViewPrelude {
            sql: format!(
                "CREATE OR REPLACE TEMP TABLE {table_name} AS SELECT * FROM {}",
                view.sql()
            ),
        },
        bases,
        base_names,
    })))
}

/// 确定性证明：token 级 `名 (` 调用点扫描——内置易变清单直接拒绝；
/// 目录命中的例程须摘要 PureExpr（PL 纯函数展开证明复用）；未登记
/// 名按内建处理。无法词法化 = 不可证。
fn deterministic(sql: &str, catalog: &RoutineCatalog) -> Result<(), SkipReason> {
    let bad = SkipReason::UnprovenQueryTotalness;
    let Some(t) = Tokens::new(sql) else {
        return Err(bad);
    };
    let words = t.words();
    for pair in words.windows(2) {
        if pair[1] != "(" {
            continue;
        }
        let name = pair[0].as_str();
        if name.starts_with('"') || name.starts_with('\'') {
            continue;
        }
        if VOLATILE_FNS.contains(&name) {
            return Err(bad);
        }
        if catalog.lookup(name).is_some()
            && !matches!(
                catalog_summary::summarize(catalog, name),
                Summary::PureExpr(_)
            )
        {
            return Err(bad);
        }
    }
    Ok(())
}

/// 依赖提取：视图 SQL 全部标识符 token 对象名清单求交——命中普通表
/// 递归取结构证据（load 失败 = 依赖不可证），命中视图递归扫描其定义
/// （visited 防环，深度由集合规模自然封顶）。列名/字符串/数字天然
/// 不命中对象名；同名误中只会多登记底表（过近似安全方向）。
fn collect_bases(
    ex: &Source<'_>,
    catalog: &RoutineCatalog,
    sql: &str,
    tables: &HashSet<String>,
    views: &HashMap<String, String>,
    visited: &mut HashSet<String>,
    bases: &mut Vec<(Name, Table)>,
) -> Result<Result<(), SkipReason>, EngineError> {
    let Some(t) = Tokens::new(sql) else {
        return Ok(Err(SkipReason::UnprovenQueryTotalness));
    };
    for word in t.words() {
        let Some(candidate) = ident(word) else {
            continue;
        };
        if tables.contains(&candidate) {
            let name = Name(candidate.clone());
            if bases.iter().any(|(n, _)| *n == name) {
                continue;
            }
            match ex.load(&name)? {
                Some(table) => bases.push((name, table)),
                None => return Ok(Err(SkipReason::UnprovenQueryTotalness)),
            }
        } else if views.contains_key(&candidate) && visited.insert(candidate.clone()) {
            let Some(nested) = views.get(&candidate) else {
                continue;
            };
            // 依赖链每一层都要过确定性证明：nextval/random 等易变调用
            // 可藏在内层视图里，根视图 deterministic 只扫自身文本。
            // 物化会按行消费序列/熵，多次物化（探测+入口）结果漂移。
            if let Err(reason) = deterministic(nested, catalog) {
                return Ok(Err(reason));
            }
            match collect_bases(ex, catalog, nested, tables, views, visited, bases)? {
                Ok(()) => {}
                Err(reason) => return Ok(Err(reason)),
            }
        }
    }
    Ok(Ok(()))
}

/// token → 小写标识符候选：裸标识符与双引号标识符均可；字符串
/// 字面量/数字/符号天然不命中。
fn ident(word: &str) -> Option<String> {
    let unquoted = word
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .map(|s| s.replace("\"\"", "\""))
        .unwrap_or_else(|| word.to_string());
    let mut chars = unquoted.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(unquoted.to_ascii_lowercase())
}
