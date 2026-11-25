# V4 loop-parallel 真实语料验证 —— 复现包

本目录是 `loop-parallel`（`[engine].loop_parallel`，`LoopParallelMode::Safe`）
与 `set_oriented` 在**真实考题语料** `golden/case_corpus_purchase` 上的
能力边界与性能验证资产：差分探针 + 实测日志 + 一键复现脚本 + 报告。

验证对象两个存储过程（同一 `PKG_SPLIT_TRADE_STEP3_SH` 包）：

| 例程 | 角色 |
|---|---|
| `prc_trd_deal_purchase` | purchase 主过程：单游标 `c_get_purchase` + 行内 SELECT INTO 查找 + 异常行跳过 |
| `prc_trd_hz_byfund` | byfund 主过程：多游标（含 REF CURSOR 动态源）+ fn_sites 物化调用 + 动态 SQL 段 |

## 文件清单

```
v4_loop_parallel_verify/
├── README.md                        ← 本文件（方法论 + 复现步骤）
├── REPORT.md                        ← 等价性验证报告（判定矩阵 / 分支覆盖 / 性能）
├── SO_VS_LP.md                      ← SO vs LP 性能矩阵报告（100k 量级）
├── BRANCH_COVERAGE.md               ← 分支覆盖矩阵 + 种子确定性约束
├── run_verify.sh                    ← 一键复现脚本（注入探针 → cargo test → 落日志）
├── probe/
│   └── zz_v4_loop_parallel_probe.rs ← 差分/性能探针源码（16 个测试函数）
└── reports/
    ├── probe_run.log                ← 等价+规模化实测输出
    ├── perf_matrix_run.log          ← SO vs LP 矩阵首轮（分臂计划前基线）
    ├── perf_matrix_run2.log         ← SO vs LP 矩阵复测（分臂计划+派发修复后）
    └── summary.log                  ← 结果汇总表
```

分支覆盖种子（`golden/case_corpus_purchase/cases/datasets_branch/`）：

- `seed_branch_purchase.sql` —— purchase 专用维度 + P0-P5 全形态明细行
- `seed_branch_byfund.sql` —— byfund 全段维度 + B1-B5/B7-B18 主臂明细行

## 方法论

**差分等价**：同一份种子上分别以 `loop_parallel.mode = Off / Safe`
（threads=8、chunk_size=256、min_rows=256）部署独立内存引擎，`call`
同一例程后：

- `outs`（OUT 形参）逐值相等；
- 全部用户表（排除 `sd_*` 物化临时表）行集排序后逐表全等；
- Safe 侧打印 `loop_parallel_reports()` 的 `Parallelized` / `Skipped{reason}`。

**分支覆盖**：`min_rows=1` 强制小规模数据也走并行路径，使全部
数据可触发分支同时经受 Off/Safe 差分（矩阵与缺口见
`BRANCH_COVERAGE.md`）。

**规模化**：明细行克隆放大使游标行数越过 `min_rows` 门槛
（purchase ×16000 → ~96k 游标行；byfund 分组克隆 → 1k~5k 组）。

**性能矩阵**（`SO_VS_LP.md`）：同一场景数据下 4 配置
（off / so_ba / lp_safe / so_lp）预热 1 轮 + 计时多轮取最小值，
并打印 `set_oriented_reports()` 与 `loop_parallel_reports()` 判定。

探针 16 个测试函数：

| 分组 | 测试 |
|---|---|
| 等价·小规模 | `v4_purchase_small_scale_equiv`、`v4_byfund_small_scale_equiv` |
| 等价·规模化 | `v4_purchase_scale_equiv`、`v4_byfund_scale_equiv` |
| 性能·LP | `v4_purchase_perf`、`v4_byfund_perf` |
| 分支·等价 | `v4_purchase_branch_equiv`、`v4_purchase_branch_kind1_equiv`、`v4_byfund_branch_equiv`、`v4_byfund_branch_nodata_equiv`、`v4_byfund_branch_serial_stability` |
| 性能·SO vs LP 矩阵 | `v4_perf_purchase_base`、`v4_perf_purchase_branch_k0`、`v4_perf_purchase_branch_k1`、`v4_perf_byfund_base`、`v4_perf_byfund_branch` |

## 环境要求

- Rust **stable**、cargo；`--features native`（进程内 DuckDB 1.5.5）。
- 首次构建编译 DuckDB bundled（约 10 分钟）；`spinduck/target` 有缓存时秒级。
- **无需** GaussDB 容器、无需任何外部环境变量。
- 探针依赖 `spinduck_core::ir::loop_parallel` 公共 API，**只能在含
  loop-parallel 实现的分支上编译**（本 worktree 即目标分支）。

## 复现步骤

```bash
# 在 feat-loop-parallel worktree 根目录
bash golden/v4_loop_parallel_verify/run_verify.sh
# 等价+规模化约 2-4 分钟；SO vs LP 矩阵约 30 分钟
# （byfund off 单轮 ~40-50s × 多配置多轮为主）
```

脚本把 `probe/zz_v4_loop_parallel_probe.rs` 复制进 `spinduck/tests/`
（cargo autodiscovery 自动注册为 `zz_v4_loop_parallel_probe` 测试目标），
跑完删除（`KEEP_PROBE=1` 保留）。实测日志覆盖写入 `reports/probe_run.log`。
