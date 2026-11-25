# E1 行键预热（Keyed Prewarm）详设

> V8 机制 E1。目标：让**行键绑定**的 `SqlQuery` 进入 `G_pure`——Worker 经冻结快照按键探测，零 SQL。

## 1. 问题

V7 `query_cacheable`（`ir/loop_parallel/region.rs`）要求 `shape::recognize` 通过 **且** 全部 `range_binds` 绑定为循环不变（`BindSrc::Var` 不被 `loop_writes` 覆写）。行键绑定（`BindSrc::Field{rec,..}`，`rec` 被 `Fetch` 每轮重写）恒判不可预热 → 交权。purchase `c_get_purchase` 首条 `SELECT INTO` 即行键 → `G_pure={PushTrap}` → `InsufficientParallelWork`。

## 2. 核心洞察

游标行集 `OPEN` 后已物化（`RowStream.rows: Arc<Vec<Row>>`）。对区域内每个候选行键查询，其**distinct 键元组集合在派发前可由主线程枚举**：

```
keys(Q) = { key_fn(row) | row ∈ materialized_rows[base_idx..total] }
```

预热成本 = `|keys(Q)|` 次查询（主线程串行），而非 `rows × queries`。基数摊薄是常态（维表键 ≪ 行数）。

## 3. 静态面：键函数提取与 key_dag

### 3.1 键函数 `key_fn`

对候选 `IrStmt::SqlQuery { sql, binds, into, fn_sites }`：

1. `shape::recognize(sql)` 必须通过（沿用现有点查形态约束）；
2. `fn_sites.is_empty()`（E5 落地后放宽，见 e5 文档）；
3. 对 `plan.range_binds()` 之外的**全部绑定**，要求 `BindSrc` 可归约为键表达式：
   - `BindSrc::Field { rec: cursor_record, field }` → `row[field]`（`rec` 必须是区域游标记录变量，静态可判）；
   - `BindSrc::Var(v)` → 若 `v` 为循环不变 → 常量折入键；若 `v` 为前序候选查询/调用的 `into` 产物 → **二阶键**，键表达式引用 `results[prior_pc][into_var]`；
   - `BindSrc::Expr` → 拒绝（不可静态求值）。
4. `key_fn` 输出 `Vec<SpinValue>`（键元组，定长）。

### 3.2 键依赖图 `key_dag`

节点 = 区域内全部候选 keyed probe（SqlQuery / E2 调用）。边 `A → B` 当 B 的某键分量依赖 A 的 `into`/OUT 产物（例：purchase `Q1→v_fund_code→Q3`；byfund `seat_para→v_return_rate_*`）。要求 **DAG 无环**；环 → `UnsupportedControlFlow`。拓扑序决定**预热阶段**：第 k 阶段的键只能用 ≤k-1 阶段已物化的结果投影。

### 3.3 `G_pure` 准入修订

`classify` 的 `SqlQuery` 分支扩展为三类：

| 类别 | 条件 | Worker 行为 |
|---|---|---|
| `InvariantProbe`（V7 现状） | range_key 循环不变 | 现有 `FrozenLookupSnapshot::probe` |
| `KeyedProbe`（V8 新增） | 键函数可提取 + key_dag 无环 + 成本门禁过 | `snapshot.probe_keyed(query_id, key)` |
| yield | 其余 | 交权（不变） |

`queries` 计划字段扩展为 `Vec<ProbePlan>`，`ProbePlan` 为枚举 `{ Invariant{pc,sql}, Keyed{pc,sql,key_fn_id,phase} }`。

## 4. 运行面：分阶段预热协议

在现有三步协议（§5.1）之后、`dispatch_chunk` 之前插入 **Phase-B**：

```
for phase in topo_order(key_dag):
    for each probe p in phase:
        keys = distinct{ key_fn_p(row, prior_results) | row ∈ rows[base..total] }
        if keys.is_empty(): continue
        if est_prewarm_cost(keys) > PREWARM_BUDGET: → Skipped(PrewarmTooExpensive) 串行
        for k in keys:
            outcome = exec_lookup_or_call(p, k)   # 主线程, try_prewarm_only 同口径
            match outcome:
                Ok(res)      → snapshot.insert(p.id, k, res)
                Empty        → snapshot.insert(p.id, k, Empty)   # NO_DATA_FOUND 等价
                Err(e)       → snapshot.insert(p.id, k, Error(e.clone()))  # 错误也冻结? 见 §6
    # 阶段间: prior_results = 每行已解析的 into 值 (主线程投影, 无 SQL)
```

**关键点**：预热在主线程串行执行、trace 静默、不计步数（与 §5.1 口径一致）。`Error` 冻结是必须的——行键查询的 `NO_DATA_FOUND`/`OTHERS` 在串行中是**逐行**语义（handler 分派），快照必须能区分 `Empty`（→ handler）与 `Error`（→ `__sqlerrm` + handler），Worker 据此产生 `WorkerOutcome::ErrorRow{pc}` 交权。

### 4.1 键空间上界与成本门禁

```
PREWARM_BUDGET = max(64, rows_remaining / 8)   # 可调旋钮, 默认值进 LoopParallelConfig
est_cost(p)     = |keys(p)| × est_query_cost(p.sql)
```

任一阶段 `Σ est_cost > budget` → `Skipped(PrewarmTooExpensive)`（新 `ParallelSkipReason` 变体），同状态串行。防全 distinct 退化。

## 5. Worker 侧

`WorkerSandbox` 新增 `KeyedSnapshot`（`Arc<HashMap<(ProbeId, Key), ProbeResult>>`，`Send+Sync`）：

```rust
enum ProbeResult {
    Rows(Vec<Row>),        // SELECT INTO 语义: 0行=Empty由调用方判, >1行=TooMany
    Empty,
    Error(EngineError),    // 冻结的错误 (code+message, 不含连接态)
}
```

Worker 执行 keyed `SqlQuery` 语句时：
- `probe` 命中 `Rows(r)` → 按 `into` 目标写 `write_log`（复用 `interp::assign` 同语义 coerce）；
- `Empty` → 产生 `WorkerOutcome::ErrorRow { pc: stmt_pc, kind: NoDataFound }`（SELECT INTO 空集语义）；
- `Error(e)` → `WorkerOutcome::ErrorRow { pc, kind: Others(e) }`；
- miss（键不在快照——理论上不可能，防御性）→ `Yield{pc}` 交权。

trap 分派（`PushTrap`/`PopTrap` 包裹的 SELECT INTO）由 Worker 按 `TrapEntry` 结构本地模拟：命中 `ErrorRow` 时查 `cond` 匹配（`no_data_found`/`others`/`too_many_rows`）→ 若 handler 体在 `G_pure` 内继续纯执行，否则 `Yield{pc: handler_pc}` 交权——**handler 内的 `inst_log`/`GOTO` 一律交权**，主线程原生执行。

## 6. 语义对齐清单（守护点）

| 语义 | 对齐方式 |
|---|---|
| SELECT INTO 0 行 → `NO_DATA_FOUND` → handler | `ProbeResult::Empty` → `ErrorRow{NoDataFound}` → 交权 handler |
| SELECT INTO >1 行 → `TOO_MANY_ROWS` | `Rows(r) if r.len()>1` → `ErrorRow{TooManyRows}` |
| 内核/业务错 → `OTHERS` + `__sqlerrm` | `Error(e)` → `ErrorRow{Others}`，主线程重放 `__sqlerrm` 写入 |
| 键相同结果必相同 | 确定性门禁（门禁 5 扩展至 keyed 查询） |
| 预热额外 SQL | trace 静默、不计步数、`prewarm_sql_count` 单列（§5.1 口径不变） |
| 键依赖顺序 | key_dag 拓扑 + 分阶段物化 |

## 7. 改动面

- `ir/loop_parallel/keyed.rs`（新）：键函数提取、key_dag、ProbePlan
- `ir/loop_parallel/region.rs`：`classify`/`query_cacheable` 三类化
- `lookup_cache/`：`FrozenLookupSnapshot` 扩展 `probe_keyed`（或新 `KeyedSnapshot` 类型）
- `runtime/parallel/worker.rs`：`ProbeResult` 消费 + `ErrorRow` 产生
- `engine/loop_parallel.rs`：Phase-B 预热循环 + 键投影 + 成本门禁
- `ir/loop_parallel/mod.rs`：`ParallelSkipReason::PrewarmTooExpensive`、`LoopParallelConfig` 新旋钮

## 8. 守护测试（TDD 必备）

1. `should_keyed_probe_collapses_distinct_keys`：600 行 3 distinct 键 → FakeExecutor 只见 3 条预热 SQL
2. `should_keyed_probe_second_order_dependency`：Q3 键含 Q1 产物，预热顺序断言
3. `should_keyed_probe_empty_maps_no_data_found`：快照 Empty → handler 交权，`__sqlerrm`/trace 与 Off 一致
4. `should_keyed_probe_error_maps_others`：冻结 Error → `OTHERS` handler
5. `should_prewarm_budget_fallback`：distinct 键超阈值 → `Skipped(PrewarmTooExpensive)` 且输出 ≡ Off
6. `should_keyed_probe_real_duckdb_equiv`：构造例程（行键 SELECT INTO + INSERT）Off/Safe 逐行全等
