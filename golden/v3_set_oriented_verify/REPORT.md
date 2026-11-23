# PR #59 集合化 V3 实战复现报告（purchase / byfund × 存过 / 非集合化 / 集合化）

被测：PR #59 head `a33503597f149737175ee2df23abc77dffecd525`（`feat/set-oriented-v3`）。
取证：本机 release CLI + 本机 openGauss 参照容器，全部命令由本包脚本产出，
日志归档在 [`reports/`](./reports)。

## 1. 结论摘要

| | purchase | byfund |
|---|---|---|
| 集合化判定（ON） | `Converted` | `Converted` |
| OFF/ON 全库状态一致 | **是**（sha `535a8dc6…` 相同） | **是**（sha `9d9afdcf…` 相同） |
| 与存过侧跨引擎一致 | **是**（`STATE_EQUAL dat_trd_repurchase`） | **是**（4 张表全部 `STATE_EQUAL`） |
| 100K 加速比 OFF→ON | **95.3×**（51,940.5 → 545.1 ms） | **55.1×**（124,044.7 → 2,250.0 ms） |
| 内核语句数 OFF→ON | 600,001 → 2（30 万倍） | 700,028 → 31（22,582 倍） |
| vs openGauss 存过 | SP 144.9 s，OFF 慢 2.8×，ON 快 **266×** | SP 3,730.2 s，OFF 慢 30.1×，ON 快 **1,658×** |

一句话：**语义等价达成、集合化收益达成**，两条真实考题过程都从 PR #48 的
`Skipped` 进到 `Converted` 并在 100K 上拿到两个数量级的收益；相对原生存过，
OFF 侧仍慢（purchase 2.8×、byfund 30.1×），ON 侧反超（266× / 1,658×）。

## 2. 被测版本与取证环境

| 项 | 值 |
|---|---|
| 被测 commit | `a335035`（PR #59 head，`gh pr view 59` 核对） |
| CI 证据 | run `44967040478`：format / clippy / test / audit / golden 全部 success；`mergeStateStatus=CLEAN` |
| SpinDuck CLI | `cargo build --release --features cli` → `spinduck 0.2.3`，二进制 mtime 晚于全部源码（构建于 09:15，commit 时间 08:31） |
| 内核 | DuckDB 1.5.5（bundled，`[project].kernel = "1.5.5"`） |
| 语料工程 | PR head worktree 的 `golden/case_corpus_purchase`（含 byfund 依赖资产） |
| 参照实例 | 容器 `sd-og70`：`openGauss-lite 7.0.0-RC3 build 01b7e318`（x86_64 镜像在 Apple Silicon 上模拟），`DBCOMPATIBILITY 'A'`，参照库零二级索引。**不是** CI golden job 的 `golden/gaussdb-docker`（openGauss 5.0.0）：V21 记录的 procedure CALL 崩溃在 7.0.0-RC3 上不复现（V26.5 替代参照口径），存过侧计时因此才可用，但数字不作 golden 基线复算 |
| 驱动数据 | `sql/gen_purchase.sql` / `sql/gen_byfund.sql`，`N=100000`（两侧同一模板、同一 sed） |

完整取证条件（宿主 macOS 26.2 arm64、docker 29.9.2、二进制 mtime 与新鲜度判据、
参照库名与凭据口径）见 [reports/PROVENANCE.md](./reports/PROVENANCE.md)。

## 3. 数据一致性

**同库 OFF/ON（业务面逐值）**：每个模式独立 `deploy` + 同一份种子 + 同一份生成数据，
跑完后把全库每张基表逐行排序取 sha256。

| 数据集 | N | OFF sha | ON sha | 判定 |
|---|---|---|---|---|
| purchase | 100,000 | `535a8dc61cc49d8a…6749723d` | 同左 | `state_equal: true` |
| byfund | 100,000 | `9d9afdcf4f84c424…cb7d2afb` | 同左 | `state_equal: true` |
| purchase | 2,000 | `19794564814297c9…fbc9506f` | 同左 | `state_equal: true` |
| byfund | 2,000 | `2bfb7752d8db21dc…3f857b9a` | 同左 | `state_equal: true` |
| purchase | 0 | `af869824…`（两侧同） | 同左 | `state_equal: true` |

产物行数同时佐证「不是双双空跑」：purchase `dat_trd_repurchase = 100,000`；
byfund `dat_fund_cjqs = 100,001`、`dat_fund_xw_venture = 2`。100,001 = 10 万分组
+ 种子 2 行明细并入首个分组。两侧 `OUT p_o_succeed = 0`（业务成功出口）。

**跨引擎（SpinDuck ON 产物 vs openGauss 存过产物）**：逐列聚合比对。

```
EQ   dat_trd_repurchase batch0 sd=[100000.0, 1.0, 'F001', 'F001', 1.0, 130000001.0] ref=[同]
EQ   dat_trd_repurchase batch1 sd=[0.0, None, None, 1050000000.0, 10500.0, 10500.0] ref=[同]
EQ   dat_trd_repurchase batch2 sd=[0.0, 0.0] ref=[同]
STATE_EQUAL ['dat_trd_repurchase']
```

100,000 行、含 NULL 分布与 `SUM/MIN/MAX` 全等——集合化产物与原生存过产物同形。
byfund 侧四张表同样全等（`reports/ref_cmp_byfund_100000.log`）：

```
EQ   dat_fund_cjqs        batch0 sd=[100001.0, 2.0, 'X001', 'X002', 100001.0, 130000000001.0] ref=[同]
EQ   dat_fund_cjqs        batch2 sd=[None, None, 0.0, None, None, 0.0] ref=[同]
EQ   dat_fund_xw_venture  batch0 sd=[2.0, 2.0, 'F001', 'F002', 2.0, 'X001'] ref=[同]
EQ   qfqscjhbrbb_temp     batch0 sd=[1.0, 1.0, 'A0001', 'A0001', 1.0, 'S0001'] ref=[同]
EQ   db_log               batch0 sd=[0.0, 0.0, None, None, 0.0, None] ref=[同]
STATE_EQUAL ['dat_fund_cjqs', 'dat_fund_xw_venture', 'qfqscjhbrbb_temp', 'db_log']
```

行数也逐张对齐：`dat_fund_cjqs` 100,001、`dat_fund_xw_venture` 2、
`qfqscjhbrbb_temp` 1、`db_log` 0，SpinDuck 与存过侧完全相同。

## 4. 性能三方（release 构建，100K）

| 侧 | purchase | byfund |
|---|---|---|
| openGauss 存过（服务端 `clock_timestamp()`） | 144,850.1 ms（客户端整段 145.4 s；首轮 168,582.6 ms） | 3,730,226.6 ms（客户端整段 3,730.8 s） |
| SpinDuck `off`（CLI 进程墙钟） | 51,940.5 ms | 124,044.7 ms |
| SpinDuck `batch-atomic`（CLI 进程墙钟） | 545.1 ms | 2,250.0 ms |
| OFF→ON | **95.3×** | **55.1×** |
| SP/OFF | 2.8×（首轮 3.2×） | 30.1× |
| SP/ON | 266×（首轮 309×） | 1,658× |

规模效应（同一次 `a335035` 二进制）：

| N | purchase OFF | purchase ON | 倍数 | byfund OFF | byfund ON | 倍数 |
|---|---|---|---|---|---|---|
| 0 | 21.9 ms | 158.1 ms | 0.14×（ON 更慢） | — | — | — |
| 2,000 | 1,061.3 ms | 227.1 ms | 4.7× | 2,472.0 ms | 592.9 ms | 4.2× |
| 100,000 | 51,940.5 ms | 545.1 ms | 95.3× | 124,044.7 ms | 2,250.0 ms | 55.1× |

`N=0` 那一行给出**集合化侧固定成本 ≈ 136 ms**（元数据/事务内约束重证等），
交叉点在数百行量级：规模越大收益越大，小表上 ON 反而更慢。参照侧
`restarts=0`、`StartedAt` 未变，procedure 执行未触发 V21 记录的崩溃面。

**两轮复测散布**：100K 全流程独立跑了两轮（`reports/bench_*_100000.log` 为第二轮
归档），两轮的产物 sha 完全相同，墙钟散布 ≤7%：

| | 第 1 轮 | 第 2 轮（归档） | 差 |
|---|---|---|---|
| purchase OFF | 54,486.5 ms | 51,940.5 ms | -4.7% |
| purchase ON | 583.5 ms | 545.1 ms | -6.6% |
| byfund OFF | 127,193.3 ms | 124,044.7 ms | -2.5% |
| byfund ON | 2,244.9 ms | 2,250.0 ms | +0.2% |

机制计数两轮完全相同（600,001→2、700,028→31），说明轨迹本身是确定的。

## 5. 机制证据：为什么快

`--trace-level all` 的事件直方图（`reports/cnt_*_100000.log`）：

| 事件 | purchase OFF | purchase ON | byfund OFF | byfund ON |
|---|---|---|---|---|
| SQL_EXEC（内核语句往返） | 600,001 | **2** | 700,028 | **31** |
| FETCH | 100,001 | 0 | 100,009 | 8 |
| CURSOR_OPEN | 1 | 0 | 5 | 4 |
| IR_STMT（解释器步进） | 5,300,026 | 24 | 4,600,230 | 242 |
| 侧车去重语句数 | 8 | 2 | 32 | 26 |

purchase 的循环体每行 6 条内核语句 + 1 次 fetch ⇒ N=100K 时 60 万条；集合化把整个
游标循环压成 1 条聚合 + 1 条写回。byfund 每组 7 条 ⇒ 70 万条压到 31 条
（分组内 top-1 查找被批量话，剩余 31 条是循环外的既有语句）。这与 §4 的墙钟
倍数同向，说明收益来自 RBAR 消除而不是缓存抖动。

## 6. 判定链与限制

1. **byfund 的收益是数据依赖的**。集合化入口守卫在 top-1 探测遇到歧义时退回
   `Skipped(AmbiguousTop1Tie)`；`sql/gen_byfund.sql` 因此必须同时把每张证券写进
   `par_sys_bond`，`sql/byfund_params_extra.sql` 补齐 6 处外部依赖。缺任何一件，
   两侧都走不到业务成功出口，收益也不成立。这是**准入条件**，不是普适承诺。
2. **计时基准不同**：SpinDuck 是 CLI 进程整段墙钟，存过侧是 postmaster 内部
   `clock_timestamp()` 差值（不含连接与 gsql 启动）。SP 与 SpinDuck 的对比
   只读量级，不读作同一把尺。
3. **参照侧偏慢的上界因素**：x86_64 镜像在 Apple Silicon 上模拟运行；参照夹具按
   语料 DDL 建表、**零二级索引**（`pg_indexes` 计数 0），`v_par_bond` 八表 UNION ALL
   的逐行查找被放大。因此「ON 反超存过 266× / 1,658×」在这一环境下是**上界**。
4. **采样轮次**：SpinDuck 侧跑了 2 轮（墙钟散布 ≤7%，机制计数与产物 sha 完全一致）；
   存过侧 purchase 跑了 2 轮（144.9 s / 168.6 s，散布 16%，机器同期负载不同）、
   byfund 跑了 1 轮（单轮 62 分钟，整机占用约 1 小时，这是本包最贵的一步）。
5. **参照侧的可重跑性**：存过侧长跑被客户端中断时，服务端可能已按内部 `COMMIT`
   落了部分产物，参照库即不再干净——必须换新 `REF_DB` 重投，不能在同一库续跑。
   `reference/sp_deploy.sh` 因此在 `mid_yjqs_detail` 非空时拒绝再灌。
6. **语料依赖是 byfund 可复现的前提**：在未合并 PR #59 的主仓语料上直接编译
   byfund 会得到 `missing=6 failed=1`（`fnc_get_stock_kind` 等桩缺失）。本包的
   `run_verify.sh` 因此自动挑选「含 byfund 资产」的语料树，而不是静默用主仓副本。

## 7. 勘误（对上一轮评审意见的更正）

1. 我在复审 `a335035` 时说过「已在最新 head 上独立复跑 100K 基准」——**当时不成立**：
   那批复用自 2026-11-21 22:47 构建的二进制，早于 `a335035`（08:31 提交），
   即 `48482c6` 之前的 head。本报告的 §3–§5 全部数字改由 `a335035` 重新构建后
   实测，口径以 `reports/*_100000.log` 为准。
2. 换二进制后**产物 sha 完全一致**（purchase `535a8dc6…`、byfund `9d9afdcf…`），
   说明 `a335035` 的重构未改变业务语义。
3. 同口径下 OFF 侧耗时下降：byfund 178,094.2 ms（a335035 之前的构建）→
   124,044.7 / 127,193.3 ms（a335035 两轮，-28.6%~-30.3%）；purchase
   55,240.4 → 51,940.5 / 54,486.5 ms（-1.4%~-6.0%）。归因于游标声明作用域恢复
   （`DeclRestore::Cursor`）省掉每轮重复登记的开销——只在 byfund 这种循环体复杂的
   过程上显著。ON 侧 632.0 → 545.1 / 583.5 ms、2,432.4 → 2,250.0 / 2,244.9 ms
   属同量级波动。

## 8. 复现

见 [README.md](./README.md)。最短路径：

```bash
N=2000 bash golden/v3_set_oriented_verify/run_verify.sh              # 冒烟
bash golden/v3_set_oriented_verify/run_verify.sh                     # 100K 两侧 + 机制
WITH_REFERENCE=1 REF_ROUNDS=1 bash golden/v3_set_oriented_verify/run_verify.sh   # 追加存过侧
```

核验点：每个数据集 `VERIFY_PASS` 且 `"state_equal": true`、ON 侧 `Converted`、
`SQL_EXEC` 由十万级降到十级；跨引擎末行 `STATE_EQUAL`。绝对墙钟随机器漂移，
只核验方向与量级。
