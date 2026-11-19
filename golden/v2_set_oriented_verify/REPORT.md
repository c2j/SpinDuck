# 集合化 V2 能力验证报告 —— 真实 purchase 主过程 100k 三方基准

- 验证对象：`feat(set-oriented): 集合化 V2 离线候选编译、区域证明与条件提交`（`976dfec`，main `9946a2b`）
- 参照方法：`golden/pr48_set_oriented_verify`（PR #48 V1 差分探针 + 100k 三方基准）
- 被测过程：`pkg_split_trade_step3_sh.prc_trd_deal_purchase`（`golden/case_corpus_purchase` 真实语料逐字）
- SpinDuck 版本：0.2.2，debug 构建，DuckDB 内核 1.5.5
- 日期：2025-11-18

## 1. 结论

| 维度 | 结果 |
|---|---|
| V2 能否转换真实 purchase 主过程 | **能**。运行期证据齐备 + `batch-atomic` 下 `SET_ORIENTED … converted` |
| Off→On 性能 | **≈97×**（380.2s → 3.9s，debug 构建端到端墙钟） |
| SP 存过→On | **≈38×**（148.7s → 3.9s；参照为 openGauss 原生 CALL） |
| 数据一致性 | **三方全等**：Off / On / Strict / openGauss 参照均 100000 行、27 列，排序逐行相等，`p_o_succeed=0` |
| Strict 模式 | `Skipped(TransactionBoundaryInsideLoop)` → 安全回退，输出与 Off 一致（362.2s） |

## 2. 三方定义与执行形态

| 方 | 命令/形态 | 集合化判定 |
|---|---|---|
| **SP** | openGauss 7.0.0-RC3 `CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase('20250715', NULL)` | 原生 plpgsql 逐行 RBAR |
| **SpinDuck Off** | `spinduck call --set-oriented=off …` | 解释器逐行 FETCH+INSERT |
| **SpinDuck On** | `spinduck call --set-oriented=batch-atomic …` | V2 集合化批写（Converted） |
| SpinDuck Strict | `--set-oriented=strict` | Skipped→回退（见 §5） |

On 侧集合化报告原文（`reports/v2_on_call.txt`）：

```
SET_ORIENTED pkg_split_trade_step3_sh.prc_trd_deal_purchase converted
OUT p_o_succeed = 0
```

## 3. 测量条件

- **数据**：`mid_yjqs_detail` 100000 行，SpinDuck 侧 `generate_series` 生成 → `COPY TO` CSV → 参照容器 `\copy` 载入，两侧同源逐字一致（源表 sha256 `529abd68…`，两侧 count 均 100000）。参数表用 `cases/datasets/seed.sql` 原样 INSERT。
- **行数规模**：源 100000 行全部命中游标过滤与 F001 资金路径，目标表 `dat_trd_repurchase` 产出 100000 行。
- **计时口径**：SpinDuck 侧为 `spinduck call` 进程端到端墙钟（python `perf_counter`），含 CLI 启动与 noop 固定开销 ~246ms；SP 侧为 `docker exec … gsql -f call.sql` 客户端墙钟，含连接建立。两侧口径不同，绝对值仅供方向与量级参照。
- **构建**：debug（`cargo build --features cli`），未做 release 优化；相对倍数不受 debug/release 影响结论，但绝对值偏保守。
- **轮数**：Off/Strict 单轮（百秒级成本），On 4 轮取中位 3913.9ms（样本 3891.8 / 4004.9 / 3855.5 / 3935.9）。
- **step-budget**：`--step-budget 100000000`（解释器逐行 100k 行超出默认预算）。

## 4. 性能结果（100k 行）

| 方 | 墙钟 | 说明 |
|---|---|---|
| SP（openGauss 7.0.0-RC3） | 148738 ms | gsql 客户端墙钟 |
| SpinDuck Off | 380222 ms | 解释器逐行，单轮 |
| SpinDuck Strict | 362156 ms | Skipped→回退，与 Off 同量级（符合预期） |
| **SpinDuck On（batch-atomic）** | **3913.9 ms（中位）** | V2 Converted |

加速比：

- **Off → On ≈ 97.1×**（380222 / 3913.9）
- **SP → On ≈ 38.0×**（148738 / 3913.9）
- Off 解释器较原生存过慢 ≈2.6×，属预期（解释执行 vs 编译态 plpgsql）；集合化后反超原生存过 ≈38×。

## 5. 集合化判定与模式差异

- **`batch-atomic`：Converted**。真实 purchase 主过程含游标、子调用、事务边界，V2 在运行期证据齐备时完成区域证明并批量提交。
- **`strict`：Skipped `TransactionBoundaryInsideLoop`**。过程体内存在 `COMMIT`，strict 模式要求严格事务等价，保守拒绝并安全回退原解释路径——输出与 Off 逐行一致（已验证），仅损失性能。这是设计内的保守行为，非缺陷。
- **证据完备性敏感**：离线 `compile --analyze` 的 codeweb 计划不含视图文件，最初报 `UnknownObjectDependency`；将 `view_check_para.sql` 补入计划 `ddl_tables` 并经 `deploy` 装配后，运行期证明通过。报告须区分"证据不全被跳过"与"真实转换"——本报告 On 侧为真实 Converted。

## 6. 数据一致性

`dat_trd_repurchase` 27 列全列排序逐行比对（`reports/consistency.log`）：

- Off JSON 与 On JSON **字节级 sha256 相同**（`e2b96153…`），各 100000 行。
- Strict（回退路径）输出与 Off 一致。
- openGauss 参照导出 CSV 与 SpinDuck 侧在 NULL/空串归一化后逐行相等。
- `OUT p_o_succeed = 0` 三方一致。

## 7. 口径与限制

1. **参照版本**：本次 SP 参照用 openGauss **7.0.0-RC3**（`sd-og70` 容器）。历史 golden 用 5.0.0，但 V21 已记录 5.0.0 amd64 在存过 `CALL` 下 postmaster 自杀；7.0.0 为 validation.md V26.5 的替代参照口径。**与 PR #48 历史 5.0.0 数字不可直接比较**。
2. **参照侧 stub**：`STUB_pkg_split_trade_log.sql` 为简化桩（真实日志包不在语料内），两侧使用同一桩。
3. **计时边界**：SpinDuck 计时含 CLI 启动固定开销（~246ms），SP 计时含 gsql 连接开销；倍数结论稳健，绝对值不宜跨口径引用。
4. **debug 构建**：绝对耗时为开发机 debug 值；97×/38× 为同二进制内 Off↔On 与跨引擎量级对比。
5. **On 轮次**：Off/Strict 各单轮（单次 ~6min），On 4 轮中位；如需更严谨可多轮 Off。
6. **复现**：`bash golden/v2_set_oriented_verify/run_bench.sh`（`N=100000` 默认；`OG_CONTAINER`/`OG_LIBS` 启用 SP 参照；`N=1000` 快速冒烟）。

## 8. 与 PR #48（V1）的对照

| | PR #48 Part B（V1） | 本次（V2） |
|---|---|---|
| 真实 purchase 主过程 | Skipped（准入面外） | **Converted**（batch-atomic） |
| Off≈On | 是（无加速） | **97×** |
| SP 参照 | 无（5.0.0 CALL 不稳定） | openGauss 7.0.0-RC3，148.7s |
| 一致性 | —（未转换） | 三方 100k 行全等 |

V2 的区域证明 + 条件提交（BatchAtomic）使原先被拒的真实过程获得集合化收益，同时 strict 模式对事务边界保持保守回退，语义安全。
