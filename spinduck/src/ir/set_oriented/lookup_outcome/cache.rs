//! 区域级 lookup outcome 缓存执行器（issue #65）。
//!
//! 包在 `SqlExecutor` 外层：`query` 按 IR SQL 原文匹配站点，提取行变
//! 键绑定构成探测键；首个探测触发一次 `GROUP BY` 聚合预计算（规格见
//! `site::build_spec`），此后 `Missing/Duplicate/ExactValue` 直接合成
//! 与原路径一致的结果或 `NO_DATA_FOUND`/`TOO_MANY_ROWS` 错误。
//! `execute` 按表名失效、回滚全清——与 `lookup_cache` 同一失效口径。
//! 缓存只服务本次回退 `RunSpan`，句柄随调用结束丢弃。

use super::SiteSpec;
use crate::error::EngineError;
use crate::exec::{is_rollback_stmt, BindMap, ExecOutcome, RowStream, SqlExecutor};
use crate::lookup_cache::{mentioned, KeyPart};
use crate::value::SpinValue;
use std::cell::RefCell;
use std::collections::HashMap;

/// 聚合预计算行数上限（防止无界物化；超限禁用该站点走原路径）。
const MAX_BUILD_ROWS: usize = 1_000_000;
/// distinct 键上限（issue 建议值 200k）。
const MAX_KEYS: usize = 200_000;
/// 预计算行拉取分块。
const FETCH_CHUNK: usize = 8192;

/// 回退 `RunSpan` 的缓存执行器：站点索引懒构建，写/回滚即失效。
pub(crate) struct FallbackExecutor<'a> {
    inner: &'a dyn SqlExecutor,
    sites: Vec<SiteSlot>,
}

struct SiteSlot {
    spec: SiteSpec,
    state: RefCell<SlotState>,
}

enum SlotState {
    /// 尚未有探测行触发预计算。
    Pending,
    /// 预计算完成：键 → outcome（`Exact` 携带投影行，缺省键 = Missing）。
    Ready {
        outcomes: HashMap<Vec<KeyPart>, SiteOutcome>,
    },
    /// 预计算失败/超限/被写失效：恒走原路径。
    Disabled,
}

enum SiteOutcome {
    /// 恰一行：投影列值（与原行同序，`write_into` 逐列赋值）。
    Exact(Vec<SpinValue>),
    /// 多行：原路径 `fetch_many(2)` 判定 TOO_MANY_ROWS。
    Duplicate,
}

/// 由站点规格组装执行器；无可证站点返回 `None`。
pub(super) fn wrap<'a>(
    inner: &'a dyn SqlExecutor,
    specs: Vec<SiteSpec>,
) -> Option<FallbackExecutor<'a>> {
    if specs.is_empty() {
        return None;
    }
    Some(FallbackExecutor {
        inner,
        sites: specs
            .into_iter()
            .map(|spec| SiteSlot {
                spec,
                state: RefCell::new(SlotState::Pending),
            })
            .collect(),
    })
}

impl SqlExecutor for FallbackExecutor<'_> {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        let Some(site) = self.sites.iter().find(|s| s.spec.sql == sql) else {
            return self.inner.query(sql, binds);
        };
        let Some(key) = probe_key(&site.spec, binds) else {
            return self.inner.query(sql, binds);
        };
        self.ensure(site, binds);
        let state = site.state.borrow();
        match &*state {
            SlotState::Ready { outcomes } => match outcomes.get(&key) {
                Some(SiteOutcome::Exact(row)) => {
                    Ok(RowStream::new(site.spec.schema.clone(), vec![row.clone()]))
                }
                Some(SiteOutcome::Duplicate) => Err(EngineError::too_many_rows()),
                None => Err(EngineError::no_data_found()),
            },
            _ => self.inner.query(sql, binds),
        }
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.invalidate(sql);
        self.inner.execute(sql, binds)
    }
}

impl FallbackExecutor<'_> {
    /// 首个探测触发聚合预计算；失败/超限一律降级 `Disabled`（原路径接管，
    /// 预计算错误不额外改变语句成败序列——残余谓词已限定为无副作用形态）。
    fn ensure(&self, site: &SiteSlot, binds: &BindMap) {
        {
            let state = site.state.borrow();
            if !matches!(*state, SlotState::Pending) {
                return;
            }
        }
        *site.state.borrow_mut() = self.build(site, binds);
    }

    /// 聚合预计算：`GROUP BY` 键列一次扫表，逐键产出
    /// `Missing`（无组）/`Duplicate`（COUNT>1）/`ExactValue`（COUNT=1 投影）。
    fn build(&self, site: &SiteSlot, binds: &BindMap) -> SlotState {
        let Some(subset) = bind_subset(binds, &site.spec.inv_phs) else {
            return SlotState::Disabled;
        };
        let Ok(mut stream) = self.inner.query(&site.spec.build_sql, &subset) else {
            return SlotState::Disabled;
        };
        let dims = site.spec.key_phs.len();
        let projs = site.spec.schema.len();
        let width = dims + 1 + projs;
        let mut outcomes: HashMap<Vec<KeyPart>, SiteOutcome> = HashMap::new();
        let mut rows = 0usize;
        loop {
            let chunk = stream.fetch_many(FETCH_CHUNK);
            if chunk.is_empty() {
                break;
            }
            rows += chunk.len();
            if rows > MAX_BUILD_ROWS || !parse_chunk(&chunk, dims, width, &mut outcomes) {
                return SlotState::Disabled;
            }
        }
        if outcomes.len() > MAX_KEYS {
            return SlotState::Disabled;
        }
        SlotState::Ready { outcomes }
    }

    /// 写/回滚失效：`Ready` 索引回到 `Pending` 由下一探测重建（重建
    /// 看到失效后的最新事务态，与 `lookup_cache` 的丢索引重建同口径）；
    /// `Disabled` 永久降级不复活，`Pending` 原样保留。
    fn invalidate(&self, sql: &str) {
        if self
            .sites
            .iter()
            .all(|s| !matches!(*s.state.borrow(), SlotState::Ready { .. }))
        {
            return;
        }
        if is_rollback_stmt(sql) {
            for site in &self.sites {
                reset(site);
            }
            return;
        }
        match mentioned(sql) {
            Some(names) => {
                for site in &self.sites {
                    if names.contains(&site.spec.table) {
                        reset(site);
                    }
                }
            }
            // `mentioned` 分词失败：保守全失效（同 lookup_cache 口径）。
            None => {
                for site in &self.sites {
                    reset(site);
                }
            }
        }
    }
}

/// `Ready` → `Pending`：索引作废待重建；其余状态不动。
fn reset(site: &SiteSlot) {
    if matches!(*site.state.borrow(), SlotState::Ready { .. }) {
        *site.state.borrow_mut() = SlotState::Pending;
    }
}

/// 探测键：全部行变键绑定值；任一缺失/NULL/不可表键化 → bypass 原路径。
fn probe_key(spec: &SiteSpec, binds: &BindMap) -> Option<Vec<KeyPart>> {
    let mut key = Vec::with_capacity(spec.key_phs.len());
    for ph in &spec.key_phs {
        let part = KeyPart::from_value(binds.get(ph)?)?;
        if matches!(part, KeyPart::Null) {
            return None;
        }
        key.push(part);
    }
    Some(key)
}

/// 预计算行 → outcome 表项：行布局 `[键列×k, COUNT, 投影列×p]`。
/// 任一行形态不符 → 整表弃用（`false`）。
fn parse_chunk(
    rows: &[Vec<SpinValue>],
    dims: usize,
    width: usize,
    outcomes: &mut HashMap<Vec<KeyPart>, SiteOutcome>,
) -> bool {
    for row in rows {
        if row.len() != width {
            return false;
        }
        let mut key = Vec::with_capacity(dims);
        for v in &row[..dims] {
            match KeyPart::from_value(v) {
                Some(part) => key.push(part),
                None => return false,
            }
        }
        let SpinValue::Int(cnt) = row[dims] else {
            return false;
        };
        let proj = row[dims + 1..].to_vec();
        let outcome = if cnt == 1 {
            SiteOutcome::Exact(proj)
        } else {
            SiteOutcome::Duplicate
        };
        outcomes.insert(key, outcome);
    }
    true
}

/// build_sql 引用的不变量占位符子集；缺绑定 → 站点禁用。
fn bind_subset(binds: &BindMap, names: &[String]) -> Option<BindMap> {
    let mut out = BindMap::new();
    for name in names {
        out.insert(name.clone(), binds.get(name)?.clone());
    }
    Some(out)
}
