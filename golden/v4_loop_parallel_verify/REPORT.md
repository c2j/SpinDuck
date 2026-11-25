# V4 loop-parallel 真实语料验证报告（V8 终验：purchase + byfund）

> 被测面：`feat-loop-parallel` 分支，`LoopParallelMode::Safe`
> （threads=**8**、chunk_size=256、min_rows=256）。语料：
> `golden/case_corpus_purchase`。内核：进程内 DuckDB 1.5.5。
> 实测日志：`reports/probe_run.log`。

## 1. 判定矩阵（终验口径）

| 例程 | 数据规模 | Safe 判定 | 说明 |
|---|---|---|---|
| `prc_trd_deal_purchase` | 3 行种子 | `Skipped InsufficientParallelWork` | 低于 min_rows，正确终态 |
| `prc_trd_deal_purchase` | **96,006 游标行**（3×16000 克隆） | **`Parallelized`** workers=8, fallback=0 | 全量并行派发 |
| `prc_trd_hz_byfund` | 种子 | `Skipped InsufficientParallelWork` | 低于 min_rows，正确终态 |
| `prc_trd_hz_byfund` | **1,002 游标行** | **`Parallelized`** workers=8, fallback=0 | V8-E4+E5 后段 1 准入 |

## 2. purchase 终验结果（约 10 万行 / 并行度 8）

**正确性**：`outs=EQ state=EQ`——OUT 形参与全部用户表行集逐值全等；
`rows_dispatched=96006`、`rows_fallback_serial=0`、
`rollback_invalidated=false`。

**性能**（3 轮取最小，含 1 轮预热不计入）：

| 模式 | min | 全部轮次 |
|---|---|---|
| Off（串行） | **8744 ms** | [9701, 8744, 8752] |
| Safe（并行×8） | **3859 ms** | [3859, 3937, 3958] |

**加速比 ≈ 2.27×**（`reports/probe_run.log` 终验轮；独立复测轮
8538→3562 ms ≈ 2.40×，两轮一致量级）。小规模复测（1206 行 /
4 线程）：115 ms → 65 ms（≈1.77×），收益随行数放大符合预期。

## 3. 本轮使 purchase 可并行的关键修复（相对上一版报告）

上一版结论"purchase 无可并行纯前缀"已被推翻，根因是两个精度缺陷
+ 一个证明过粗，均已修复并配 TDD 守护：

1. **`exprs::stmt_defs` 过度近似**：`CallRoutine` 的每个可写实参都
   计为写，但运行时仅 `ParamMode::Out/InOut` 形参写回。`inst_log`
   的 `p_date` 为 IN——`p_i_date` 被误判循环写 → 行键查询交权 →
   `G_pure` 为空。修复：按 catalog 形参 mode 过滤（守护：
   `callout_tests.rs` 2 例）。
2. **`prewarm_one` 绑定解析过宽**：对全部 binds（含行字段）做
   `resolve_binds`，FETCH 前记录未绑定必失败。修复：只解析
   `shape.range_binds()`——`FrozenLookupSnapshot::probe` 本就按
   行键 binds 逐行过滤，**行键查询无需 distinct-key 预热**
   （E1 详设已按此修正）。
3. **反重入证明过粗**：trap 分派边一律作为交权种子 → purchase 的
   「处理器前向汇合到 `G_pure` 后继」（pc50→handler54→pc56）被
   误判重入。修复：种子只取结构后继；trap 边改为「受保护语句 →
   处理器 → 可达集 ∩ 该语句的 `G_pure` 祖先集」精确判定——
   后继/并行分支可达安全（原生执行受保护语句 ⟹ Worker 已在该点
   交权，其后代未执行），祖先可达才拒（守护：
   `reentry_tests.rs` 准入用例 + `gate_tests` 既有拒绝用例）。
4. **E2 调用物化**：`FuncCall`/`SqlFn` 经 `callmat` 静态证明
   （无写/无事务锚定/无动态 SQL/无游标/无 Raise）后按
   `(name, args)` 键物化进 `CallSnapshot`；Worker miss 返回
   `NeedKeys` 由主线程补物化重派（`runtime/parallel/calls.rs`、
   `worker.rs`）。
5. **`WorkerOutcome::QueryError`**：快照权威的 SELECT INTO 基数
   错误（NO_DATA_FOUND/TOO_MANY_ROWS）显式携带错误种类交权，
   主线程注入 trap 分派而非整行重跑。
6. **Top-1 查询降级为交权边界**（`rownum=1`/`LIMIT 1` 不再整域
   拒绝，保留前缀并行度）。

## 4. byfund 终验结果：段 1 `Parallelized`（V8-E4 + E5 生效）

上一轮 `Skipped MaterializedFnSitePresent` 的两重阻断已按批准的
V8 修订解除，**未实施 E3/E6**——实证表明段 1 并不需要它们：

- **E4（已批准修订：审计排除后缀）**：门禁 5 事务锚定审计范围从
  全例程收窄为 `[入口 ..= close_pc]` + 传递调用闭包。byfund 段 2
  动态游标 `OPEN c_fxj FOR` 位于段 1 `close_pc` 之后，不再污染
  段 1 判定（守护：`gate_tests` 两个 admit 用例）。
- **E5（fn_site 快照化准入）**：`c_get_data` 游标 WHERE 的
  `fnc_get_fund_data_mode` 物化在 `OPEN` 主线程同点完成、内容对
  Worker 只读——准入条件收窄为「绑定无行相关源（`Field`/`Expr`）
  且物化表不被 `W_loop` 写」。游标 fn_site callee 豁免事务锚定
  审计（执行点两种模式逐语句一致）。守护：`fnsite_tests.rs` 4 例
  （Var 绑定准入 / Field 绑定拒 / 循环写 sd_fn0 拒 / 写他表准入）。
- **E3 未触发**：`v_temp_stock_kind` 残留的全部读点均在交权/后缀
  侧（`fnc_get_rate_info2` + INSERT 由主线程按 row_idx 顺序执行），
  Worker 前缀不需要该值 → 门禁 2 未检出循环携带依赖，无需主线程
  携带态解算。
- **E6 未触发**：`extract` 仍只取首个静态游标区域（段 1），段 2/3/4
  在区域关闭后原生串行续跑——单区域并行已获益，多区域属增量优化。

**正确性**：`outs=EQ state=EQ`；`rows_dispatched=1002`、
`rows_fallback_serial=0`、`rollback_invalidated=false`、
`snapshot_hits=1002 / misses=512`（miss 经 `NeedKeys` 主线程补物化）。

**性能**（终验轮，off_all/safe_all 五轮取最小）：

| 模式 | min | 全部轮次 |
|---|---|---|
| Off（串行） | **9809 ms** | [17521, 12440, 9809, 9967, 10150] |
| Safe（并行×8） | **8702 ms** | [8872, 8702, 8769, 8743, 8776] |

**加速比 ≈ 1.13×**——收益有限且如实记录：并行窗口只覆盖段 1
（`c_get_data`）的纯前缀查找，段 1 的 INSERT+COMMIT 后缀及段 2/3/4
全部留在主线程串行；1002 行规模下派发开销占比高。小规模（种子）
正确 `Skipped InsufficientParallelWork`。

## 5. 分支覆盖验收（BRANCH_COVERAGE.md）

`tests/zz_v4_loop_parallel_probe.rs` 新增 5 个分支用例（`min_rows=1`
强制小数据也走并行路径），种子为 `cases/datasets_branch/` 下两个
自含文件（`seed_branch_purchase.sql` / `seed_branch_byfund.sql`）：

| 用例 | 结果 |
|---|---|
| `v4_purchase_branch_equiv`（P0-P5 全形态） | outs=EQ state=EQ，Parallelized |
| `v4_purchase_branch_kind1_equiv`（P4 kind='1'） | outs=EQ state=EQ，Parallelized |
| `v4_byfund_branch_equiv`（B1-B5、B7-B18 主臂） | outs=EQ state=EQ，Parallelized |
| `v4_byfund_branch_nodata_equiv`（B5 NO_DATA_FOUND 臂） | outs=EQ state=EQ，Parallelized |
| `v4_byfund_branch_serial_stability`（Off↔Off 确定性守护） | outs=EQ state=EQ |

覆盖缺口与种子确定性约束（`p_o_succeed` 末次写赢 + 无序游标 →
同消息约束）详见 `BRANCH_COVERAGE.md` 末节，如实记录未穷举臂。

## 6. 门禁与已知环境问题

- `cargo fmt --all -- --check`：干净；`cargo clippy --all-features
  -- -D warnings`：干净；`cargo test --lib`：**779/779**；
  `tests/loop_parallel.rs` 4/4、`cases_native` 6/6、
  `zz_v4_loop_parallel_probe` **11/11**（含 5 分支用例 + 6 规模/等价用例）。
  `set_oriented_*` 相关全绿。
- `cargo test --all-features --test cli`：1 失败
  （`gauss_boundary::...materialize_has_no_instance_env`）——
  **主 worktree 同样失败，环境/既有问题，与本改动无关**。
- `cargo test --all-features --test golden_corpus_purchase`：
  需 GaussDB A 模式容器，本地无容器必红（主 worktree 同）。
- `cargo audit`：`zip 6.0.0` 7 个 RUSTSEC（advisory-db 新增）——
  **主 worktree 同样失败**，属依赖升级事项（Ask first），
  非本改动引入。

## 7. 复现

```bash
bash golden/v4_loop_parallel_verify/run_verify.sh
# 期望：purchase scale_equiv/perf 输出 Parallelized + EQ（≈2.3×）；
# byfund 大规模 equiv/perf 输出 Parallelized + EQ（≈1.13×），
# 小规模 Skipped InsufficientParallelWork
```
