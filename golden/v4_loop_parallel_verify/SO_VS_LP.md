# SO vs LP 性能矩阵报告（100k 量级 / 并行度 8）

> 对比 `set_oriented`（BatchAtomic）与 `loop_parallel`（Safe, threads=8）
> 在两个真实存储过程上的性能。**当前数据为分臂备选计划 + SO+LP
> 派发缺口修复后的复测轮**（`reports/perf_matrix_run2.log`，
> `v4_perf_*` 5 个矩阵用例，每配置预热 1 轮 + 计时 N 轮取最小值；
> purchase 3 轮 / byfund 2 轮）。首轮矩阵（分臂计划前）存档于
> `reports/perf_matrix_run.log`，差异见 §3.5。
>
> 配置含义：`off`=两者全关；`so_ba`=仅 set-oriented BatchAtomic；
> `lp_safe`=仅 loop-parallel Safe；`so_lp`=两者同开
> （run_selected 中 set-oriented 先裁决，LP 为二级降级通道）。

## 1. 结果矩阵（min_ms，越小越好）

| 场景 | 数据规模 | off | so_ba | lp_safe | so_lp | SO 加速比 | LP 加速比 | 最优配置 |
|---|---|---|---|---|---|---|---|---|
| purchase_base | ~9.6 万游标行（happy path, kind='0'） | 8564 | **2987** | 4997 | 3088 | **2.9×** | 1.7× | so_lp≈so_ba |
| purchase_branch_k0 | ~2 万派发行（P0 排除+P1/P2 MISS+P5） | 21091 | **2341** | 19028 | 2365 | **9.0×** | 1.1× | so_lp≈so_ba |
| purchase_branch_k1 | 同上 + `swh_all_kind='1'`（P4 臂） | 23377 | **3637** | 19800 | **3423** | **6.4×** | 1.2× | **so_lp** |
| byfund_base | 5000 分组（happy path） | 40794 | **3672** | 40376 | 3707 | **11.1×** | 1.0× | so_lp≈so_ba |
| byfund_branch | 5000 分组（B1-B5 混合臂+段2/3/4） | 43747 | 44977 | 43143 | 45737 | 0.97× | 1.01× | **lp_safe≈off** |

判定状态（`so=`/`lp=` 列，本轮实测）：

- `so_ba`：purchase_base/k0/**k1**、byfund_base → `Converted`
  （**k1 为分臂备选 THEN 臂计划**，§3.5——首轮为
  `Skipped:UnsupportedControlFlow`）；byfund_branch →
  `Skipped:AmbiguousTop1Tie`。
- `lp_safe`：五个场景全部 `Parallelized`（workers=8,
  fallback=0；dispatch 96006/20004/20004/5002/5005 行）。
- `so_lp`：SO `Converted` 的四场景 LP 报告为空（完全遮蔽）；
  byfund_branch SO `Skipped` → **LP 接管成功**
  （`Parallelized` 5005 行，派发缺口修复生效），但净耗时仍 ≈ off
  （守卫探测+前缀开销 vs 并行收益相抵）。

## 2. 结论

1. **set-oriented 在可转换场景全面优于 loop-parallel**：集合化把整个
   循环区域重写为单条 `INSERT...SELECT`（含 LATERAL 查找/聚合），
   DuckDB 一次执行完毕；LP 只并行纯前缀、INSERT/COMMIT/后缀仍逐行
   主线程串行。本轮实测 SO 比 LP 快 **1.7×（purchase_base）～
   11×（byfund_base）**，purchase_branch_k0/k1 达 **9.0×/6.4×**。
2. **分臂备选计划消除了 route-IF 的覆盖盲区**：purchase_branch_k1
   （`kind='1'` 臂）首轮 SO `Skipped`、最优仅 LP 1.25×；本轮 SO
   `Converted` **6.4×**（so_lp 3423ms 为全场最优）。LP 的兜底角色
   收窄到剩余不可转换形态——byfund_branch 的 `AmbiguousTop1Tie`
   运行态守卫拒绝由 LP 接管（`Parallelized` 5005 行），但该场景
   前缀浅+后缀串行占比高，净收益 ≈ 0（so_lp 45737 ≈ off 43747，
   守卫探测成本未完全收回）。
3. **LP 在异常/交权密集数据上收益为零甚至为负**：byfund 两场景
   LP 均 ≈ off（byfund_branch lp_safe 43143 vs off 43747 持平；
   首轮矩阵曾现 0.88×~0.95× 回退，本轮机器方差内持平）——段 1
   纯前缀计算量小、行级 INSERT+COMMIT+段 2/3/4 全串行，8 线程
   派发+快照探测+NeedKeys 回主线程补物化的开销被放大。
   purchase_branch_k0/k1 类似（LP 1.1×~1.2×，远低于 SO）。
4. **SO+LP 组合在 SO 可转换时与 SO 单开持平**（LP 被遮蔽，无额外
   开销）；在 SO 运行态守卫拒绝时 LP 接管（§3 派发缺口已修复，
   本轮 byfund_branch so_lp 的 `lp=Parallelized` 为实证）。

## 3. 派发缺口（已修复，复测数据如下）

**问题**：`run_selected` 只在 `select_plan` **计划期**失败时才把控制
权交给 `try_loop_parallel`；`adjudicate_region` 内 `entry_ok`
**运行态入口守卫**失败（`AmbiguousTop1Tie`、`UnsupportedControlFlow`
等）时，前缀与 `OPEN` 已执行，区域按原 PC 串行续跑——**LP 不再介入**。
矩阵轮实测 so_lp 比 lp_safe 还慢（55.9s vs 53.5s；26.2s vs 22.4s）——
付了 SO 计划+守卫探测成本却拿不到任何并行收益。

**修复**（`engine/set_oriented.rs` + `engine/loop_parallel.rs`）：
`adjudicate_region` 新增 `is_last`——仅末区域守卫运行态拒绝时调用
`try_loop_parallel_region`：SO 前缀已执行，LP 以 `ParallelEntry::
RegionOpen` 只补 `[open_pc, fetch_pc)` 的 OPEN 段，绝不重放前缀；
LP 区域 pc 与 SO 计划不一致或静态门禁/预热失败则返回 `None` 走
原串行续跑，语义零变化。配套精化 `prove_no_reentry`：交权边续跑
可达 `G_pure` **后代/并行分支**（IF 汇合点）不再误拒，仅祖先可达
才拒（`reentry_tests::should_admit_yield_merge_into_g_pure_descendant`）。

**复测**（修复后单独轮，同机同数据，`v4_perf_*` 各配置取 min）：

| 场景 | off | so_ba | lp_safe | so_lp | so_lp vs lp_safe |
|---|---|---|---|---|---|
| purchase_branch_k1 | 49920 | 36042 (Skipped) | 20555 | **21046** | +2.4%（≈持平） |
| byfund_branch | 43304 | 47392 (Skipped) | 45439 | **46468** | +2.3%（≈持平） |

两场景 so_lp 均 `Parallelized`（k1: 20004 行派发；byfund: 5005 行），
守卫拒绝后 LP 接管成功，派发缺口消除。绝对值与 §1 矩阵轮存在机器
方差（本轮负载更高），**轮内相对比较**有效：so_lp 从「最慢配置」
回到「≈lp_safe」。byfund_branch 仍 ≤ off（0.93×）——LP 前缀浅+
后缀串行占比高的固有属性，非派发问题。

## 3.5 分臂备选计划（route-IF 双臂转换，已实施）

**问题**：route-IF（`IF <不变量查找变量> = <常量>`）原 SO 模型只
转换 ELSE 臂，THEN 臂须由运行态守卫证明不可达——`kind='1'` 数据
使守卫失败 → `Skipped:UnsupportedControlFlow` → 整区域串行。

**机制**：`lookup::fallback_if_group_at` 返回结构化 `IfGroup`
（`cond`/`group`/`resume`/`then_range`/`else_range`/`resets`）；
`Body` 以 `WalkArm::{Primary,ThenArm}` 分别走查两臂——THEN 臂内
`SqlQuery` 经 `arm_query`（`parse_total` + reset 默认值）准入，
`finish_arm` 做未重置变量 dead 存储检查后登记备选查找组。
`analyze_region` 在主体 saw route-IF 时追加产出备选 `CopyLoop`；
`generate` 以 `PlanFlags{tail_commit,then_arm}` 生成第二份
`ConversionPlan`（其 route 守卫为主计划的**逆**），存于
`ConversionPlan.route_alt`——仅当备选无 view 物化前导且
`fn_preludes` 与主计划逐项全等（`same_fn_preludes`；INSERT 位
sd_bfN 的 distinct-over-sd_cur 按臂投影渲染，同名表内容可不同，
入口期只物化一份，不等即丢弃备选）才挂载。
`adjudicate_region` 先评主计划 `entry_ok`（route_guard 证明 THEN
不可达则主臂生效）；失败则评备选 `entry_ok`（`route::pick` 守卫
求值路由条件为真 + 备选自身守卫）——任一通过即 Converted，
双双失败回退原串行续跑。

**安全口径**：route 条件是循环不变量 → 每次调用至多一臂生效，
两计划不重复执行；备选臂内联调用的 OUT 写回由 `live_out_check`
+ `call_outs` 后缀读检查兜底；THEN 臂含不可翻译构造时备选生成
失败仅丢弃备选，主计划行为不变（`should_route_invariant_if_*`
既有断言保持绿）。

**实测**（矩阵轮 `perf_matrix_run2.log`）：purchase_branch_k1
so_ba **3637ms Converted**（THEN 臂备选计划，生成 SQL 含
`fnc_com_get_seat_para` 内联→sd_lk7-10 查找链→14 路 CASE），
so_lp 3423ms（SO 遮蔽 LP），vs off 23377ms **6.4×**——首轮矩阵
（`perf_matrix_run.log`）同场景 so_ba=Skipped 27347ms、最优仅
lp_safe 22351ms（1.25×）。k0 对照组仍走主计划（ELSE 臂）
Converted 2341ms，双臂按数据各取其一、互不干扰。正确性由
`copy_both_arms` fixture（route='1'/'0' 双臂均 Converted+行等价）
与 8 个 `v4_*_equiv` 用例锁定。

## 4. 口径与限制（如实声明）

- **byfund 未达 10 万分组**：5000 分组 ≈ 5×2500 明细行克隆，
  off 单轮 ~42-47s；10 万分组外推 off ≈ 14-16 分钟/轮，
  4 配置 × 3 调用 ≈ 3 小时，不具可行性。purchase 达 ~9.6 万游标行
  为真 100k 量级。
- **计时口径**：`call()` 全程墙钟（含例程前后缀），每配置取
  预热后多轮最小值；SO 侧 `plan()` 静态证明为**每引擎一次性**
  成本（计入预热轮，不计入 min_ms）——byfund SO 证明耗时可达
  分钟级，生产环境应经 `attach_set_oriented_plans` 离线计划摊销。
- **机器方差**：purchase_base 的 lp_safe 在独立轮为 3839ms、矩阵轮
  为 6211ms（同数据同时段其他负载干扰）；报告取矩阵轮同会话值，
  加速比结论不受单点抖动影响（SO/LP 差距一个数量级）。
- **正确性**：矩阵内每配置断言 OUT 形参与 off 全等；状态等价由
  `v4_*_equiv`/`v4_*_branch_*` 用例另行锁定（本矩阵不重复快照）。
- `Strict` 模式未单列：与 BatchAtomic 共用同一集合 SQL 直发路径，
  差异仅在提交快照语义（`batch_commit` 互斥规则），性能同量级。

## 5. 复现

```bash
cd spinduck
cargo test --features native --test zz_v4_loop_parallel_probe v4_perf -- --nocapture --test-threads=1
# 约 20-30 分钟；V4PERF| 行即本报告数据源
# 当前矩阵 = reports/perf_matrix_run2.log（分臂计划+派发修复后）；
# 首轮基线 = reports/perf_matrix_run.log（分臂计划前）
```

探针源码：`probe/zz_v4_loop_parallel_probe.rs`（`perf_matrix`/
`perf_one` + `v4_perf_{purchase_base,purchase_branch_k0,
purchase_branch_k1,byfund_base,byfund_branch}`）。

## 6. 更新：SO 行级混合（Exception-Row Carve-out，详见 docs/.../so-exception-row-hybrid.md）

byfund_branch 的 `AmbiguousTop1Tie` 根因查明为**基数守卫**（B5 种子
`par_sys_bond` 双行 → 串行 `TOO_MANY_ROWS` → handler 副作用），非
Top-1 并列。新增混合执行：dup 守卫改产异常行号集（`exc_guards`），
正常行按 `__sd_rn` 行段批写，异常行 `seek_before`+真实 Fetch 单行
重放；逃逸/事务证明（`hybrid::analyze`）不满足时退回普通 SO。

**全量验证**：`run_verify.sh` 16/16 通过（767s，6 个性能矩阵并行
跑、计时含并行干扰，绝对值偏高、相对结论有效）；数据源
`reports/probe_run.log`。

| 场景 | off | so_ba | lp_safe | so_lp | 备注 |
|---|---|---|---|---|---|
| purchase_base | 13641 | **5408** Converted | 6215 | **4021** | 混合计划形态，exc=∅ → 纯批路径 |
| purchase_branch_k0 | 32992 | **3089** Converted | 27295 | **3063** | 同上 |
| purchase_branch_k1 | 35813 | **4703** Converted | 30353 | **4076** | 分臂 THEN 计划 |
| byfund_base | 70214 | **5902** Converted | 48844 | **4331** | exc=∅ → 纯批路径 |
| byfund_branch | 72983 | 56466 **Converted(混合)** | 42092 | 50658 | exc>0：段批写+逐行重放 |

判定状态变化：**byfund_branch so_ba `Skipped{AmbiguousTop1Tie}` →
`Converted`（混合路径）**，全配置 OUT 与 Off 基线逐值相等
（perf_matrix 内断言 + `v4_byfund_branch_*` 等价用例）。

性能结论（诚实口径）：

- exc=∅ 场景（purchase 全部、byfund_base）混合计划退化为纯批路径，
  性能与历史 Converted 持平（so_lp 3.1s~5.4s 量级）。
- byfund_branch 混合 SO 56.5s < off 73.0s 但 **慢于 lp_safe 42.1s**：
  异常行占比高时段数≈|E|+1、每段重跑游标 GROUP BY + 逐行重放解释
  开销大。该场景 LP 仍是最优路径；混合 SO 的价值是**正确性完备**
  （此前整体拒绝的区域现在可转换且结果全等），不是该种子上的提速。
- 已知边界 R7（详设 §5）：重放行触发 Kernel 域错误被 handler 捕获
  时 `reset_session` 可能丢未提交段写——Kernel 域=引擎故障面，
  BIZ 域异常不受影响。
