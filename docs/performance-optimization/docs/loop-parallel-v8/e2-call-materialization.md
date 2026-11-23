# E2 确定性调用物化（Call Materialization）详设

> V8 机制 E2。目标：让**确定性、无副作用**的 `CallRoutine` / `FuncCall` / `SqlFn` 按参数键进入 keyed prewarm（含 OUT 参整体物化），其余照旧交权。

## 1. 问题

V7 门禁 3/表达式级禁令：`CallRoutine` 强制交权（`interp.rs` 每次调用前 `writebuf::flush_all`，batch_commit 下影响落盘时机）；`FuncCall`/`SqlFn` 未本地降阶一律交权。purchase 的 `FNC_GET_PURCHASE_JS_DAYS`、`fnc_com_if_deal_date`、`fnc_com_getday`，byfund 的 `fnc_com_get_seat_para`（3 个 OUT 参）、`fnc_get_rate_info2`、`fnc_get_bond_yjlx` 全部命中 → 即使 E1 落地，这些调用仍是逐行串行瓶颈。

## 2. 机制

### 2.1 callee 确定性证明（静态，传递闭包）

对候选调用目标 `name`，沿 `RoutineCatalog` 解析 callee 体，递归证明：

- 无 `SqlExec`/`SqlExecDynamic`/`TxCommit`/`TxRollback`/`OpenCursor`/`Fetch`/`CloseCursor`/`Raise`（无写、无事务、无游标、无异常外抛——**注意：`SELECT INTO` 的 `NO_DATA_FOUND` 业务异常允许**，它是返回值语义而非副作用）；
- 无 `NEXTVAL`/`CURRVAL`/`RANDOM`/`UUID`/`CURRENT_*`/`NOW`/`SYSDATE` 等会话/事务锚定函数（复用门禁 5 `scan_expr_tx` 判定集）；
- 无动态 SQL 文本构造；
- 传递闭包内所有 `CallRoutine`/`FuncCall`/`SqlFn` 同样满足（闭包不可达 → 拒）。

不满足 → 该调用语句成为 `E_yield` 交权点（V7 行为不变），**不阻断** `G_pure` 其余部分。

### 2.2 调用键与结果物化

满足证明的调用纳入 E1 `key_dag`，键 = `args` 全部实参求值结果（`BindSrc`/`Expr` 均可——实参在 Worker 侧由 `eval_pure` 可算子集求值；含 `FuncCall` 实参的递归要求同 2.1）：

```
key = (fn_id, eval(arg1), eval(arg2), …)
result = CallOutcome {
    ret: Option<SpinValue>,
    outs: Vec<Option<SpinValue>>,   // out_targets 全槽位, 包括 None
    raised: Option<ErrorKind>,      // NO_DATA_FOUND / OTHERS(e) —— 调用级异常也物化
}
```

预热执行 = 主线程真实 `CallRoutine` 一次/键（trace 静默、不计步数、计入 `prewarm_sql_count`）。`raised` 物化是必须的：callee 内 `SELECT INTO` 空集在串行中走 handler——Worker 命中 `raised` 产生 `ErrorRow{pc}` 交权，语义与 E1 `ProbeResult::Error` 同构。

### 2.3 Worker 侧

命中快照 → 按 `interp::assign` 同语义把 `ret`/`outs` 写入 `write_log`（coerce 后终值，主线程不二次 coerce——沿用 R4-P1-1）；`raised` → `ErrorRow`/`Yield{handler_pc}` 交权。

### 2.4 `flush_all` 时序例外

V7 拒 `CallRoutine` 入 `G_pure` 的理由之一是 `interp.rs` 每次调用前 `writebuf::flush_all`。物化调用**在预热期**由主线程真实执行（flush 语义原样发生在预热点——但预热发生在 `OPEN` 后、循环前，此时 `wbuf` 按 `clean_at_prewarm` 已证为空，flush 为空操作，语义无损）。**循环内 Worker 命中不再触发 flush**——等价性论证：串行中第 i 行的 CallRoutine flush 刷的是第 i-1 行及更早的缓冲写；并行模式下缓冲写全部由主线程 suffix 按序产生，时机由主线程原生 `TxCommit`/`SqlExec`/出口指令决定，与串行逐指令序列一致（Worker 不产生任何写）。此论证须写入 §3.3 守护测试。

## 3. 与 E1 的关系

E2 是 E1 `key_dag` 的节点类型扩展：`ProbePlan::KeyedCall { pc, fn_id, key_fn, out_targets, phase }`。键依赖规则相同（OUT 参可成为下游键分量）。

## 4. 改动面

- `ir/loop_parallel/callmat.rs`（新）：callee 确定性证明、调用键提取、`ProbePlan::KeyedCall`
- `ir/loop_parallel/proof.rs`：复用/扩展 `scan_callee_tx` 为"可物化"判定
- `lookup_cache/` 或新 `call_snapshot`：`(fn_id,key)→CallOutcome` 冻结表
- `runtime/parallel/worker.rs`：`CallOutcome` 应用 + `raised` 分派
- `engine/loop_parallel.rs`：Phase-B 预热执行真实 CallRoutine

## 5. 守护测试

1. `should_materialize_deterministic_call_with_out_params`：3 OUT 参调用 keyed 物化，Worker 还原全槽位
2. `should_materialize_call_raised_maps_handler`：callee 内 SELECT INTO 空集 → `raised` → handler 交权逐字一致
3. `should_reject_nondeterministic_callee`：callee 含 `NEXTVAL`/写/动态 SQL → 交权，输出 ≡ Off
4. `should_call_key_second_order`：下游查询键依赖调用 OUT 参 → 分阶段预热顺序
5. `should_call_prewarm_trace_silent`：预热 CallRoutine 不产生 TraceEvent、不计步数
6. `should_wbuf_flush_timing_equiv`：batch_commit 下含物化调用循环，INSERT 落盘时机与 Off 一致
