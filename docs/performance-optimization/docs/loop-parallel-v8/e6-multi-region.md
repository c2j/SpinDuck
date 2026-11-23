# E6 多区域判定与报告 + E8 报告字段扩展详设

> V8 机制 E6（byfund 4 段共存判定）+ E8（`ParallelStatus` 可观测扩展）。

## E6 多区域判定

### 问题

`set_oriented::region::extract` 每例程只取**首个**静态游标；byfund 有 4 段循环（`c_get_data`/`c_fxj`/`c_get_unknown`/QFII FOR），段 1 被并行后其余段仍由主线程原生串行——语义正确，但审计面无法表达"段 1 Parallelized、段 2-4 Skipped"的分段判定。

### 机制

1. `analyze` 改为**逐区域**：枚举例程内全部 SESE 游标区域（静态 + 动态分别处理），每区域独立跑门禁链，产出 `Vec<RegionPlan>`；
2. `try_loop_parallel` 驱动**首个**获得 `Parallelized` 资格的区域（保守：同时只并行一个循环，避免跨区域状态交互证明）；其余区域照常串行；
3. 每区域独立 `LoopParallelReport`（`routine` 字段扩展为 `routine#region_id` 或报告携带 `region: CursorRegion` 摘要）；
4. 显式排除清单（设计声明，非"碰巧被拒"）：段 2 `c_fxj`（嵌套动态游标，本期不支持）、段 3 `c_get_unknown`（体仅 `inst_log`，`InsufficientParallelWork` 终态）、QFII FOR 段（fn_sites + 双 UPDATE 写集，E7 不做）——在 `docs/validation.md` 记录排除理由。

### 正确性论证

多区域顺序执行：区域 i 并行窗口结束后 `InterpState` 完整交还主线程，区域 i+1 的 `OPEN`/前缀在同一状态上原生执行——与串行逐指令序列一致。区域间携带变量由主线程 frame 自然承载（`InterpState` 不重置）。

## E8 报告字段扩展

`ParallelStatus::Parallelized` 增补（全部 `u64`/`usize`，审计可选）：

```rust
Parallelized {
    workers, rows_dispatched, rows_fallback_serial,
    rollback_invalidated, prewarmed_tables, prewarm_ms,
    snapshot_hits, snapshot_misses, peak_snapshot_bytes,
    // V8 新增
    prewarm_keys: u64,            // E1/E2 distinct 键总数（分阶段累计）
    keyed_probe_hits: u64,        // Worker probe 命中
    keyed_probe_misses: u64,      // Worker probe miss → yield（应恒 0，防御性计数）
    carried_vars_resolved: u64,   // E3 主线程顺序解算的携带变量写点数
    rows_yield_mid: u64,          // 行中途交权（Yield{pc}/ErrorRow handler）行数
}
```

`Skipped` 细分：`reason` 复用现有枚举 + 新增 `PrewarmTooExpensive`、`FnSiteNotLoopInvariant`、`FnSiteTableWrittenInLoop`。

## 改动面

- `ir/loop_parallel/region.rs`：`extract` → `extract_all`（返回 `Vec<CursorRegion>`）
- `ir/loop_parallel/mod.rs`：`analyze` 多区域、`ParallelStatus` 字段、`ParallelSkipReason` 新变体
- `engine/loop_parallel.rs`：逐区域驱动循环、报告携带 region 标识
- `runtime/parallel/`：probe 计数器透传

## 守护测试

1. `should_report_per_region_status`：byfund 种子上段1 Parallelized + 段2-4 各自 Skipped{reason} 共存
2. `should_only_parallelize_first_eligible_region`：两区域均可并行时只并行首个
3. `should_report_keyed_probe_counters`：`prewarm_keys`/`keyed_probe_hits` 与 FakeExecutor 实际 SQL 数一致
4. `should_excluded_segments_stay_serial`：段2/3/4 输出 ≡ Off 且报告 reason 符合预期
