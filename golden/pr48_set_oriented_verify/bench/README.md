# PR #48 集合化 —— purchase 同形 100k 三方基准

在 **100k 行同源数据**下，对比三方执行 purchase 核心『游标读 → INSERT』搬运的
**性能**与**数据一致性**：

| 方 | 含义 | 执行形态 |
|---|---|---|
| **存过（openGauss 原生）** | 参照容器内原生 plpgsql `FOR` 隐式游标逐行 INSERT | 编译态 RBAR |
| **SpinDuck OFF** | `set_oriented=false`，解释器逐行 FETCH+INSERT | 解释态 RBAR |
| **SpinDuck ON** | `set_oriented=true`，整体下推 `INSERT...SELECT` | 集合化 |

> 重要前提：**原始 purchase 主过程**含异常/子调用/多写，被集合化 `Skipped`，
> ON==OFF（不转换）。本基准用的是 purchase **同形可转换**过程（纯列投影搬运），
> 让集合化真正触发，以展示收益与等价。原始过程的边界实证见 [../REPORT.md](../REPORT.md) §8
> 与 Part B（`bench_partb/`）。

## 文件清单

```
bench/
├── README.md                  ← 本文件
├── run_bench.sh               ← 一键三方基准（构建/复用 CLI → 同源 100k → 3 轮测时 → 三方一致性断言）
├── sql/
│   ├── spinduck_tables.sql    ← SpinDuck 侧同形表（裸 VARCHAR+BIGINT，集合化 DDL 白名单）
│   ├── copy_procedure.sql     ← 被测过程（六指令可转换模式）
│   ├── reference_setup.sql    ← 参照侧表 + plpgsql 游标函数
│   └── gen_data.sql           ← 100k 确定性数据（仅 DuckDB 生成，CSV 同源载入参照）
└── reports/
    ├── bench_summary.log      ← 性能表 + 一致性结论 + 口径
    ├── bench_on_call.txt      ← ON 调用输出（含 SET_ORIENTED converted）
    └── bench_off_call.txt     ← OFF 调用输出
```

## 方法论

1. **同形数据**：列形取自 `mid_yjqs_detail` 关键列（gddm/xwdm/scdm/zqdm/security_id/
   bs/trade_type/bcrq/cjsl）。类型映射到集合化 DDL 白名单——**VARCHAR 不带长度**
   （带长度会使 `schema.rs` DDL 解析失败 → `UnknownObjectDependency`）、`cjsl BIGINT`、
   全可空、无约束。100k 行确定性分布（500 股东 × 50 席位 × 1000 证券 × 2000 合约）。
2. **同源保证**：数据仅在 DuckDB 侧 `generate_series` 生成，`COPY TO` CSV 后
   `docker cp` 进容器、参照侧 `\copy FROM` 载入——两侧源数据逐字一致（脚本断言两侧均 100000 行）。
3. **测时**：每方 `ROUNDS` 轮（默认 3）取中位数。每轮先 `TRUNCATE` 目标表再调用。
   - SpinDuck：`python3 perf_counter` 包裹 `call` 命令的**端到端墙钟**（含进程启动/开库/注册/事务）。
   - 参照：plpgsql `DO` 块内 `clock_timestamp()` 差，**纯函数执行**（不含连接往返）。
   - 另测 `noop` 空过程 call 中位数，量化 SpinDuck 固定开销。
4. **一致性**：三方各做一次清洁调用，目标表按全列 `ORDER BY` 导出 CSV，归一表头后
   **三方逐行 diff**（含重复行；本数据无 NULL）。脚本断言 diff 为空且三方均 100000 行。

## 复现

```bash
# 参照容器须 healthy
docker compose -f golden/gaussdb-docker/docker-compose.yml up -d
# 仓库根目录下执行
bash golden/pr48_set_oriented_verify/bench/run_bench.sh
SPINDUCK_BIN=/path/to/spinduck bash ...   # 复用已构建 PR-head CLI（跳过首次构建）
ROUNDS=5 bash ...                         # 覆盖轮数
KEEP_WORKTREE=1 bash ...                  # 保留临时目录
```

口令经容器 env 的 `$GS_PASSWORD` 引用在容器内展开，不上命令行/输出。
SpinDuck 侧无需 GaussDB 容器；参照侧需要。

## 口径与边界（务必阅读）

- **测量口径不同**：SpinDuck 是 CLI 端到端墙钟（含固定开销，实测 noop 中位约 175ms）；
  参照是纯函数执行。OFF→ON 加速比用**同为 CLI 端到端**口径计算， apples-to-apples。
  ON 墙钟主要由固定开销 + 落盘提交构成，**净集合化执行远小于墙钟**
  （进程内探针 100k 约 50ms 佐证，见 [../reports/perf.log](../reports/perf.log)）。
- **debug 构建**：绝对值仅证明方向与量级，非 release 性能承诺。
- **同形 ≠ 原过程**：本基准过程是为触发集合化而抽取的纯列投影搬运，
  不含原 purchase 过程的异常/子调用/逐行 CASE/多写。它证明的是
  「**当过程落在准入面内时**，集合化的收益与等价」，而非原过程可被集合化。
- 参照侧用 `FUNCTION + FOR 隐式游标`（openGauss `CREATE PROCEDURE` 不接受 PG `$$` 体，
  V10/V13），与被测过程同一搬运逻辑，结果表逐行比对保证语义对齐。