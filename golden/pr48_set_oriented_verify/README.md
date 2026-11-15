# PR #48 集合化能力边界验证 —— 复现包

本目录是 PR #48（`feat/set-oriented-v1-pr`，`[engine].set_oriented` 集合化改写）
**能力边界与完成情况**的独立验证资产：差分探针 + 实测日志 + 一键复现脚本 + 报告。
验证视角取自 `docs/performance-optimization/docs/set-orientation-papers/`
的三条集合化路线（去相关内联 / 游标消除 / 批处理重写）。

> 与 `golden/` 其余用例不同：本包的**探针与语料层不依赖 GaussDB 容器**，跑在
> 进程内 DuckDB 1.5.5 上；它验证的是「改写器在 OFF/ON 两种模式下业务语义是否等价、
> 准入边界何在、RBAR 消除方向是否成立」。**100k 三方基准的 Part A 需要 openGauss
> 参照容器**（`golden/gaussdb-docker`），Part B 不需要。

## 文件清单

```
pr48_set_oriented_verify/
├── README.md                  ← 本文件（方法论 + 复现步骤）
├── REPORT.md                  ← 验证报告（边界矩阵 / 等价结论 / 性能 / 路线完成度 / 真实语料 / 100k 三方）
├── run_verify.sh              ← 一键复现脚本（fetch PR head → worktree → 注入探针 → 运行）
├── probe/
│   └── zz_pr48_probe.rs       ← 差分探针源码（5 个测试函数，50 个差分变体 + 性能基准）
├── reports/
│   ├── correctness.log        ← 正确性差分实测（47 PASS + 3 CHAR，零分歧）
│   └── perf.log               ← 性能基准实测（多规模多轮墙钟 + 业务 SqlExec 计数）
├── corpus/                    ← 真实语料纯 CLI 验证（不写 Rust，空种子）
│   ├── README.md              ← 语料验证方法论 + 口径
│   ├── run_corpus_verify.sh   ← 特殊项目 toml + compile/deploy/call + ON/OFF 差分断言
│   └── reports/               ← corpus_{on,off,summary}.log（四例程全 Skipped，ON==OFF）
├── bench/                     ← Part A：purchase 同形可转换过程 100k 三方基准（需参照容器）
│   ├── README.md              ← 三方方法论 + 口径
│   ├── run_bench.sh           ← 同源 100k → 3 轮测时 → 三方逐行一致性断言
│   ├── sql/                   ← 同形表 / 可转换过程 / 参照函数 / 数据生成
│   └── reports/               ← bench_summary.log（OFF→ON ~90×，三方一致）
└── bench_partb/               ← Part B：原始 purchase 主过程 100k 边界实证（无需容器）
    ├── README.md              ← 边界实证方法论 + V21 参照限制
    ├── run_partb.sh           ← 原始语料工程 + 100k 种子 + ON/OFF Skipped 断言
    └── reports/               ← partb_summary.log（ON≡OFF Skipped，逐行一致）
```

本包有三层验证：**probe/**（玩具构造例程，Rust 差分探针，扫准入边界）、
**corpus/**（真实考题语料，纯 CLI，空种子验证转换判定与等价）、
**bench/ + bench_partb/**（100k 同源数据的端到端三方性能与一致性基准）。
三者结论一致：PR #48 集合化是路线一「模式 C」的最窄落点，真实语料全部 Skipped；
收益只在准入面内兑现（Part A ~90×），真实过程当前零收益（Part B ON≡OFF）。

## 方法论

**差分等价测试**：每个变体在同一份独立快照上分别跑 `set_oriented=false`（OFF）
与 `true`（ON），逐值比对业务观察面（目标表行 bag 含重复/NULL、OUT 参数、
错误码+消息），并断言 ON 的 `Converted` / `Skipped{reason}`。等价不变量：
Converted 时业务面必须与 OFF 逐值一致；Skipped 时回退原 IR 亦须一致。

探针 5 个测试函数：

| 测试函数 | 覆盖 |
|---|---|
| `probe_boundary_matrix` | 准入边界主矩阵（表达式/状态/副作用/类型/约束/对象依赖） |
| `probe_positive_controls` | 正例（应 Converted）+ 大小写引号边界 |
| `probe_case_sensitivity_edges` | 引号/混合大小写标识符（CHAR：等价 OK + 记录选择） |
| `probe_trace_roundtrip_counts` | OFF/ON 的 SqlExec/Fetch 往返计数（RBAR 消除证据） |
| `probe_perf_rbar_elimination` | 多规模（3/1k/20k/100k）多轮墙钟基准 |

探针采用失败收集模式（`thread_local FAILS`），单点不符不中断矩阵，末尾汇总断言。

## 环境要求

- Rust **stable**、cargo、git（与仓库日常门禁同口径，见根 AGENTS.md §8）。
- 首次构建需编译 DuckDB bundled（约 10 分钟）；若主仓 `spinduck/target`
  已有缓存，脚本自动经 `CARGO_TARGET_DIR` 复用，增量编译仅数秒。
- **无需** GaussDB 容器、无需任何外部环境变量。
- 探针依赖 PR #48 引入的 `spinduck_core::ir::set_oriented` 模块，
  **不能在主分支编译**——脚本在 PR head 的临时 worktree 中注入运行。

## 复现步骤

### 方式一：一键脚本（推荐）

```bash
# 仓库根目录下执行
bash golden/pr48_set_oriented_verify/run_verify.sh            # 只跑正确性差分
RUN_PERF=1 bash golden/pr48_set_oriented_verify/run_verify.sh # 追加性能基准（100k 轮约 3 分钟）
KEEP_WORKTREE=1 bash golden/pr48_set_oriented_verify/run_verify.sh  # 保留临时 worktree 供检查
PR_HEAD=<sha> bash golden/pr48_set_oriented_verify/run_verify.sh    # 覆盖被测 commit
```

脚本做的事：`fetch origin feat/set-oriented-v1-pr` → 在仓库同级目录建临时
worktree 并 checkout 被测 head（默认 `f62787d95fa82fb442147809978b0250ea0bb48c`）
→ `submodule update --init lib/ogsql-parser` → 把 `probe/zz_pr48_probe.rs`
注入 `spinduck/tests/` → 复用主仓 target 缓存 → 运行探针 → 退出时清理 worktree
（`KEEP_WORKTREE=1` 保留）。**不改动主工作目录。**

### 方式二：手工

```bash
git fetch origin feat/set-oriented-v1-pr
git worktree add --detach ../SpinDuck-pr48-verify f62787d95fa82fb442147809978b0250ea0bb48c
cd ../SpinDuck-pr48-verify
git submodule update --init lib/ogsql-parser
cp <repo>/golden/pr48_set_oriented_verify/probe/zz_pr48_probe.rs spinduck/tests/
cd spinduck
export CARGO_TARGET_DIR=<repo>/spinduck/target   # 可选：复用缓存
cargo test --features cli --test zz_pr48_probe -- --nocapture --skip probe_perf  # 正确性
cargo test --features cli --test zz_pr48_probe probe_perf -- --nocapture         # 性能
```

## 真实语料验证（纯 CLI，不写 Rust）

`corpus/` 子包用原始考题语料 SQL + 特殊项目 toml + `compile`/`deploy`/`call` 指令验证：

```bash
bash golden/pr48_set_oriented_verify/corpus/run_corpus_verify.sh
# 或复用已构建的 PR-head CLI：
SPINDUCK_BIN=/path/to/spinduck bash golden/pr48_set_oriented_verify/corpus/run_corpus_verify.sh
```

四个语料例程全部 `Skipped`、ON/OFF 业务输出逐字一致。方法论、口径（空种子确定性路径、
外部依赖处理）与实测见 [corpus/README.md](./corpus/README.md)。

## 100k 同源三方基准（Part A / Part B）

在 100k 同源数据下做端到端三方性能与一致性对比（含 openGauss 参照）：

```bash
# Part A：purchase 同形可转换过程（集合化触发，需参照容器）
docker compose -f golden/gaussdb-docker/docker-compose.yml up -d
bash golden/pr48_set_oriented_verify/bench/run_bench.sh

# Part B：原始 purchase 主过程（集合化 Skipped，无需容器）
bash golden/pr48_set_oriented_verify/bench_partb/run_partb.sh
N=1000 bash golden/pr48_set_oriented_verify/bench_partb/run_partb.sh   # 快速冒烟
```

- **Part A**（[bench/README.md](./bench/README.md)）：同形可转换过程三方各 3 轮，
  OFF 中位 ~83s / ON ~0.92s / 参照 ~1.36s，**OFF→ON ~90×**，三方目标表逐行一致（各 100k 行）。
- **Part B**（[bench_partb/README.md](./bench_partb/README.md)）：原始过程 100k 种子，
  ON 报 `Skipped(ExceptionScopePresent)`、ON≡OFF 逐行一致——真实过程零收益零分歧；
  参照因 V21（procedure CALL 崩溃）不可基线。
- 口径：debug 构建、CLI 端到端墙钟（含固定开销）；Part B 须 `--step-budget` 放大
  （十万行游标循环超默认 1M 步预算，脚本默认 100M）。详见 REPORT §9。

## 核验

把运行输出与 `reports/correctness.log`、`reports/perf.log` 对照：

- 正确性应为 `47 PASS + 3 CHAR`、`test result: ok. 4 passed; 0 failed`，零分歧。
- 性能方向：业务 `SqlExec` 由 OFF=N 降为 ON=1；规模越大加速比越高
  （100k 行 debug ~910×），约 25 行存在固定开销交叉点（小表 ON 反而更慢）。

绝对墙钟随机器/构建漂移，**只核验方向与量级**，不要求逐毫秒一致。
完整论断与边界矩阵见 [REPORT.md](./REPORT.md)。

## 已知口径

- 性能为 **debug 构建**绝对值，仅证明方向与量级，非 release 承诺。
- ON 侧含 2 次元数据查询（事务内约束重证，不入 trace），构成固定开销。
- 两处与设计文档 §8.6 的措辞差异（SELECT 小写引号列会 Converted；
  不支持类型标 `UnknownObjectDependency`）见 REPORT §7，均不影响等价性。
- 被测 head 与 CI 证据：`f62787d`，CI run `43674843043` 全绿。