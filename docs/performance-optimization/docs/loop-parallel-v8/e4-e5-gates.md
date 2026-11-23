# E4+E5 门禁区域化与 fn_site 快照化详设

> V8 机制 E4（门禁 5 审计范围区域化）+ E5（游标 fn_site 物化快照化）。byfund 段 1 的两个前置阻断。

## E4 门禁 5 区域化（Tx-Anchor Audit Scoping）

### 问题

`proof.rs scan_prog_tx` 遍历 `prog.code` **全例程**：byfund 段 2 `OPEN c_fxj FOR v_sql_txt`（动态源）→ `TransactionAnchoredRead`，污染全部 4 段——尽管目标区域 `c_get_data` 与该动态游标在控制流上不相交。

### 机制

审计范围从"全例程"收窄为 **目标区域 + 区域可达后缀 + 传递调用闭包**：

1. **区域内**：`[open_pc ..= close_pc]` 全部语句照旧审计；
2. **区域后缀：排除**（2026-XX 终审修订）：`close_pc` 之后语句在并行窗口关闭后原生执行，事务演化与串行逐语句一致——预热建索引仅只读 SELECT（`lookup_cache::build`），DuckDB 1.5.5 实证只读查询失败不污染事务（后续 SELECT/INSERT/COMMIT 全部正常），实现亦无 `reset_session`（`try_loop_parallel` 预热失败直接串行续跑）；`CallRoutine`/`SqlFn` 物化经 `callmat` 证明无事务操作。**不存在串行没有的额外事务切换 ⇒ 后缀事务锚定读/动态 SQL 不审计**。byfund 段 2 `OPEN c_fxj FOR` 即因此不再污染段 1；
3. **区域前缀**：`[0 .. open_pc]` 全量审计（保守——前缀在 OPEN 前已执行，实际亦可豁免，保留以维持审计面）；
4. **传递闭包**：区域内全部 `CallRoutine` 的 callee 体递归审计（现有 `scan_callee_tx` 逻辑不变）；
5. **排除**：不在上述集合内的语句（其他游标段、互不相交的分支）不参与判定。

**正确性论证（修订后）**：并行路径相对串行**不引入任何额外事务切换**——预热仅执行只读 SELECT（失败不污染事务，DuckDB 1.5.5 探针实证），预热失败直接串行续跑不复位，物化调用经 `callmat` 排除事务操作，区域内事务语句两模式逐语句一致。因此"事务锚定读随复位变化"的分歧场景不存在；保留前缀+区域+闭包审计仅为保守冗余（该范围内本就无可疑点时零成本）。

**反例守护（修订后口径）**：区域后缀含 `CURRENT_TIMESTAMP` / 动态 SQL → **放行**（后缀事务演化与串行一致）；区域/前缀内动态 SQL、`CURRENT_TIMESTAMP` → 仍拒；区域外互不相交段 → 放行。

## E5 fn_site 物化快照化

### 问题

`reject_cursor_fn_sites`：游标 `CursorSource::Static.fn_sites` 非空 → `MaterializedFnSitePresent`。byfund `c_get_data` 游标 WHERE 含 `fnc_get_fund_data_mode(...)` → OPEN 时 `materialize_fn_sites` 建 `sd_fn0` 临时表。实测该物化是**一次性 DISTINCT 预计算**（`distinct_sql` 仅绑 `p_i_date`），循环不变。

### 机制

fn_site 物化从"写临时表 → 拒"重新定性为"**入口期一次性物化 + Worker 只读快照**"，条件全部满足才放行：

1. **物化时机**：`materialize_fn_sites` 只在 `OPEN` 阶段执行（现状 `cursors.rs` 已是），发生在 `clean_at_prewarm` 校验**之前**——物化写属于前缀写，`dirty_tx_observed` 如实记录；若物化后存在未提交写 → `DirtyPrefixWithRollbackRisk` 维持拒绝（V7 口径不变）；
2. **循环不变性**：fn_site 的 `distinct_sql` 绑定全部循环不变（复用 `query_cacheable` 的 range_key 判定）——行相关 fn_site 仍拒；
3. **只读共享**：物化结果表（`sd_*`）在 `W_loop` 写集枚举中证明**循环内零写**（含传递闭包）；Worker 侧不再触碰物化表本身，而是把"对 `sd_fn0` 的查询"改写为对**冻结快照**的 probe——物化表内容在 OPEN 后冻结为 `FrozenSlot` 等价物；
4. **主线程仍执行物化**：`OPEN` 的 `materialize_fn_sites` 调用本身留在主线程原生路径（trace/步数/错误语义零变化），Worker 只消费其结果。

**等价性论证**：串行中 `sd_fn0` 在 OPEN 时建、循环内只读、CLOSE 后随例程结束；并行中物化时机、内容、可见性完全一致——唯一差异是 Worker 查的是快照副本而非活表，快照在 `rollback_invalidated` 时与活缓存一同作废（§5.2 口径扩展）。

## 改动面

- `ir/loop_parallel/proof.rs`：`scan_prog_tx` 范围参数化（region+suffix+closure）
- `ir/loop_parallel/region.rs`：`reject_cursor_fn_sites` 三条件判定
- `ir/loop_parallel/prewarm.rs`：fn_site 物化表写集证明 + 快照导出
- `engine/loop_parallel.rs`：fn_site 快照并入 `FrozenLookupSnapshot`
- `ParallelSkipReason`：新增 `FnSiteNotLoopInvariant`、`FnSiteTableWrittenInLoop`（细化原 `MaterializedFnSitePresent` 保留给不可物化形态）

## 守护测试

1. `should_scope_tx_anchor_to_region_suffix_closure`：区域外动态 SQL 放行、后缀 `CURRENT_TIMESTAMP` 仍拒
2. `should_snapshot_frozen_fn_site_table`：fn_site 物化后 Worker probe 命中、CLOSE 后语义一致
3. `should_reject_row_dependent_fn_site`：fn_site distinct_sql 含行字段绑定 → `FnSiteNotLoopInvariant`
4. `should_reject_fn_site_table_written_in_loop`：循环内 UPDATE sd_* → 拒
5. `should_byfund_segment1_cursor_admitted`：c_get_data 游标 fn_sites 通过 E5 后进入后续门禁
