# PR #48 集合化 —— 原始 purchase 主过程 100k 边界实证（Part B）

与 [Part A](../bench/README.md)（purchase **同形可转换**过程）互补：本目录跑
**真实语料原始过程** `pkg_split_trade_step3_sh.prc_trd_deal_purchase`，在 100k
同源种子下证明集合化的**准入边界**与**零收益零分歧**。

## 核心结论

| 方 | 100k 墙钟 (ms) | 集合化判定 | 目标行数 |
|---|---|---|---|
| SpinDuck OFF（解释器游标 RBAR） | 429536 | feature 关，无判定行 | 100000 |
| SpinDuck ON（`set_oriented=true`） | 469232 | **skipped ExceptionScopePresent** | 100000 |
| openGauss 原生参照 | **不可基线（V21）** | — | — |

1. **原始过程不转换**：含异常域 / 子调用 / 多写 / REF CURSOR → 编译期 IR 判定
   `Skipped(ExceptionScopePresent)`。集合化在真实生产语料上**不触发**。
2. **Skipped ⇒ ON≡OFF**：墙钟同量级（差异为 debug 测量噪声），目标表
   `dat_trd_repurchase` 排序后逐行一致，业务输出逐字一致——**零收益、零分歧**。
3. **判定与数据量无关**：Skipped 是编译期结构分析，100k 与空种子判定相同。
4. **参照限制（V21）**：原始过程是 PACKAGE PROCEDURE，须经 `CALL`；amd64
   postmaster 在 procedure `CALL` 上崩溃（validation V21），故参照**无法**对完整
   原始过程基线。本脚本不尝试参照 `CALL`（避免崩溃容器）。Part A 参照用
   `FUNCTION`（非 `CALL`）不受此限。

## 文件清单

```
bench_partb/
├── README.md                 ← 本文件
├── run_partb.sh              ← 一键边界实证（构建/复用 CLI → 原始语料工程 → 100k 种子 → ON/OFF → 断言）
└── reports/
    ├── partb_summary.log     ← 性能 + 一致性 + 结论 + 口径
    ├── partb_on_call.txt     ← ON 调用输出（含 SET_ORIENTED skipped）
    └── partb_off_call.txt    ← OFF 调用输出（无判定行）
```

## 方法论

1. **原始语料逐字**：`golden/case_corpus_purchase/sql` 原样复制进临时工程，
   特殊 `spinduck.toml`（`set_oriented` 开关）+ `codeweb.toml` 标记，
   `compile --analyze` 取图 → 剥离外部依赖 `[[missing]]`（`fnc_com_get_seat_para`）→ `deploy`。
2. **确定性路径，无需伪造桩**：种子取 `swh_all_kind.kind_id='0'` →
   `v_kind='0'` → `IF v_kind='1'` 分支跳过 → 运行时**不调用** `fnc_com_get_seat_para`。
   这是语料黄金口径本身（见 `cases/datasets/seed.sql` 头注），非为本基准伪造。
3. **100k 同源种子**：`generate_series` 造 100k 行 `mid_yjqs_detail`，全部命中游标
   过滤（`stock_kind '05%'≠'0509'`、`cjsl>0`、`scdm='001'`、`bcrq='20250715'`）
   且匹配 `par_fund_partner`/`par_fund_seat` 的 F001 资金路径与
   `par_sys_purchase('0130000001')`，确保每行真正被处理并写入目标。
4. **ON/OFF 双跑**：每方先 `DELETE` 目标再 `call`，捕获判定行 / 业务输出 / 端到端墙钟，
   导出目标表全列。断言：ON 报 `skipped ExceptionScopePresent`、OFF 无判定行、
   ON 去判定行后与 OFF 逐字一致、目标排序后逐行一致且均 100000 行。
5. **步数预算**：十万行游标循环合法超过默认 `DEFAULT_STEP_BUDGET=1_000_000`
   （`runtime/interp.rs:36` 注释明示须按调用面放大），故 `call` 传
   `--step-budget`（脚本默认 `STEP_BUDGET=100000000`）。不放大则 100k 调用
   报 `step budget exceeded` 退出 1——这是引擎防死循环的保护，非缺陷。

## 复现

```bash
# 仓库根目录下执行（SpinDuck 侧无需 GaussDB 容器）
bash golden/pr48_set_oriented_verify/bench_partb/run_partb.sh
SPINDUCK_BIN=/path/to/spinduck bash ...   # 复用已构建 PR-head CLI
N=1000 bash ...                           # 快速冒烟（默认 100000）
KEEP_WORKTREE=1 bash ...                  # 保留临时目录
```

## 口径与边界

- **单轮测时**：100k 单趟约数分钟（debug 实测边际约 4–5ms/行），ON/OFF 两趟约
  15–20min；多轮成本过高，故各 1 轮。Skipped ⇒ ON≡OFF，墙钟差异是噪声而非收益。
- **debug 构建**：绝对值仅证明方向与量级，非 release 承诺。
- **目标表是 `dat_trd_repurchase`**（主过程 `prc_trd_deal_purchase` 的写入对象），
  不是 `dat_fund_cjqs`（后者属包内另一过程 `prc_trd_hz_byfund`）。
- 本实证证明的是「**真实过程落在准入面外**，集合化对其零作用」；要获得 Part A
  那样的收益，过程须落在准入面内（纯列投影搬运，无异常域/子调用/多写）。