# SO 行级混合执行（Exception-Row Carve-out）详设

> 状态：已实现（Slice A–D 初验通过，性能复测中）。动机：`byfund_branch` 实测 `Skipped{AmbiguousTop1Tie}`
> 的根因不是 Top-1 ORDER BY 并列，而是**基数守卫**（`vguard.rs`，复用
> `top1_guards` 槽位）检出"游标行关联维表多行命中 → 串行 `SELECT INTO` 必抛
> `TOO_MANY_ROWS`"的行（种子 B5：`par_sys_bond` `0130000002` 双行 inure 重叠）。
> SO BatchAtomic 是单条集合 SQL，无法逐行复现异常与 handler 副作用 → 全区域
> 拒绝。本文设计"正常行集合化 + 异常行串行重放"的混合执行。

## 1. 问题定义与语义合同

串行语义（对每个游标行 i，按游标序）：

```
row_i: 前缀语句 → [某 SELECT INTO 多行命中 → TOO_MANY_ROWS]
       → WHEN OTHERS handler（inst_log / UPDATE / GOTO，任意副作用）
       → （GOTO error_row 类跳转跳过本行剩余体）→ 下一行
```

混合执行目标：**逐行结果与串行全等**（OUT、用户表行集、trace 序、事务可见性、
步数预算口径），同时把"无异常行"的绝大多数工作集合化。

### 1.1 必须保序的资源

| 资源 | 串行顺序 | 混合下的保序手段 |
|---|---|---|
| 用户表写（INSERT/UPDATE/DELETE） | 行序交错 | 按游标行序分段执行：批段与单行段严格交替 |
| COMMIT / 事务快照 | 行内提交点 | 见 §4.3——沿用 SO `tail_commit`/`batch_commit` 既有口径 + 单行段原样解释 |
| trace 事件序 | 逐行 `SqlExec`/`IrStmt` | 批段沿用 SO 单条 `SqlExec` 埋点；单行段原样逐语句埋点（混合 trace 形态需文档化） |
| 步数预算 | 每语句 +1 | 批段计 1（SO 既有口径）；单行段按解释器原计——混合计数 ≠ 串行 ≠ 纯 SO，须定义并文档化 |
| 异常/handler 副作用 | 行内即时 | 单行段内由解释器原生 trap 分派，天然一致 |

### 1.2 行序一致性的关键假设（须实证+守护）

`s d_cur` 是**CTE 查询重执行**（`sd_cur AS (<cursor SQL>)`），而单行重放用的是
OPEN 时物化的 `RowStream`（`Arc<Vec<Row>>`）。两者行集相等已由 SO 计划期证明；
**行序一致是新假设**——GROUP BY/无 ORDER BY 游标的输出序依赖 DuckDB 执行计划，
同一查询同一数据两次执行实践中确定一致，但无规范保证。对策：

- 守卫/批写/重放统一以 **`sd_cur` 物化为唯一行序权威**：入口把异常行的
  `sd_cur` 行号物化为临时表 `sd_exc(rowid)`；单行重放不按游标流位置寻址，
  而是按"该行在 `sd_cur` 中的序号"回放对应游标行——前提是证明
  `sd_cur` 序 == 游标流序（§5 风险 R1）。
- 若不能证明序一致 → 整机制 `Skipped`（回退现状）。

## 2. 机制总览

```
入口（adjudicate_region，守卫全过之后、批写之前）：
  1. 异常行探测守卫（现有基数守卫改造）：不再 COUNT>0 即拒，
     而是把违规行键/行号物化为 TEMP TABLE sd_exc。
  2. 若 sd_exc 为空 → 现行单批路径（零开销分支）。
  3. 若 |sd_exc| > 阈值（建议初值 5% 行数或绝对值上限）→
     放弃混合，走现有全区域串行续跑（异常行太多时混合无收益）。
  4. 否则进入分段混合执行（§3）。
```

## 3. 分段混合执行协议

设游标行序为 `0..N-1`，异常行集合 `E = {e1 < e2 < ... < ek}`（按行序）。

```
prev = 0
for each e in E ∪ {N}:            # N = 虚拟哨兵
    if e > prev:
        批段 [prev, e)：
          执行 plan.sql 的分段变体——sd_cur CTE 追加
          `WHERE __rn >= prev AND __rn < e`（__rn 由 sd_cur 扫描注入）
          → INSERT...SELECT 只产出该段行；尾提交口径同现行
    if e < N:
        单行段 e：游标 seek 到第 e 行 → 解释器执行一行循环体
                    （§3.1）→ Stopped 后继续
    prev = e + 1
```

- 批段复用**同一 `ConversionPlan.sql`**，仅在 `sd_cur` CTE 注入行段谓词
  与 `__rn` 序号列（生成期参数化，运行期填三元组）。
- 每批段独立 `exec_region_sql` + `commit_region_tail`：段间提交点与串行
  "行间"位置对齐（串行在异常行处没有批段提交——见 §4.3 口径）。
- 单行段按游标序逐个执行，天然保序。

### 3.1 单行重放原语（核心新机制）——已实现形态

对第 `e` 行，需让解释器"恰好执行一行循环迭代"。**实现采用"真实
Fetch 指令"路径**（优于最初设想的手动回填）：Fetch 的 trace 事件、
步数、`%ROWCOUNT`/`%FOUND`/`%NOTFOUND`、INTO 目标回填（含 RECORD）
全部由原指令路径零改动复用，与串行逐语句一致。

1. **游标前置定位**：`CursorState::seek_before(index)`——游标状态
   等价于"顺序 FETCH 完前 index 行"，随后**真实 `Fetch` 指令**取
   第 index 行（`runtime/cursor.rs`）。
2. **有界执行**：`RunSpan` 扩展第二停止点 `exit_pc`（循环早退边
   目标）+ `entry_exec`（入口即边界仍执行一条，重放专用）；
   `replay_cursor_row`（`runtime/interp.rs`）以
   `span{start=fetch_pc, stop=fetch_pc, exit=exit_pc, entry_exec}`
   驱动 `run_state_spanned`，返回 `RowReplay::{IterDone, LoopExited,
   Returned, Completed}`——`LoopExited` 时混合调度中止余段。
   **逃逸分析（硬性前置证明）**：体内/handler 内不得存在跳到
   边界之外且最终不返回的跳转——由计划期门禁证明，否则 `Skipped`。
3. **步数/trace**：单行段内语句走原解释器路径，事件与步数原样产生；
   与串行的差异仅在"Fetch 由重放驱动而非顺序到达"——口径见 §4.2。

已实现：`seek_before`（cursor.rs + 守护测试）、`RunSpan.exit_pc/
entry_exec`（interp.rs，多停止点+入口豁免 opt-in——`start==stop`
的既有"立即停"语义由 `entry_exec=false` 保留，guard_set_oriented
34/34 回归验证）、`ReplaySpec`/`RowReplay`/`replay_cursor_row`
（interp.rs + 2 个行为测试：IterDone 行号对齐赋值、LoopExited 早退）。

### 3.2 已实现的计划/执行形态（与草案的差异）

- **异常行集不落 TEMP TABLE**：`exc_guards`（每查找一条）在入口期
  各执行一次，返回行号集由 `exc_rows()` 归并为 `BTreeSet<u64>`
  （0 基，`ROW_NUMBER() OVER () - 1`，与 `sd_cur.__sd_rn` 同口径）。
- **分段模板**：`ConversionPlan.seg_sql` 存含 `__SD_LO__`/`__SD_HI__`
  字面量占位符的批写模板；`plan.sql` 保持全范围版本（`|E|=0` 与
  既有断言逐字兼容）。段谓词为
  `__sd_rn >= COALESCE(__SD_LO__,-1) AND __sd_rn < COALESCE(__SD_HI__,MAX)`，
  引擎侧 `.replace()` 注入边界。
- **真实游标驱动**：混合不走 `sd_cur` 批路径的游标物化，而是
  `OpenCursor` span 原生开游标 → `seek_before(e)` + 真实 `Fetch`
  逐异常行重放；`CursorState.total` 给出行数上界。
- **逃逸分析已实现**（`ir/set_oriented/hybrid.rs`）：区域内所有
  跳转目标 ∈ `[fetch_pc, close_pc+1]`、早退目标唯一、`TxRollback`
  全区域（含处理器）拒绝、`PushTrap` 处理器体界内——任一不满足
  `flags.hybrid=false` 走普通 SO。
- **物化表刷新**：重放行的原生例程调用会撤销 `sd_fn*/sd_bf*/sd_vw*`
  会话临时表（解释器由 dynamic.rs 逐站点重写兜底）。混合路径以
  `so_materializations_intact`（一次空集交叉探活）检测，缺失才
  `refresh_so_materializations`（views→fn→views 重建），段前与
  miss_outs 前各探测一次。
- **提交口径**：正常路径 `TxCommit/TxCommitIfWritten` 与普通 SO
  同口径折叠尾提交；异常行重放经真实 Fetch 原生执行体内 COMMIT。
  `TxRollback` 因回滚边界不可复现而整体拒绝混合。

## 4. 语义对齐细则

### 4.1 写序

分段按行序执行 ⇒ 全局写序 = 串行写序（批段内部行序由 `sd_cur.__rn`
升序保证——批段 SQL 须 `ORDER BY __rn` 使 INSERT 物理序与游标序一致；
若目标表消费方依赖堆序，此为必需项，须验证）。

### 4.2 步数预算与 trace（新口径，须文档化+测试锁定）

- 批段：1 次 `SqlExec` trace + 不计步（SO 既有口径）；
- 单行段：体内每语句原样计步/埋点；Fetch 不埋点（定位语义替代）；
- `TxCommitIfWritten`/`TxCommit`：批段尾提交沿用 `commit_region_tail`；
  单行段内 COMMIT/TxCommitIfWritten 由解释器原生处理；
- **风险**：混合计数与任何单一模式都不同——`step_budget` 恰在边界触发
  ENG-0032 的场景会分歧。对策：计划期证明"预算充足"或接受该口径差异
  并在文档显式声明（参考 SO 对批段不计步的既有先例）。

### 4.3 事务/提交点

串行区段内若含逐行 COMMIT：单行段天然复现；批段以 `tail_commit` 单次
提交——**批段覆盖的行区间在串行下有多个提交点，混合下合并为段尾一次**。
提交点合并是 SO 既有偏差的延伸（纯 SO 已把全循环合并为一次），混合不引入
新偏差类；但须验证 `batch_commit` 两种模式下段间提交与单行 COMMIT 交错
不产生"提交后读不到"或"回滚误伤"——单行段异常触发 `reset_session`
（Kernel 域）时，已提交的批段不受影响（已持久化），未提交部分随事务
语义处理——与串行"前面行已提交"一致。

### 4.4 异常行判定的时点正确性

入口守卫在**批写前一次性**判定异常行；串行语义下第 i 行的查找结果可能
受第 j<i 行写入影响。SO 计划期已证"前缀读集 ∩ 循环写集 = ∅"（查找表在
区域内只读）⇒ 判定结果在整个区域执行期间稳定 ⇒ 一次性物化 `sd_exc`
**等价于逐行判定**。该证明是混合的硬前置，缺失即 `Skipped`。

### 4.5 阈值与回退

- `|E| = 0` → 纯批路径（现行）；
- `|E|` 超阈值 → 全区域串行续跑（不重放前缀，现行 fallback 通道）；
- 单行重放中若出现计划期未证的控制流逃逸 → **无法安全回退**（前缀已
  执行、批段已写）→ 必须由 §3.1-3 静态证明杜绝；运行期检测到逃逸
  （pc 越出预期集合）按引擎不变量错误上抛（宁错不错）。

## 5. 风险登记

| # | 风险 | 等级 | 对策 |
|---|---|---|---|
| R1 | `sd_cur` 重查序 ≠ 游标流序 → 行号错位 | **高** | 入口物化 `sd_cur` 快照行数+抽样校验；或改为物化 `sd_cur` 为 TEMP TABLE 一次、批写/守卫/重放共用（推荐，顺带消除重复查询） |
| R2 | 单行重放逃逸（trap resume/GOTO 出体） | **高** | 计划期逃逸分析门禁；运行期越界检测→invariant error |
| R3 | 步数/trace 混合口径与既有契约冲突 | 中 | 文档化新口径 + 对照测试锁定 |
| R4 | 批段 `ORDER BY __rn` 改变执行计划/性能 | 中 | 性能实测；必要时仅在混合模式注入 |
| R5 | `sd_exc` 物化时点与批写间数据漂移 | 低 | 复用 `drifted` 入口漂移复核（lookup_tables 已覆盖） |
| R6 | 异常行比例高 → 混合退化 | 低 | §4.5 阈值回退 |
| R7 | 重放行触发 Kernel 域错误被 handler 捕获 → `reset_session` 作废事务：串行下此前行已逐行 COMMIT 可存活，混合下未提交段写一并丢失 | 低 | 已知边界：Kernel 域=引擎故障面，BIZ 域异常（too_many/no_data_found/others 业务异常）不经过 reset_session 不受影响；如需完全封闭可在计划期对"可能捕获 Kernel 错误的 handler"再加证明，当前未实现 |

## 6. 实施切片（TDD 顺序建议）

1. **Slice A** ✅：`CursorState::seek_before` + 真实 Fetch 驱动的
   `replay_cursor_row`（`RunSpan.exit_pc`/`entry_exec` 多停止点）——
   单测：行号对齐赋值、早退边 LoopExited、流尾/Closed 三态。
2. **Slice B** ✅：`vguard.rs` miss_emu dup 守卫新增行号版
   `exc_guard` 变体（`ROW_NUMBER()` 包裹 cur_scan，输出违规
   `__sd_rn`）；`generate.rs` 在 `flags.hybrid` 时将 dup 守卫从
   `top1_guards` 顶替至 `exc_guards`；`entry.rs` 新增
   `exc_rows()` 归并入口期求值。`|E|=0` 时 exc 为空 → 落回
   既有 SO 路径（`plan.sql` 全范围，行为逐字不变）。
3. **Slice C** ✅：`hybrid::analyze` 逃逸/事务证明 +
   `run_hybrid_region` 分段交错调度（段批写 ↔ 异常行重放保序）+
   物化表探活刷新 + 终止空 FETCH/CloseCursor/miss_outs 尾段。
4. **Slice D** ✅（初步）：byfund_branch `so_ba` 由
   `Skipped{AmbiguousTop1Tie}` → `Converted`（混合路径，
   exc={1,2,4} 规模探针 / 规模化实测见 probe_run.log），
   `v4_perf_byfund_branch` 全配置 OUT 与 Off 基线逐值相等。
   性能（`v4_perf_byfund_branch`，~5000 分组，rounds=2 取最小）：
   off=43.1s / so_ba=**51.0s** / lp_safe≈45s / so_lp=50.9s，
   全配置 OUT 与 Off 基线逐值相等。so_ba 仍慢于串行——异常行
   占比高时段数≈|E|+1，每段重跑一次游标 GROUP BY 是主成本；
   混合的正确性价值在于"此前整体拒绝的区域现在可转换"，
   该种子上 LP 仍是更快路径（SO 系优先序不变）。

## 7. 与既有机制的关系

- **不替代 LP**：LP 是"前缀并行+后缀主线程"的逐行模型；本机制是
  "正常行集合化+异常行解释"的区域模型。混合区若 LP 也能接管，
  优先级仍是 SO 系（现行 funnel 不变）。
- **不放宽 SO 安全不变量**：读集/写集不相交证明、drift 复核、
  fn/view 前导物化全部保留；混合只是把"不可批的行"显式切出。
- **Top-1 取行等价守卫（独立项）**：`top1.rs` 的 `ROW_NUMBER` vs
  `LIMIT 1` 并列守卫放宽仍是独立改进，与本机制正交。
