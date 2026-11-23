# E3 携带态主线程解算（Carried-State Partitioning）详设

> V8 机制 E3。目标：把门禁 2 的"循环携带依赖一律拒"扩展为"携带变量白名单 + 主线程顺序解算"，解锁 byfund `v_temp_stock_kind` 类残留语义。

## 1. 问题

byfund `c_get_data` 段：`SELECT bond_kind INTO v_temp_stock_kind … EXCEPTION` 三分支（`no_data_found`/`too_many_rows`/`others`）**都不赋值** `v_temp_stock_kind` 只写 `p_o_succeed` → 该变量在异常行保留**上一行**的值，下游 `fnc_get_rate_info2(…, v_temp_stock_kind)` 消费。值依赖迭代历史——Worker 从同一 base frame 出发无法复现，门禁 2 按 `CycleCarriedDependence` 拒（设计 §7.1 预测的 280–297 行实锤）。`v_return_rate_flag/org/rate`（seat_para OUT 参）在调用异常路径同样残留。

## 2. 机制

### 2.1 携带变量分类（静态）

门禁 2 到达定值分析产出携带变量集后，逐变量判定是否可"主线程解算"：

**可解算（whitelist）**当且仅当：

1. 该变量的**全部读点**位于：① yield/后缀侧指令；② `G_pure` 内但其值可由"本行原始观测"确定性重建（即读点实际消费的是观测结果而非变量本身——静态上表现为：变量写点序列中，除"保留前值"外的写均为本行内确定性赋值）；
2. 该变量的写点序列可静态枚举（每个写点要么本行赋值、要么"异常路径不写=保留"）；
3. 该变量不被 `E_yield` 目标中的非顺序路径读取（反重入证明已保证 yield 后不可达 `V_pure`）。

不满足 → `CycleCarriedDependence`（V7 行为不变）。

### 2.2 Worker 侧：原始观测上报

Worker 不求值携带变量。对携带变量的每个"本行写点"，Worker 返回**观测结果**而非最终值：

```rust
enum CarriedObs {
    /// 本行写点成功赋值 (coerce 后终值)
    Assigned(SpinValue),
    /// 本行写点走异常路径未赋值 → 主线程保留前值
    Retained,
}
// WorkerRowResult.carried: Vec<(VarName, CarriedObs)>  (按写点序)
```

携带变量的**读点**若在 `G_pure` 内：要求该读点消费的表达式可改写为对"本行观测+前值"的纯函数——静态上即读点必须位于 yield 之后或其输入不含携带变量（由 2.1-1 保证）。实际 purchase/byfund 形态：携带变量只被 INSERT 内 `decode(fnc_get_rate_info2(…))` 消费 → 该调用本身就是 yield/后缀侧，天然满足。

### 2.3 主线程侧：顺序状态机

`consume_row` 在应用 `write_log` 后、续跑 suffix 前，按 `row_idx` 顺序执行携带变量状态机：

```
for (var, obs) in res.carried (写点序):
    match obs:
        Assigned(v) → frame.assign(var, v)          # coerce 后终值, 不二次 coerce
        Retained    → 保持 frame 当前值 (上轮顺序解算结果)
```

这精确复现串行语义：串行中第 i 行 handler 不写 → 变量保持第 i-1 行结束值；并行中主线程按序应用 `Retained` = 不动 frame = 同值。**关键不变量**：携带变量的 frame 值只由主线程顺序写入，Worker 的 `write_log`/`carried` 均不直接含其最终值——`write_log` 中携带变量槽位**必须剔除**（静态校验）。

### 2.4 与 trap/`__sqlerrm` 的关系

携带变量可能被 trap handler 读写（`__sqlerrm` 是内置携带变量，V7 已按隐式写建模拒绝）。V8 仅放行**用户变量**；`__sqlerrm`/`current_error` 依赖一律维持 `CycleCarriedDependence` 拒绝。

## 3. 守护测试

1. `should_resolve_carried_var_retained_across_error_rows`：构造"行1赋值→行2异常保留→行3读值"循环，Off/Safe 逐字段全等
2. `should_reject_carried_var_read_inside_gpure`：携带变量被 `G_pure` 内读点消费 → `CycleCarriedDependence`
3. `should_reject_carried_var_written_by_suffix`：suffix 写携带变量 → 拒（顺序不可证）
4. `should_carried_obs_order_matters`：同一行多写点（先异常后赋值）按序应用
5. `should_byfund_segment1_equiv`：真实 c_get_data 种子 Off/Safe 全表全等（集成级）

## 4. 改动面

- `ir/loop_parallel/proof.rs`：门禁 2 携带变量分类（whitelist 判定）
- `ir/loop_parallel/mod.rs`：`PurePrefixPlan.carried_vars: BTreeSet<VarName>`
- `runtime/parallel/worker.rs`：`WorkerRowResult.carried` + write_log 剔除
- `engine/loop_parallel.rs`：`consume_row` 携带状态机
- 报告：`ParallelStatus::Parallelized.carried_vars_resolved: usize`
