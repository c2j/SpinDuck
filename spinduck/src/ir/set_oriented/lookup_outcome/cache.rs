//! 区域级 lookup outcome 缓存执行器（issue #65）。
//!
//! 包在 `SqlExecutor` 外层：`query` 按 IR SQL 原文匹配站点，提取
//! **全部**绑定值（行变键 + 不变量）构成探测键；首个探测触发一次
//! `GROUP BY` 聚合预计算（规格见 `site::build_spec`），此后
//! `Missing/Duplicate/ExactValue` 直接合成与原路径一致的结果或
//! `NO_DATA_FOUND`/`TOO_MANY_ROWS` 错误。`execute` 按表名使站点
//! 规格作废——下次探测按当前 schema 重证（B3），回滚全清。
//! 缓存只服务本次回退 `RunSpan`，句柄随调用结束丢弃。
//!
//! 审核修复口径：
//! - B1/R1：残余证明已收紧为 conversion-total，构建错误致事务
//!   aborted 属不可达状态——按 ENG 不变量 fail-loud，禁止用
//!   ROLLBACK 回滚用户未提交写入做缓存降级。
//! - B2：探测键 = 键占位符 + 不变量占位符的全部绑定值；不变量值与
//!   构建值不一致（嵌套例程同文本不同实参）→ bypass 原路径。
//! - B3：写/DDL 命中监视表 → `spec` 置空待重证，重建走
//!   `site::analyze_one` 当前 schema 全证据链，旧 proof 不复用。
//! - R4：SET/RESET/PRAGMA 会话态变更与回滚同口径全失效重证；
//!   预计算 SQL 下推 `LIMIT MAX_KEYS+1` 约束物化上界。

use super::site;
use super::{residual, SiteSpec, MAX_KEYS};
use crate::error::EngineError;
use crate::exec::{is_rollback_stmt, BindMap, ExecOutcome, RowStream, SqlExecutor};
use crate::ir::set_oriented::region::CursorRegion;
use crate::ir::{BindSrc, IrProg};
use crate::lookup_cache::{mentioned, KeyPart};
use crate::value::SpinValue;
use std::cell::RefCell;
use std::collections::HashMap;

/// 聚合预计算行数上限（防止无界物化；超限禁用该站点走原路径）。
const MAX_BUILD_ROWS: usize = 1_000_000;
/// 预计算行拉取分块。
const FETCH_CHUNK: usize = 8192;

/// 回退 `RunSpan` 的缓存执行器：站点索引懒构建，写/回滚即失效重证。
pub(crate) struct FallbackExecutor<'a> {
    inner: &'a dyn SqlExecutor,
    prog: &'a IrProg,
    region: &'a CursorRegion,
    /// 本执行器所属例程名（R3 站点身份一等键）：仅当
    /// `current_scope()` 栈顶与之相等时才参与站点匹配——
    /// 嵌套例程复用本执行器但作用域不同，一律 bypass。
    owner: String,
    sites: Vec<SiteSlot>,
}

struct SiteSlot {
    /// IR SQL 原文（站点匹配键）。
    sql: String,
    /// 站点身份（例程名@PC 列表）：写失效重证的 scope 输入。
    scope: String,
    /// 站点分析输入（重证用）：`(占位符, BindSrc)` 原表。
    binds: Vec<(String, BindSrc)>,
    /// 失效监视表集合（小写；规格重建后刷新）。
    watch: Vec<String>,
    cell: RefCell<SlotCell>,
}

struct SlotCell {
    /// 当前已证规格；`None` = 待重证（初始不可能——wrap 只收已证
    /// 站点）或重证失败。
    spec: Option<SiteSpec>,
    /// `spec` 为 `None` 时：`false` = 待重证，`true` = 本轮重证失败
    /// （下次写失效后允许再次重证——DDL 可能让形态重新可证）。
    proven: bool,
    state: SlotState,
}

enum SlotState {
    /// 尚未有探测行触发预计算。
    Pending,
    /// 预计算完成：键 → outcome（`Exact` 携带投影行，缺省键 = Missing）；
    /// `inv` = 构建时的不变量绑定值（探测不一致 → bypass，B2）。
    Ready {
        outcomes: HashMap<Vec<KeyPart>, SiteOutcome>,
        inv: Vec<KeyPart>,
    },
    /// 预计算失败/超限/类型复核不通过/重证不可证：恒走原路径。
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
    ex: &'a dyn SqlExecutor,
    prog: &'a IrProg,
    region: &'a CursorRegion,
    specs: Vec<SiteSpec>,
    routine: &str,
) -> Option<FallbackExecutor<'a>> {
    if specs.is_empty() {
        return None;
    }
    Some(FallbackExecutor {
        inner: ex,
        prog,
        region,
        owner: routine.to_string(),
        sites: specs
            .into_iter()
            .map(|spec| {
                let sql = spec.sql.clone();
                let scope = spec.scope.clone();
                let binds = spec.binds.clone();
                let watch = spec.watch.clone();
                SiteSlot {
                    sql,
                    scope,
                    binds,
                    watch,
                    cell: RefCell::new(SlotCell {
                        spec: Some(spec),
                        proven: true,
                        state: SlotState::Pending,
                    }),
                }
            })
            .collect(),
    })
}

impl SqlExecutor for FallbackExecutor<'_> {
    fn query(&self, sql: &str, binds: &BindMap) -> Result<RowStream, EngineError> {
        // R3：例程身份是匹配一等键——嵌套例程经 run_with_catalog 复用
        // 本执行器，栈顶例程名与 owner 不一致（或无例程上下文）一律
        // bypass，绝不按 SQL 文本跨例程命中。
        if super::current_scope().as_deref() != Some(self.owner.as_str()) {
            return self.inner.query(sql, binds);
        }
        let Some(slot) = self.sites.iter().find(|s| s.sql == sql) else {
            return self.inner.query(sql, binds);
        };
        self.ensure_spec(slot);
        // 探测期类型相容复核（R2/R3）：键与残余 `$v_` 绑定值的
        // SpinValue 变体+值域必须与列 kind 相容——表达式游标键、
        // 嵌套例程不同键表示等一律 bypass 原路径，绝不合成 Missing。
        let typed_ok = {
            let cell = slot.cell.borrow();
            cell.spec.as_ref().is_some_and(|spec| {
                spec.typed_binds
                    .iter()
                    .all(|(ph, kind)| binds.get(ph).is_none_or(|v| residual::runtime_ok(v, kind)))
            })
        };
        if !typed_ok {
            return self.inner.query(sql, binds);
        }
        let probe = {
            let cell = slot.cell.borrow();
            let Some(spec) = cell.spec.as_ref() else {
                return self.inner.query(sql, binds);
            };
            probe_key(spec, binds)
        };
        let Some((key, inv)) = probe else {
            return self.inner.query(sql, binds);
        };
        self.ensure_built(slot, binds, inv.clone())?;
        let cell = slot.cell.borrow();
        let Some(spec) = cell.spec.as_ref() else {
            return self.inner.query(sql, binds);
        };
        match &cell.state {
            SlotState::Ready {
                outcomes,
                inv: built,
            } if *built == inv => match outcomes.get(&key) {
                Some(SiteOutcome::Exact(row)) => {
                    Ok(RowStream::new(spec.schema.clone(), vec![row.clone()]))
                }
                Some(SiteOutcome::Duplicate) => Err(EngineError::too_many_rows()),
                None => Err(EngineError::no_data_found()),
            },
            // Disabled / 重证不可证 / 不变量值不一致（嵌套例程）→ 原路径。
            _ => self.inner.query(sql, binds),
        }
    }

    fn execute(&self, sql: &str, binds: &BindMap) -> Result<ExecOutcome, EngineError> {
        self.invalidate(sql);
        self.inner.execute(sql, binds)
    }
}

impl FallbackExecutor<'_> {
    /// 写失效后的规格重证：`spec=None` 且未证失败 → 按当前 schema
    /// 重走 `site::analyze_one` 全证据链；不可证 → `proven=true`
    /// 本轮禁用（下次失效后允许再次重证）。
    fn ensure_spec(&self, slot: &SiteSlot) {
        let stale = {
            let cell = slot.cell.borrow();
            cell.spec.is_none() && !cell.proven
        };
        if !stale {
            return;
        }
        let spec = site::analyze_one(
            self.prog,
            self.region,
            &slot.sql,
            &slot.binds,
            self.inner,
            &slot.scope,
        );
        let mut cell = slot.cell.borrow_mut();
        match spec {
            Some(spec) => {
                cell.spec = Some(spec);
                cell.state = SlotState::Pending;
            }
            None => {
                cell.proven = true;
                cell.state = SlotState::Disabled;
            }
        }
    }

    /// 首个探测触发聚合预计算；类型复核/超限失败一律 `Disabled`
    /// （原路径接管）；构建 SQL 执行期错误致事务 aborted 时按 ENG
    /// 不变量 fail-loud（R1：禁止用 ROLLBACK 回滚用户未提交写入）。
    fn ensure_built(
        &self,
        slot: &SiteSlot,
        binds: &BindMap,
        inv: Vec<KeyPart>,
    ) -> Result<(), EngineError> {
        {
            let cell = slot.cell.borrow();
            if !matches!(cell.state, SlotState::Pending) {
                return Ok(());
            }
        }
        let spec = {
            let cell = slot.cell.borrow();
            cell.spec.clone()
        };
        let Some(spec) = spec else {
            return Ok(());
        };
        let state = self.build(&spec, binds, inv)?;
        let mut cell = slot.cell.borrow_mut();
        if matches!(cell.state, SlotState::Pending) {
            cell.state = state;
        }
        Ok(())
    }

    /// 聚合预计算：`GROUP BY` 键列一次扫表，逐键产出
    /// `Missing`（无组）/`Duplicate`（COUNT>1）/`ExactValue`（COUNT=1 投影）。
    /// 构建 SQL 报错分两类：prepare/binder 期错误事务仍健康 → 安全
    /// `Disabled` 降级；执行期错误事务已 aborted → 按 ENG 不变量
    /// fail-loud（R1：残余证明已收紧为 conversion-total，此路径
    /// 构造上不可达；若未来实现回归也不得静默回滚用户写入）。
    fn build(
        &self,
        spec: &SiteSpec,
        binds: &BindMap,
        inv: Vec<KeyPart>,
    ) -> Result<SlotState, EngineError> {
        // 运行期类型复核（B1/R1）：键与残余 `$v_` 绑定值的 SpinValue
        // 变体+值域必须与列 kind 相容——不相容禁用且**不发 SQL**。
        for (ph, kind) in &spec.typed_binds {
            let ok = binds.get(ph).is_some_and(|v| residual::runtime_ok(v, kind));
            if !ok {
                return Ok(SlotState::Disabled);
            }
        }
        let Some(subset) = bind_subset(binds, &spec.inv_phs) else {
            return Ok(SlotState::Disabled);
        };
        let mut stream = match self.inner.query(&spec.build_sql, &subset) {
            Ok(s) => s,
            Err(_) => {
                if self.txn_aborted() {
                    return Err(EngineError::invariant(
                        74,
                        "lookup outcome build failed inside aborted transaction",
                    ));
                }
                return Ok(SlotState::Disabled);
            }
        };
        let dims = spec.key_phs.len();
        let projs = spec.schema.len();
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
                return Ok(SlotState::Disabled);
            }
        }
        if outcomes.len() > MAX_KEYS {
            return Ok(SlotState::Disabled);
        }
        Ok(SlotState::Ready { outcomes, inv })
    }

    /// 事务健康度探测：`SELECT 1` 失败 = 事务已 aborted（构建错误
    /// 发生在执行期）；成功 = prepare/binder 期失败，事务仍健康。
    fn txn_aborted(&self) -> bool {
        self.inner.query("SELECT 1", &BindMap::new()).is_err()
    }

    /// 写/回滚失效：命中监视表（或回滚/无法定名）→ 站点规格置空
    /// 待重证（B3：旧 schema/type proof 一律不复用）；`Pending`/
    /// `Ready`/`Disabled` 一视同仁——重证成功与否由 `analyze_one`
    /// 按当前 schema 判定。
    fn invalidate(&self, sql: &str) {
        if self.sites.iter().all(|s| {
            let cell = s.cell.borrow();
            cell.spec.is_none() && cell.proven
        }) {
            return;
        }
        // R4-1：SET/RESET/PRAGMA 会话态变更（如 default_collation）
        // 与回滚同口径全失效——下次探测重证时 collation 复核把关。
        let names = if is_rollback_stmt(sql) || is_setting_stmt(sql) {
            None
        } else {
            mentioned(sql)
        };
        for slot in &self.sites {
            let hit = match &names {
                // 回滚或分词失败：保守全失效（同 lookup_cache 口径）。
                None => true,
                Some(ns) => {
                    let cell = slot.cell.borrow();
                    cell.spec
                        .as_ref()
                        .is_some_and(|s| s.watch.iter().any(|t| ns.contains(t)))
                        || slot.watch.iter().any(|t| ns.contains(t))
                }
            };
            if hit {
                let mut cell = slot.cell.borrow_mut();
                cell.spec = None;
                cell.proven = false;
                cell.state = SlotState::Pending;
            }
        }
    }
}

/// 会话态变更判定（R4-1）：前导注释剥离后首关键字为
/// `SET`/`RESET`/`PRAGMA`——可能改变 `default_collation` 等
/// 排序规则会话态，站点规格必须作废重证（重证含 collation 复核）。
fn is_setting_stmt(sql: &str) -> bool {
    crate::exec::strip_leading_comments(sql)
        .split_whitespace()
        .next()
        .is_some_and(|w| {
            w.eq_ignore_ascii_case("set")
                || w.eq_ignore_ascii_case("reset")
                || w.eq_ignore_ascii_case("pragma")
        })
}

/// 探测键：全部绑定值 = 键占位符 + 不变量占位符（B2）；返回
/// `(键部, 不变量部)`。任一缺失/NULL/不可表键化 → bypass 原路径。
fn probe_key(spec: &SiteSpec, binds: &BindMap) -> Option<(Vec<KeyPart>, Vec<KeyPart>)> {
    let mut key = Vec::with_capacity(spec.key_phs.len());
    for ph in &spec.key_phs {
        let part = KeyPart::from_value(binds.get(ph)?)?;
        if matches!(part, KeyPart::Null) {
            return None;
        }
        key.push(part);
    }
    let mut inv = Vec::with_capacity(spec.inv_phs.len());
    for ph in &spec.inv_phs {
        let part = KeyPart::from_value(binds.get(ph)?)?;
        if matches!(part, KeyPart::Null) {
            return None;
        }
        inv.push(part);
    }
    Some((key, inv))
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
