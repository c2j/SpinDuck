# SpinDuck 集合化转换引擎 V3 设计文档（面向 `prc_trd_hz_byfund` 多区域转换与安全协同）

> **基线依据**：`c2j/SpinDuck` 工作树 `9946a2b`（`spinduck/src/ir/set_oriented/`、`spinduck/src/engine/set_oriented.rs`、`spinduck/src/runtime/interp/writebuf.rs`）与 `golden/case_corpus_purchase/sql/routines/PKG_SPLIT_TRADE_STEP3_SH.sql`；并参考 PR #56（`test/v2-set-oriented-verify`，提交 `dcb5a2f`）补充的过程验证资产与桩实现。  
> **文档范围说明**：`9946a2b` 主干 `docs/` 下不含 `spinduck-set-oriented-design.md`、`spinduck-set-oriented-v2-design.md` 及 `golden/v2_set_oriented_verify-complex`；本文提到的“V1/V2 设计与 PR #56 桩数据”均在本优化研究目录及 PR #56 外部参照口径下界定，所有实现落点严格对齐 `9946a2b` 实际代码。

---

## 1. 现状核查：为什么 `9946a2b` 拒绝 `prc_trd_hz_byfund`，以及完整阻塞链路

在 `9946a2b` 上对 `PKG_SPLIT_TRADE_STEP3_SH.prc_trd_hz_byfund` 运行 V2 集合化通道时，实际拦截点与拆开第一道门后仍会碰到的门禁必须严格区分：

### 1.1 当前直接打出的拒因（第一道门）
* **`region/mod.rs::extract` 返回 `SkipReason::UnsupportedControlFlow`**：`extract` 在整段 `IrProg` 中只要扫描到第 2 个静态 `IrStmt::OpenCursor`（`if found.is_some()`）即返回 `UnsupportedControlFlow`。`prc_trd_hz_byfund` 依次含 `c_get_data`、`c_get_venture_data`、`c_get_unknown` 三个静态显式游标（以及后续 `FOR res IN (SELECT ...)` 查询循环），因此立即在此处被拒。
* **已成立的骨架前提**：`c_get_data` 的 IR 布局为 `OPEN c_get_data` $\rightarrow$ `FETCH c_get_data INTO r_get_data`（`<<error_row>>` 标签标在 `FETCH` 上，不占指令）$\rightarrow$ `JumpIf(%NOTFOUND)` $\rightarrow$ 主体 $\rightarrow$ `Jump(fetch_pc)` $\rightarrow$ `CLOSE c_get_data`，**已能被现有 `region/mod.rs::recognize` 完整认出**。

### 1.2 拆开“多静态游标门”之后，各段实际触发的后续门禁

| 过程段落（`PKG_SPLIT_TRADE_STEP3_SH.sql`） | 在 `9946a2b` 上随后会碰到的真实门禁 | V3 处置策略 |
| :--- | :--- | :--- |
| **整体与运行驱动**<br>（`engine/set_oriented.rs` · `entry.rs`） | `run_selected` 只认单一 `open_pc`，`suffix_prog` 直接切片 `&code[close_pc + 1 ..]` 拼接到新 `IrProg` 且**不重映射绝对跳转 PC 与 `PushTrap { body_start, body_end }`**，会破坏后段 `c_fxj`/`QFII` 的 `GOTO` 与异常处理 | **V3-1 多区域 `RunSpan` 驱动**：直接在原 `IrProg` 上按分段 `RunSpan` 执行；前缀可证干净的静态区域局部替换，后缀原 PC 不动 |
| **外部未注册函数（3 个）**<br>`fnc_com_get_seat_para`<br>`fnc_get_fund_data_mode`<br>`fnc_get_rate_info2` | 在 `9946a2b` 仓库上均无源码定义（`docs/validation.md` 亦记为外部缺件）。`catalog_summary` 对未登记名称得 `Unregistered`，`admit_effects` 报 `UnsupportedCall`，游标/插入 `fn_sites` 报 `UnresolvedFnSite` | **V3-2**：摘要器只消费 `RoutineCatalog` 中**真实注册的函数体**；未登记一律保守退回。附 PR #56 桩源码仅作验证基准，绝不在代码生成器里硬编码默认值（另见 §4 关于 `extra_tables.sql` 列宽前提说明） |
| **段 1（区域 0）`c_get_data`：** 循环内 `CallRoutine`（`fnc_com_get_seat_para`） | `traps.rs::admit_effects` 只放行处理器体内的 `NoOp` 调用，普通 `CallRoutine` 报 `UnsupportedCall`；`catalog_summary` 不支持多 `OUT` 参数查表函数 | **V3-2 `Summary::LookupTuple`**：按真实 IR 结构匹配「`WHEN OTHERS` 陷阱包裹整段成功路径 + 局部变量直线拷贝到 `OUT`/`RETURN`」，证明全路径不抛错且全 `OUT` 必赋值 |
| **段 1（区域 0）`c_get_data`：** 外层 `WHEN OTHERS -> v_yj := '0'` 与 PL 乘减 `v_yj` | 187–253 行外层 `PushTrap` 包裹函数调用、`v_is_venture`、`sys_dummy` 查询；255–258 行再次对 `v_yj` 赋 `cjje * v_return_rate_org - cjje * v_return_rate`；`analyze/body.rs` 的 `assign` 拒绝重复赋值、`sink` 只认 `Field/Field` 的除和拼接 | **V3-3 显式符号环境 `SymEnv`**：支持局部顺序重赋值、`IF` 合并与算术下沉证明；仅当证明 187–253 行块本身永不抛错时才裁剪 `WHEN OTHERS` |
| **段 1（区域 0）`c_get_data`：** `IF ... jyfy_temp` 聚合与 `r_get_data.jsf := v_jsf` | `analyze/body.rs` 不支持循环内条件 `SELECT nvl(sum(...), 0)` 聚合查表，也不支持 `AssignTarget::Field` 覆写记录字段 | **V3-4 条件聚合与字段覆写**：无 `GROUP BY` 聚合恒 1 行；严格区分 `IF` 门（品种/证券代码）与聚合 `WHERE` 门（`gddmzm`/`scdm`） |
| **段 1（区域 0）`c_get_data`：** `v_par_bond` 异常写 `p_o_succeed` 且不跳行 | `traps.rs::trap_bodies_admissible` 报 `ExceptionScopePresent`；异常时不改 `v_temp_stock_kind`（沿用上一轮残留）并改写 `p_o_succeed` 后仍继续 `INSERT` | **V3-5 入口零异常守卫**：先证 `v_par_bond` 视图及谓词全定义，入口检查 `COALESCE(__cnt, 0) <> 1` 为 `0` 才裁剪异常块；否则整段退回 |
| **段 1（区域 0）`c_get_data`：** 尾部 `INSERT` + `COMMIT` + `GOTO error_row` | **现有 `admit_effects` 已在 `BatchAtomic` 下放行“受 `PushTrap` 保护、紧跟 `SqlExec` 的尾 `TxCommit`”** | 按 `batch_commit` 分流：`false` 时依据 `rows_affected > 0` 当场 `flow::tx(true)` 提交；`true` 时仅发 `TxCommit` trace 并延至调用出口提交；非空 `inst_log` 需 `bad_rows == 0` 守卫 |
| **段 2–4：`c_get_venture_data`/`c_fxj`、`c_get_unknown`、`QFII`** | 嵌套动态游标（`OPEN c_fxj FOR v_sql_txt`）、前缀含 `JumpIf`（`QFII` 位于 `IF` 内且位于非确定提交的循环后）、循环内 `INSERT dat_fund_cjqs` + `UPDATE qfqscjhbrbb_temp` 混合写 | **V3A 显式保留为原 IR 解释执行**；**V3-6** 在全例程 IR 编译期默认将 `IrStmt::SqlExec.buffer_policy` 设为 `Immediate`，仅安全扫描通过才放行 `Buffered` |

---

## 2. V3 总体架构：原 IR 上的多区域 `RunSpan` 状态机与分区域契约

### 2.1 运行驱动重构：`engine/set_oriented.rs`（弃用切片拼装 `suffix_prog`）

V2 的 `suffix_prog` 直接把 `&routine.prog.code[plan.close_pc + 1 ..]` 拷贝进新 `IrProg` 头部并塞入 `SqlExec`，这会导致后文所有绝对 PC 跳转（`c_fxj`、`GOTO error_row2`、`c_get_unknown`、`QFII` 的 `GOTO next_row`）与 `PushTrap { body_start, body_end }` 全部错位。

V3 改为**保留原始 `routine.prog` 只读不变，通过 `RunSpan` 在原程序上分段步进驱动**（设已选区域列表为互不重叠、按 `open_pc` 升序排列的 `plans: Vec<ConversionPlan>`）：

> **关键不变量（对齐 `9946a2b` `runtime/interp.rs::run_spanned` 与 `engine/set_oriented.rs`）**：
> 1. **陷阱栈不跨段继承**：`run_spanned` 每次调用都从空 `traps: Vec<TrapFrame> = Vec::new()` 与 `last_write_rows: 0` 起步（`interp.rs:144-162`）。因此候选区域抽取（`region/mod.rs`）必须硬性校验：**所有分段边界点 `plan.open_pc` 与 `plan.close_pc + 1` 的静态异常陷阱深度必须严格等于 `0`（`trap_depth == 0`）**。
> 2. **成功分支与 `batch_commit` 对齐的唯一提交规则（对齐 `interp.rs:293-308`、`sqlstmts.rs:46-50`）**：
>    * **当 `batch_commit = false`**：与现有 `TxCommitIfWritten` 语义完全一致——先发 `IrStmt` variant `SqlExec` trace、再发 `TraceEvent::SqlExec { sql, binds, elapsed_us: 0 }`（由执行函数发出），当 `plan.tail_commit && rows_affected > 0` 时**先发 `IrStmt` variant `TxCommit` trace，再当场调用 `flow::tx(true)`（`COMMIT + BEGIN TRANSACTION`）**，绝不留给下一次 `run_spanned`（因新 `run_spanned` 会将 `last_write_rows` 清零，且原后缀无 `TxCommitIfWritten`，不提交会导致后续 `ROLLBACK` 误撤已提交的 `dat_fund_cjqs`）。`rows_affected == 0` 时不发 `TxCommit` 且不切换快照；**全程不得发出 `TxCommitIfWritten` 事件**。
>    * **当 `batch_commit = true`**：与解释器开启 `--batch-commit` 时吞并过程内 `TxCommit` / `TxCommitIfWritten`（`env.wbuf.is_some()` 时跳过 `flow::tx`）保持同一事务边界——成功分支同样不经 `WriteBuffer` 直发集合 `SqlExec`（按上述顺序发 `SqlExec` 双事件），在 `plan.tail_commit && rows_affected > 0` 时**仅发 `IrStmt` variant `TxCommit` trace，不在中途调用 `flow::tx(true)`**，将实际提交统一延迟到 `call_with_sink` 出口（从而使“后缀回滚撤掉区域 0”与原循环 `--batch-commit` 语义保持严格一致）。
> 3. **`entry_ok` 的 `EngineError` 与 `SkipReason` 分层（绝不吞掉内核错误）**：`entry_ok` 返回 `Result<Result<(), SkipReason>, EngineError>`。**外层 `Err(EngineError)`（例如守卫 SQL 执行报错或会话事务已进入 aborted 状态）必须通过 `?` 直接上抛**，交由外层 `call_with_sink` 执行 `ROLLBACK`；**只有内层 `Ok(Err(reason: SkipReason))`（结构漂移、计数不为 0 等逻辑拒绝）才代表事务仍干净，允许改跑原区域 `[open_pc .. close_pc + 1)`**。任何可能因求值报错而引发事务异常的表达式检查，必须在计划期通过 `UnprovenQueryTotalness` / `UnprovenExpression` 拒绝，不得放入运行期 `entry_ok`。
> 4. **`--batch-commit` 与多段 `RunSpan` 的停点刷盘契约**：`9946a2b` 的 `run_spanned` 在每段新建独立 `shared_buffer()` 并在任何 `Ok(out)` 返回（**包含 `RunOutcome::Stopped`**，`interp.rs:175`）执行 `writebuf::flush_all`。这意味着在分段边界 `stop_pc = Some(plan.open_pc)` 处，当前段已缓冲的 `INSERT` 会被**有意刷盘**（由于 `trap_depth == 0`，此处无活跃外层异常处理器会被吞并）；若后续实测需要“跨越区域停点继续攒批、停点不刷盘”，则必须同步修改 `run_spanned` 签名，改为接收调用方传入的复用 `WBufOpt` 且仅在非 `Stopped` 返回时 `flush_all`。

```text
cursor_pc = 0
for plan in plans:
    // 1. 解释执行到当前区域入口 plan.open_pc（stop_pc 处指令不执行；要求 open_pc 处 trap_depth == 0）
    outcome = run_spanned(prog, start_pc=cursor_pc, stop_pc=Some(plan.open_pc))
    if outcome != RunOutcome::Stopped: return outcome   // 前缀提前 RETURN / RAISE

    // 2. 在 plan.open_pc 处执行只读守卫 entry_ok(plan)
    //    外层 Err(EngineError) 立即经 `?` 上抛（触发 call_with_sink 回滚），严禁落入退回分支！
    match entry_ok(ex, plan, ctx, frame)?:
      Err(reason):
        // 仅内层 Ok(Err(SkipReason)) 表示事务仍干净，绝不重放前缀：仅用原 IR 解释完本区域
        record_report(region_idx, Skipped(reason))
        outcome = run_spanned(prog, start_pc=plan.open_pc, stop_pc=Some(plan.close_pc + 1))
        if outcome != RunOutcome::Stopped: return outcome
      Ok(()):
        // 守卫通过：当场直发单条集合 SqlExec（不经 WriteBuffer）
        // 硬契约：一旦集合 SqlExec 开始执行并失败，直接上抛，严禁退回原循环重放！
        record_report(region_idx, Converted(plan.sql))
        // 按实际执行顺序发 trace：IrStmt(SqlExec) -> TraceEvent::SqlExec { sql, binds, elapsed_us: 0 }
        rows_affected = exec_region_set_sql(ex, plan, frame, sink)?
        if plan.tail_commit && rows_affected > 0:
            emit_tx_commit_ir_trace(sink)   // 仅发 variant TxCommit，绝不发 TxCommitIfWritten
            if !batch_commit:
                tx_commit_and_reopen(ex)?   // 仅 batch_commit=false 当场 flow::tx(true)；true 时随全过程出口统一提交
    cursor_pc = plan.close_pc + 1           // 要求 close_pc + 1 处 trap_depth == 0

// 3. 解释执行剩余后缀直至结束（所有后续绝对跳转与 PushTrap 原样有效）
return run_spanned(prog, start_pc=cursor_pc, stop_pc=None)
```

> **向后兼容约束**：对仅包含 1 个静态游标区域且后缀无复杂绝对跳转的过程（如 `prc_trd_deal_purchase`），其在 `batch_commit = false` 下与 V2 生成完全等价的集合 SQL、提交快照语义、Trace 事件（`IrStmt::SqlExec` $\rightarrow$ `TraceEvent::SqlExec` $\rightarrow$ `IrStmt::TxCommit`，零行无 `TxCommit`）与报告结果，必须通过锁定测试确保行为零漂移。

### 2.2 候选区域前缀准入与 `BatchAtomic` 契约

1. **前缀干净度约束（V3A 边界）**：
   * `region/proof.rs::prefix_is_clean` 在遇到 `JumpIf`/`JumpIfNot` 时保守拒绝。V3A 仅转换区域 0（`c_get_data`），沿用现有**“从最近必经无条件 `TxCommit` 起算，或整段前缀至 `open_pc` 无未提交 DML”**的必经提交走查即可通过（`c_get_data` 前是 `DELETE dat_fund_xw_venture; COMMIT;` 无条件直线路径）；
   * **禁止把前一个循环内部的 `COMMIT` 当作必经提交**：游标为空时循环内 `COMMIT` 根本不会执行。因此凡位于前序循环或条件分支之后的区域（`c_fxj`、`c_get_unknown`、`QFII`），在 V3A 中一律不作为集合化候选区域，交由 `RunSpan` 尾部原样执行。
2. **嵌套静态游标的明确语义**：
   * 若存在一个静态游标区域 `R_outer = [outer_open ..= outer_close]` 内部包含另一个静态 `OpenCursor` `R_inner`（`outer_open < inner_open < outer_close`）：**外层 `R_outer` 拒绝转换，且落在 `[outer_open ..= outer_close]` 区间内的所有内层 `OpenCursor` 也不单独抽取/转换**。
3. **`SetOrientedMode` × `batch_commit` 分区域协同矩阵**：
   * `Strict`（`batch_commit = false`）：保持 V2 严格等价，含循环内 `COMMIT` 的 `c_get_data` 继续返回 `TransactionBoundaryInsideLoop`；
   * `BatchAtomic`（`batch_commit = false`）：V3 主基线——`c_get_data`（区域 0）集合化执行，依据本次执行返回的 `rows_affected > 0` 当场调用 `flow::tx(true)` 提交 1 次并重开调用事务；`c_fxj`/`QFII` 走原 IR 解释（含 `LookupExecutor`）；
   * `BatchAtomic`（`batch_commit = true`）：解除全局互斥——区域 0 走集合 SQL（不经 `WriteBuffer`，按 §2.1 规则仅在 `rows_affected > 0` 发 `TxCommit` trace、**不在中途 `flow::tx(true)`**，与全过程 `--batch-commit` 一同在 `call_with_sink` 出口统一提交）；未集合化路径中的 `IrStmt::SqlExec` **默认即为 `BufferPolicy::Immediate`**，仅当通过 **§3.6** 安全扫描标为 `BufferPolicy::Buffered` 后才进入 `WriteBuffer`（且 `Stop` 停点遵循 §2.1 契约 4 的有意边界刷盘）。

### 2.3 拒因（`SkipReason`）与模块布局

沿用 V2 已有 `SkipReason`（**聚合全定义证明失败沿用 `UnprovenQueryTotalness`，不新增重复项**），仅新增 2 项 V3 专用精确拒因：
* `MultiOutCallImpure`：多 `OUT` 参数被调函数内部存在未捕获的 `Raise`、DML/副作用或存在未赋值的 `OUT`/返回值路径；
* `PartialHandlerStateCarryover`：区域内存在“不跳行且会漏赋变量/写 `OUT` 错码”的异常块（如 `v_par_bond` 缺失时沿用上一轮 `v_temp_stock_kind`），且无法生成或未通过零异常守卫。

模块演进蓝图（严格对齐 `9946a2b` 现有文件结构）：
```text
spinduck/src/
├── engine/set_oriented.rs              # 【重构】基于 RunSpan 的多区域分段驱动 + 区分 batch_commit 条件提交
├── ir/
│   ├── types.rs                        # 【扩展】IrStmt::SqlExec 增加编译期 buffer_policy: BufferPolicy（默认 Immediate）
│   └── mod.rs / dead.rs / ...          # 编译期 WriteBuffer 安全分析 pass（对全部编译例程生效，仅扫描通过标 Buffered）
├── runtime/interp/writebuf.rs          # 【加固】take_insert 识别 IrStmt::SqlExec 的 BufferPolicy 标记（默认 Immediate 直发）
└── ir/set_oriented/
    ├── mod.rs                          # 【升级】多区域调度、RegionReport（region_idx: 0..）
    ├── region/
    │   ├── mod.rs                      # 【升级】extract_all() 抽取顶级静态 SESE 区域（校验起止 trap_depth==0、排除嵌套内层）
    │   ├── proof.rs                    # 【升级】真实活跃分析 live_out_check()（只收集守卫裁剪后仍可能写的变量 + CallRoutine.out_targets）
    │   └── traps.rs                    # 【升级】admit_effects 放行已摘要 CallRoutine；trap_bodies_admissible 识别零异常守卫型块
    ├── catalog_summary.rs & catalog_summary/
    │   ├── body.rs                     # 【升级】支持 LookupTuple 全路径直线拷贝与全部 OUT 必赋值追踪
    │   ├── expand.rs                   # 【升级】Summary::LookupTuple 关系化 LEFT JOIN + 兜底展开
    │   └── tuple.rs                    # 【新增】多 OUT 查表（trap 包裹整段成功路径）准入与全定义证明
    ├── analyze.rs & analyze/
    │   └── body.rs                     # 【升级】显式 SymEnv（支持局部变量重复赋值、AssignTarget::Field、IF 聚合与算术表达式）
    ├── entry.rs                        # 【升级】ConversionPlan 增加 zero_error_guards（删去单区域 suffix_prog 依赖）
    └── expr.rs / schema.rs             # 【升级】PL 乘减 DECIMAL 精度/38位宽证明与 4 层等价校验
```

---

## 3. 核心阻塞与语义边界的 V3 详细设计

### 3.1 方案 V3-1：真实出口活跃分析（`live_out_check`）

V2 的 `region/proof.rs::live_out_check` 是“区域内赋过值、后缀任何位置读过就拒绝”，这会误杀 `v_is_venture`（在 `c_get_data` 写过，到 `c_fxj` 第 447 行先写/异常赋 `'0'`、第 474 行才读）。

V3 改为**面向后缀 `prog.code[region.close_pc + 1 ..]` 的控制流可达先读判定（Live-In Analysis）**：
1. 构造后缀控制流图（涵盖正常顺序、条件/无条件跳转及后缀所有 `PushTrap` 处理器体边）；
2. **变量写收集必须基于守卫裁剪后的有效区域写**：只收集在**应用 `zero_error_guards` 不可达处理器裁剪之后，集合化等价执行路径内仍可能被写入**的变量/形参——具体涵盖 `IrStmt::Assign { target }`、`IrStmt::Fetch { targets }`、`IrStmt::SqlQuery { into }`、`IrStmt::FoldedQuery { target }` **以及 `IrStmt::CallRoutine { out_targets }`（如 `OUT`/`INOUT` 写回）**；凡已由守卫证明不可达的异常处理器内部写操作（例如被 `v_par_bond` 零异常守卫裁剪掉的处理器内 `p_o_succeed` 赋值、以及当 `inst_log` 为 `NoOp` 或已守卫 `bad_rows == 0` 时被裁剪掉的 `INSERT` 处理器内 `p_o_succeed` 赋值）**一律不计入“可能写”集合**；
3. **变量读收集必须完整**：表达式 `Var`/`Field`、SQL `binds`、`CallRoutine.args` 均算作读操作；
4. **严格判定准则（只有仍可能被写才进后缀先读检查）**：
   * 若某变量在裁剪后的 `region` 内**没有任何剩余写操作**（写入集合为空），无论后缀是否在先写之前读取它，它都**不是本区域的出口活跃状态**（直接放行，因为集合路径不写它与其保持原值语义完全一致）；
   * 若某变量在裁剪后的 `region` 内**仍可能被写**，则只有当它在后缀中**“存在一条从 `close_pc + 1` 出发、未经前置必经写入就到达读取的路径”**时，才触发 `SkipReason::LiveOutState`。
   * **在 `prc_trd_hz_byfund` 区域 0 上的精确结果**：
     * `p_o_succeed`：只有当 `v_par_bond` 异常处理器（V3-5 守卫证明不可达）与 `INSERT` 异常处理器（`inst_log` 为 `NoOp` 或 `bad_rows == 0` 证明不触发）**两者均已被裁剪、区域 0 内剩余写集合为空**时，才不触发 `LiveOutState`，允许集合路径不写 `p_o_succeed`；若任一处理器写路径无法证死，则继续报 `LiveOutState` / `PartialHandlerStateCarryover` 并整段退回；
     * `v_is_venture`：区域 0 内仍有写（`SELECT ... INTO`），因此进入后缀检查；因后缀从 `close_pc + 1` 出发的**每一条**可达读取路径均先经过 `c_fxj` 第 447 行必经写（`SELECT ... INTO` 或其 `WHEN OTHERS` 处理器）才到达第 474 行的读 $\rightarrow$ **非活跃，安全通过**。

---

### 3.2 方案 V3-2：多 `OUT` 查表函数摘要 `Summary::LookupTuple`（严禁硬编码默认值）

**核心原则**：摘要器**只消费 `RoutineCatalog` 中实际注册的函数源码体**。对 `9946a2b` 上未注册的 `fnc_com_get_seat_para` / `fnc_get_fund_data_mode` / `fnc_get_rate_info2`，`catalog_summary` 必须老实返回 `Summary::Unregistered`，由 `admit_effects` / `analyze` 拒绝（`UnsupportedCall` / `UnresolvedFnSite`），**绝不在代码生成器里凭业务印象硬编码 `'00'` / `'0'` / 表名或过滤谓词**。

> **基准验证上下文说明（PR #56 桩源码）**：只有当外部项目源码或 PR #56 验证资产中注册了真实/桩函数体时（例如 PR #56 的 `0_STUB_fnc_com_get_seat_para.sql`、`0_STUB_fnc_get_fund_data_mode.sql`、`0_STUB_fnc_get_rate_info2.sql`），V3 摘要器才对其做如下静态分析与展开（桩源码清单见 **附录 A**）。

#### `Summary::LookupTuple` 准入与展开规范（对齐附录 A 编译后 IR 形态）
1. **结构准入条件（匹配“陷阱包住整段成功路径”）**：
   * 在附录 A `0_STUB_fnc_com_get_seat_para` 编译后的 IR 中，**`PushTrap(WHEN OTHERS)` 包住的是整段成功路径**（`RETURN` 直接退出函数，正常路径不会执行到 `PopTrap`），结构为：
     `[前置纯赋值]` $\rightarrow$ `PushTrap(WHEN OTHERS)` $\{$ `单条 SqlQuery(SELECT c_1..c_m INTO l_1..l_m FROM T WHERE ...)` $\rightarrow$ `直线拷贝/纯赋值（可经局部变量中转）写齐全部 OUT 形参` $\rightarrow$ `Return(expr)` $\}$ $\rightarrow$ 处理器体 `body_start..body_end`；
   * 形参仅含 `IN` 与 `OUT`（或能证明全部路径不依赖入参初值的 `OUT`）；函数体内无 DML、无循环、无动态 SQL、无外部未知调用。
2. **直线拷贝追溯、全路径异常收敛与全赋值硬证明（消除跳过 `v_is_venture` 的跨行残留陷阱）**：
   * **顺着局部变量的直线拷贝反推查询列**：分析正常路径（`SqlQuery` 后到 `RETURN` 之间）维护函数内局部符号环境，将 `p_o_flag := v_flag; p_o_org := v_org; p_o_rate := v_rate; RETURN v_type;` 沿直线拷贝追溯还原到 `SqlQuery` 的第 `i` 个查询列（允许 `SELECT INTO` 写入局部变量再直线拷贝给 `OUT`，不要求 `SELECT INTO` 直接写入 `OUT`）；
   * **全定义证明覆盖整段受保护成功路径（含拷贝阶段）**：因 `WHEN OTHERS` 陷阱兜底的是整段成功路径而不只是查询基数错误（`NO_DATA_FOUND`/`TOO_MANY_ROWS`），`proof.rs` 必须同时证明：① `SqlQuery` 自身谓词与投影全定义；② **正常路径上每条直线拷贝/表达式求值及写入局部/`OUT` 槽位（含 `IrProg.var_types` 的 `VARCHAR`/数值隐式转换）自身也不会抛错**；
   * **处理器体全赋齐且绝不抛错**：`PushTrap` 处理器体内部**不得 `Raise`**，且沿所有控制流路径必须对**每一个 `OUT` 形参以及 `RETURN` 返回值赋全定义常量/纯表达式**；正常路径（`SqlQuery` 返回 1 行）同样必须赋齐每一个 `OUT` 形参及 `RETURN`；
   * 只有上述全部证明通过，才能保证函数在运行期**绝对不会向调用方抛出异常**（从而证明调用方 `c_get_data` 中外层 `BEGIN ... fnc_com_get_seat_para; v_is_venture; sys_dummy; EXCEPTION WHEN OTHERS` 不会因函数报错而跳过 `v_is_venture` 查询）。
3. **关系化展开（全从被调函数 AST 与符号环境自动提取，零硬编码）**：
   * 将被调函数 `SqlQuery` 的表、谓词、经直线拷贝还原后的“返回值/`OUT` 形参 $\leftrightarrow$ 查询列”映射，以及处理器体中提取出的兜底常量映射打包为 `LookupTuplePlan`；
   * 在调用方生成 `LEFT JOIN` 派生表（按关联键 `GROUP BY` + `COUNT(*) AS __cnt`），并为返回值与每个 `OUT` 实参生成 `CASE WHEN __cnt = 1 THEN <列_i> ELSE <函数体提取的常量_i> END` 绑定到调用方 `SymEnv`，同时逐一校验接收槽位 `IrProg.var_types` 的 `coerce_for_slot` 等价性。

---

### 3.3 方案 V3-3：显式符号环境 `SymEnv`、多次赋值、`sys_dummy` 与 PL 乘减

1. **升级 `analyze/body.rs` 为顺序推进的符号环境 `SymEnv`**：
   * 将原 V2 的单次绑定列表（`inlined` / `folded` / `sinks`）升级为按程序顺序维护的符号状态映射 `SymEnv: BTreeMap<BindSrc, SymExpr>`（同时覆盖局部变量 `Var` 与记录字段 `Field`）；
   * 初始状态下，游标记录字段 `r.<col>` 映射为 `_cur.<col>`；过程 `IN` 只读形参保持参数绑定占位符 `$p_*`；
   * 遇到普通赋值 `AssignTarget::Var(v) | AssignTarget::Field { rec, field }` 时，先将右值表达式内所有引用变量按当前 `SymEnv` 展开（**同步继承并合并子表达式触发的前置 `expr_guards` 与类型约束**），再覆写 `SymEnv[target]`——**天然支持 `v_yj`、`v_jsf`、`r_get_data.jsf` 在同一轮循环内被多次顺序赋值**；
   * 遇到 `IF cond THEN ... [ELSE ...] END IF` 时，克隆分支 `SymEnv` 各自推进，汇合处对任意一侧修改过的 `BindSrc` 做 Froid Phi 合并：`SymEnv[k] = CASE WHEN (<cond_sql>) THEN <then_env[k]> ELSE <else_env[k]> END`。
2. **精确处理 `c_get_data` 第 187–258 行的 `v_yj` 两条路径**：
   * 严格区分：
     * **第 207–248 行 `SELECT CASE v_commision_type ... ELSE 0 END INTO v_yj FROM sys_dummy`**：这里的 `ELSE 0` 是**普通 `CASE` 业务落空分支（数值 `0`）**，`v_commision_type` 是该 `CASE` 的输入；
     * **第 251–252 行外层 `EXCEPTION WHEN OTHERS THEN v_yj := '0'`**：这是覆盖第 187–250 行整个块的**异常兜底处理器**，执行后继续向下运行（不跳过 `INSERT`）。
   * **V3 证明与裁剪规则（不混淆 `ELSE 0` 与异常 `'0'`，也不把 `WHEN OTHERS` 收成坏行过滤）**：
     1. 必须先后证明：`fnc_com_get_seat_para` 自身内部收敛不抛异常（V3-2）、`v_par_fund_check_para` 查找已通过内部 `WHEN OTHERS` 折叠为全定义 `LEFT JOIN` 常量兜底、`sys_dummy` 存在且恰 1 行、`CASE v_commision_type ... ELSE 0` 全部分支表达式与比较在当前类型域上**全定义不抛错**；
     2. 只有当上述证明全部成立时，外层 `WHEN OTHERS`（`v_yj := '0'`）才被证明**绝对不可达而安全裁剪**，投影中**仅保留纯数值 `CASE`**；
     3. 若无法证明该 `sys_dummy` 块全定义不抛错（例如某个 `yjXX` 列存在类型风险），**必须整段退回原解释器**（严禁像 371–395 行 `GOTO error_row` 处理器那样把它转成 `INSERT` 坏行过滤！）。
   * **第 255–258 行 PL 乘减与 `%TYPE` 等价**：
     * `IF v_return_rate_flag = '1' THEN v_yj := r_get_data.cjje * v_return_rate_org - r_get_data.cjje * v_return_rate; END IF;`
     * 因为 `v_yj` 为 `dat_fund_cjqs.yj%TYPE`（未进 `IrProg.var_types`，运行时不做中间槽位截断），`SymEnv` 直接记录合并后的数值表达树；
     * **`proof.rs` 精度与溢出证明**：依据各操作数来自游标聚合与 `par_fund_seat_para` 列的 `SchemaProvider` 类型（如 `DECIMAL(p, s)` / `DOUBLE`），验证 DuckDB 中 `cjje * return_rate_org - cjje * return_rate` 的计算**不会突破 DuckDB `DECIMAL` 38 位最大精度限制，且与 SpinDuck `runtime/eval.rs` / `numeric.rs` 乘法标度/舍入一致**；若无法证明精度等价或存在溢出可能，则保守报 `UnprovenExpression` 退回。

---

### 3.4 方案 V3-4：条件无 `GROUP BY` 聚合查表（精确修复 `jyfy_temp` / `jsf` 语义）

对应 `PKG_SPLIT_TRADE_STEP3_SH.sql` 第 260–278 行：
```sql
IF r_get_data.stock_kind = '0100' AND r_get_data.zqdm between '609100' and '609999' THEN
  v_jsf := 0;
  BEGIN
    select nvl(sum(nvl(t.jyfy,0)),0) INTO v_jsf
      from jyfy_temp t
     where t.zqdm = r_get_data.zqdm
       and t.gddm = r_get_data.gddm
       and t.data_date = p_i_date
       and t.bs = decode(r_get_data.bs,'1','B','-1','S')
       and t.jydy = r_get_data.xwdm
       and r_get_data.gddmzm = '000000'
       and r_get_data.scdm = '001';
  EXCEPTION WHEN OTHERS THEN v_jsf := 0;
  END;
  r_get_data.jsf := v_jsf;
END IF;
```

1. **严禁把聚合 `WHERE` 里的游标字段条件放进外层 `IF` 门**：
   * 真正的 `IF` 条件只有 `r_get_data.stock_kind = '0100' AND r_get_data.zqdm BETWEEN '609100' AND '609999'`；
   * `r_get_data.gddmzm = '000000'` 与 `r_get_data.scdm = '001'` 是聚合查询内部的 `WHERE` 条件：当 `IF` 成立但这两项不成立（含 `NULL`）时，`SUM` 为空集，`nvl(..., 0)` 返回 **`0`**，随后 `r_get_data.jsf := v_jsf` 把原 `_cur.jsf` **覆盖为 `0`**（而不是保留 `_cur.jsf`！）。
2. **正确关系化写法**：
   * 在证明 `jyfy_temp` 关联列、聚合表达式 `nvl(sum(nvl(t.jyfy, 0)), 0)` 及 `decode(r_get_data.bs, '1', 'B', '-1', 'S')` **全定义不抛错**（否则报 `UnprovenQueryTotalness` 退回）且 `Reads(jyfy_temp) ∩ Writes(Region) = ∅` 后：
   * 构建按等值连接键分组的聚合 CTE（仅分组 `zqdm, gddm, bs, jydy`，**绝不把 `gddmzm` 或 `scdm` 放进 `GROUP BY`**）：
     ```sql
     _jyfy AS (
       SELECT zqdm, gddm, bs, jydy, COALESCE(SUM(COALESCE(jyfy, 0)), 0) AS jyfy_sum
       FROM jyfy_temp
       WHERE data_date = $p_i_date
       GROUP BY zqdm, gddm, bs, jydy
     )
     ```
   * 在主查询外层精确给出 `_jyfy` 的 `LEFT JOIN ON` 连接键（将 `decode(r_get_data.bs, '1', 'B', '-1', 'S')` 转为 `CASE`，并将 `gddmzm = '000000' AND scdm = '001'` 留在主投影外层 `CASE` 中）：
     ```sql
     LEFT JOIN _jyfy _jf
       ON _jf.zqdm = _cur.zqdm
      AND _jf.gddm = _cur.gddm
      AND _jf.jydy = _cur.xwdm
      AND _jf.bs = CASE _cur.bs WHEN '1' THEN 'B' WHEN '-1' THEN 'S' END
     ```
   * 更新 `SymEnv[(r_get_data, "jsf")]` 为：
     ```sql
     CASE WHEN _cur.stock_kind = '0100' AND _cur.zqdm BETWEEN '609100' AND '609999'
          THEN CASE WHEN _cur.gddmzm = '000000' AND _cur.scdm = '001'
                    THEN COALESCE(_jf.jyfy_sum, 0)
                    ELSE 0
               END
          ELSE _cur.jsf
     END
     ```
   * 验证：`IF` 成立且 `gddmzm`/`scdm` 成立 $\rightarrow$ 命中取 `jyfy_sum`，未命中取 `0`；`IF` 成立但 `gddmzm` 为 `NULL`/非 `'000000'` 或 `scdm <> '001'` $\rightarrow$ **`0`**；`IF` 不成立 $\rightarrow$ **`_cur.jsf`**。四分支 100% 对齐原过程。

---

### 3.5 方案 V3-5：`v_par_bond` 零异常入口守卫（修复分组逻辑与 `MAX` 消歧风险）

1. **前置全定义证明（必过）**：
   * 在生成任何入口守卫之前，`proof.rs` 必须递归展开 `v_par_bond` 视图及其底表 `par_sys_bond`（校验 `Reads(v_par_bond) ∩ Writes(Region) = ∅`），并证明其投影与 `WHERE security_id = ... AND $p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date` 自身**全定义（不会因视图内部隐式转换/除法产生运行错）**；
   * 若视图全定义证明失败，**直接拒绝转换并报 `UnprovenQueryTotalness`**（绝不生成入口守卫去执行可能抛内核错的 SQL）。
2. **唯一的入口零异常守卫 SQL（消除区间重叠漏检）**：
   * 必须**先在子查询内执行日期过滤**，再**仅按等值连接键 `GROUP BY security_id` 计数**，绝不允许把 `inure_begin_date, inure_end_date` 放进 `GROUP BY`：
     ```sql
     SELECT COUNT(*)
     FROM (<c_get_data 游标 SQL>) _cur
     LEFT JOIN (
       SELECT security_id, COUNT(*) AS __cnt
       FROM v_par_bond
       WHERE $p_i_date BETWEEN inure_begin_date AND inure_end_date
       GROUP BY security_id
     ) _b ON _b.security_id = _cur.security_id
     WHERE COALESCE(_b.__cnt, 0) <> 1;
     ```
   * **同一 `security_id` 存在两段重叠日期均覆盖 `$p_i_date` 时**：`_b.__cnt = 2`，`COALESCE(_b.__cnt, 0) <> 1` 为真，守卫结果 `> 0`，**精准拦截并整段退回原解释器**（完整保留 `too_many_rows` 的 `p_o_succeed` 文案与 `v_temp_stock_kind` 上行残留，再执行 `INSERT`）。
3. **主集合 SQL 中的安全取值（不过滤 `_cur`，也不用 `MAX` / `HAVING COUNT(*) = 1`）**：
   * 入口守卫在同一事务快照下已证明：对于 `_cur` 中的每一行，`_cur.security_id` 在 `v_par_bond WHERE $p_i_date BETWEEN inure_begin_date AND inure_end_date` 中**存在且仅存在 `1` 行**；
   * 因此主查询**直接对已过滤日期后的唯一行视图做 1:1 关联**，不再过滤 `_cur` 行，也不使用 `MAX(bond_kind)` 或 `HAVING COUNT(*) = 1`（`HAVING COUNT(*) = 1` 或外层 `WHERE __cnt = 1` 都会在意外情况把 `_cur` 游标行静默丢掉，而非整段退回！）：
     ```sql
     LEFT JOIN (
       SELECT security_id, bond_kind
       FROM v_par_bond
       WHERE $p_i_date BETWEEN inure_begin_date AND inure_end_date
     ) _b ON _b.security_id = _cur.security_id
     ```
4. **生产非空 `inst_log` 与 371–395 行 `INSERT` 处理器边界**：
   * 371–395 行的 `EXCEPTION WHEN OTHERS THEN inst_log(...); GOTO error_row;` 是本循环**唯一**的坏行跳过处理器：
     * 若 `inst_log` 已在目录中证为 `NoOp` 空桩：在 `BatchAtomic` 下允许过滤坏行。过滤按本条 `INSERT`（300–328 行列清单）实际写入的投影做 `TRY_CAST` 到目标列类型（覆盖 `numeric(15,2)` / `numeric(15,3)` / `numeric(20,8)` 精度溢出与字符串长度/类型）。`NOT NULL` 只检查这 14 列：`fund_code`、`xwdm`、`main_xwdm`、`security_id`、`bs`、`trade_type`、`bcrq`、`pay_type`、`scdm`、`stock_kind`、`gddm`、`gddmzm`、`check_type`、`gh_type`（含 `to_number(r_get_data.bs)` 合法性）。`etf_flag` 与 `mom_fund` 不在列清单中，默认 `'0'`，只记在 §4.1 的表结构指纹里，不进入过滤谓词；
     * 若 `inst_log` 非 `NoOp`：在 `expr_guards` 中要求上述投影转换与这 14 列 `NOT NULL` 校验的坏行数为 `0` 才走直写集合 SQL，否则退回原路径（或后续 V3B 再启用阶段表异常回放写日志）。

---

### 3.6 方案 V3-6（路线 B 安全协同）：运行期 `WriteBuffer` 编译期标注与精确刷盘屏障

1. **为什么必须在编译期标到 `IrStmt::SqlExec` 上、默认设为 `Immediate`、且对全部例程生效**：
   * `--batch-commit` 是可独立开启的解释器开关，即使未开启集合化（`SetOrientedMode::Off`）或过程未被集合化（如含 `QFII` 的整个过程）也会进入 `WriteBuffer`。`writebuf::replay_group` 在合批失败时会吞掉 PL 异常处理器（丢 `inst_log`、丢 `p_o_succeed` 赋值、丢 `CLOSE`/`GOTO`、丢 `UPDATE`）；
   * 在 `9946a2b` 中（`spinduck/src/runtime/interp/writebuf.rs`），`writebuf::push` 只接收展开后的 SQL 文本片段（`prefix`, `values_part`, `&resolved`），**根本看不到 CFG、`PushTrap` 或 `GOTO`**；运行期决定是否攒批的唯一接缝是 `IrStmt::SqlExec` 调用的 `writebuf::take_insert`；
   * 因此必须在**编译期（IR 编译完成后的静态 pass，对全部例程统一运行、不依赖是否走集合化）**为每条 `IrStmt::SqlExec` 设置 `buffer_policy: BufferPolicy`：
     * **默认值为 `BufferPolicy::Immediate`（安全失败兜底）**——任何未通过扫描或动态构造路径的 `INSERT` 均直接走即时执行，绝不因漏标而误入 `WriteBuffer`；
     * **只有明确通过下方安全性扫描且确认为纯 `INSERT ... VALUES` 写盘的指令，才升级标注为 `BufferPolicy::Buffered`**；
     * 运行期 `writebuf::take_insert` 首先检查该标记：若为 `Immediate`，先按表 `flush_if_touches` 保证先后序，随后直接返回 `Ok(false)` 走即时执行 `sqlstmts::exec`。

2. **按 `9946a2b` 实际刷盘点校准的 CFG 扫描屏障（绝不误用语句种类估）**：
   * 从任意一条静态 `INSERT ... VALUES` 的 `IrStmt::SqlExec` 出发，沿控制流向后扫描**直到下一次确定刷盘屏障之前的全部可达路径**，以及覆盖该 `INSERT` 的全部活跃 `PushTrap` 处理器体：
     * **确定全局刷盘屏障（`flush_all`，在此停止扫描）**：
       1. `IrStmt::CallRoutine`（`interp.rs:373` 入口调 `writebuf::flush_all`）；
       2. `IrStmt::SqlExecDynamic`（`interp.rs:363` 入口调 `writebuf::flush_all`）；
       3. 例程正常结束 / `Return`（`interp.rs:175` 返回 `Ok(out)` 前调 `writebuf::flush_all`）；若保留每段独立 `shared_buffer()`，则 `RunSpan` 停点 `stop_pc` 处（`Stopped` 也走 `interp.rs:175` 调 `writebuf::flush_all`）同样是确定刷盘边界；
       4. 到达 `IrStmt::TxRollback` 所在终止路径（`writebuf::discard` 丢弃缓冲并回滚）。
     * **仅同表刷盘点（`flush_if_touches`，只按目标表名决定是否为屏障）**：
       * `IrStmt::SqlQuery` / `IrStmt::FoldedQuery` / 非 `VALUES` 形态的 `SqlExec`：仅当语句静态触及当前 `INSERT` 的同一张表时才构成刷新点；**对不同表的 DML（如 `UPDATE qfqscjhbrbb_temp`）不会触发刷新，扫描必须继续穿过并判定存在跨表写耦合**。
     * **非刷盘屏障（绝对不能在此提前停止扫描）**：
       * `IrStmt::OpenCursor`（`interp.rs:382` / `cursors.rs::open`，**包含 `OPEN c_fxj FOR v_sql_txt` 动态游标，在 `9946a2b` 中目前均不调用 `flush_all` 也不调用 `flush_if_touches`**）、`Fetch`、`CloseCursor`、`TxCommit`（在 `batch_commit = true` 下被吞并，不是刷新点）、`Jump` / `JumpIf` / `JumpIfNot`。
   * **维持 `BufferPolicy::Immediate`（拒绝升级 `Buffered`）的两条红线**：
     1. 到达确定全局/同表刷盘屏障之前的可达路径上，存在**修改其他表的 DML**（如 `QFII` 段 `INSERT dat_fund_cjqs` 穿过被吞并的 `TxCommit` 后紧跟 `UPDATE qfqscjhbrbb_temp SET split_err_msg = '成功', split_flag = '0'`）；或
     2. 包裹该 `INSERT` 的任意活跃 `PushTrap` 处理器体不是“空跳过”，而是包含副作用或控制流跳转（含 `CLOSE`、`GOTO`、写 `p_o_succeed`、写日志）。`QFII` 688 行这条 `INSERT` 的外层处理器在 754–765 行，只调 `inst_log`，因此本条红线成立。595–605 / 629–637 / 664–673 行的 `UPDATE qfqscjhbrbb_temp` 是更早几次 `SELECT` 的处理器：写完各自的 `split_err_msg` 后 `GOTO next_row`，走不到这条 `INSERT`，不是包裹它的 `PushTrap`。
   * **关于 `--batch-commit` 下失败路径 `TxCommit` 的精确边界**：
     * `BufferPolicy` 只作用于 `IrStmt::SqlExec`（`take_insert`），碰不到 `IrStmt::TxCommit`。因此在 `--batch-commit` 下，过程内的 `COMMIT` 仍会被 `TxCommit`（`env.wbuf.is_some()`）吞并并延至外层 `call_with_sink` 统一提交。688 行这条 `INSERT` 保持 `Immediate`，是因为成功路径上 744–745 行的跨表 `UPDATE` 落在真刷盘屏障之前（红线 1），并且外层处理器调用 `inst_log`（红线 2）。`INSERT` 失败时进入 754–765 行：只写日志，不改 `qfqscjhbrbb_temp`，`split_err_msg` 与 `split_flag` 保持原值，成功分支的 `UPDATE split_err_msg = '成功'` 不会执行。595 / 629 / 664 行的错误文本是那几次 `SELECT` 失败时写入的。若未来业务要求在 `--batch-commit` 下也让这些中间 `TxCommit` 真正落盘（`flow::tx(true)`），必须在 `IrStmt::TxCommit` 上另立独立的编译期放行标记与准入条件，不能靠 `SqlExec.buffer_policy` 表达。

3. **基于 `9946a2b` 实际运行时的长尾加速路径界定**：
   * 澄清现状：`9946a2b` 的 `Engine::new` **已经启用了 `LookupExecutor` 和 `TraceNoopSink`**，因此 PR #56 的 `692s` debug 基线已经享受了这两项（`lookup_cache/shape.rs` 的 `is_plain` 拒绝带 `GROUP BY`/`WITH` 的查询，故点查缓存不能接住 `c_fxj` 外层动态拼接的聚合 SQL，但仍可接住 `c_fxj` 内层对 `v_par_fund_check_para` 的单行点查）；
   * **V3A 不改动解释器全局结构**：`Frame` 槽位 `SlotId` 数组化作为独立解释器工程任务按剖面决定，不写入 V3 集合化语义合同；
   * **V3B 通用无损加速**：仅对高频纯标量（`to_char` / `to_date` / `substr` / `nvl` / `decode`）与 `numeric.rs`/`sqlfn` 对齐等价域后做 Rust 本地求值，并对通过安全扫描标为 `BufferPolicy::Buffered` 的 `INSERT` 接入 DuckDB `Appender`。

---

## 4. `prc_trd_hz_byfund` 区域 0（`c_get_data`）完整映射规格（基于 PR #56 桩体示例）

> **实现规范与外部 DDL 前置说明**：
> 1. 本节投影与连接关系**严禁在代码生成器中硬编码**，以下仅展示当 `RoutineCatalog` 注册了 PR #56 的 3 个函数桩体后，V3 编译器自动推导出的区域 0（`c_get_data`）等效变换结构；**真实投影必须严格以 `PKG_SPLIT_TRADE_STEP3_SH.sql` 第 300–370 行 `INSERT INTO dat_fund_cjqs` 的全 28 列 AST 表达式与 `FETCH` 后的记录字段名为准**（特别注意游标第 69–86 行已将 `yj01..yj18` 别名为错位费型名，如 `yj01 AS yj12`、`yj02 AS yj11`，引用 `r_get_data.yj12` 时必须对齐游标实际别名输出！）。
> 2. **表结构列宽前置依赖**：在 `9946a2b` 的 `golden/case_corpus_purchase/sql/ddl/tables.sql` 第 153 行中，`par_fund_seat_para.commision_type` 仍定义为 `character varying(1)`；而下文投影中的 `'11'..'36'` 费型及桩默认值 `'00'` **强依赖 PR #56 `extra_tables.sql`（第 38 行 `ALTER TABLE par_fund_seat_para ALTER COLUMN commision_type TYPE character varying(2)`）将其扩宽为 `varchar(2)`**。仅注册 3 个桩函数而不做该 DDL 扩宽时，`case_corpus_purchase` 原表无法容纳两位费型代码。

### 4.1 区域 0（`c_get_data`）入口守卫清单
1. 目录指纹复核：`mid_yjqs_detail`、`dat_fund_cjqs`（无主键/唯一索引，**但含 `xwdm`、`security_id`、`bs`、`trade_type`、`bcrq`、`pay_type`、`fund_code`、`stock_kind`、`scdm`、`main_xwdm`、`etf_flag`、`gddm`、`gddmzm`、`check_type`、`gh_type`、`mom_fund` 共 16 列 `NOT NULL` 约束与多列 `numeric(p,s)` 精度限制**；其中 `etf_flag`、`mom_fund` 不在区域 0 这条 `INSERT` 的列清单内，默认 `'0'`，坏行过滤见第 3 条）、`par_fund_seat_para`、`v_par_fund_check_para`、`jyfy_temp`、`v_par_bond`（及 `par_sys_bond`）结构未漂移，`sys_dummy` 存在且 `COUNT(*) = 1`；
2. 零异常守卫（V3-5）：`v_par_bond` 在 `$p_i_date` 下按 `security_id` 计数，`_cur` 中 `COALESCE(__cnt, 0) <> 1` 为 `0`；
3. 表达式与目标约束守卫（`expr_guards`）：游标 `to_number(a.bs)`（改写后 `CAST(a.bs AS DECIMAL(38,6))`）以及 `INSERT` 内全定义表达式均不抛错；对受 371–395 行 `GOTO error_row` 保护的赋值，按 `dat_fund_cjqs` 实际列类型对投影做 `TRY_CAST`（覆盖 `numeric(15,2)`/`(15,3)`/`(20,8)` 溢出与字符串长度）。`NOT NULL` 只校验本条 `INSERT` 写入的 14 列：`fund_code`、`xwdm`、`main_xwdm`、`security_id`、`bs`、`trade_type`、`bcrq`、`pay_type`、`scdm`、`stock_kind`、`gddm`、`gddmzm`、`check_type`、`gh_type`。当 `inst_log` 为 `NoOp` 时上述条件作为坏行过滤，当 `inst_log` 非 `NoOp` 时验证坏行数为 `0`。

### 4.2 区域 0（`c_get_data`）全 28 列投影映射规格（以原 `INSERT` AST 为准）

设 `_cur` 为已内联 `fnc_get_fund_data_mode` 且清空 `fn_sites` 的 `c_get_data` 游标结果集，`_sp` 为 `fnc_com_get_seat_para` 展开后的 `LEFT JOIN`，`_cp` 为 `v_par_fund_check_para` 展开后的 `LEFT JOIN`，`_jf` 为 `jyfy_temp` 聚合 `LEFT JOIN`（连接键 `ON _jf.zqdm = _cur.zqdm AND _jf.gddm = _cur.gddm AND _jf.jydy = _cur.xwdm AND _jf.bs = CASE _cur.bs WHEN '1' THEN 'B' WHEN '-1' THEN 'S' END`），`_b` 为已证 `COUNT(*)=1` 的 `v_par_bond` 唯一派生表（直接 `LEFT JOIN`，不过滤 `_cur`）。

中间符号变量（由 `SymEnv` 顺序推演并按目标列对齐）：
* `__comm_type` = `CASE WHEN _sp.__cnt = 1 THEN _sp.commision_type ELSE <桩提取默认 '00'> END`（依赖 `varchar(2)` 扩宽）
* `__ret_flag` = `CASE WHEN _sp.__cnt = 1 THEN _sp.return_rate_flag ELSE <桩提取默认 '0'> END`
* `__ret_org` = `CASE WHEN _sp.__cnt = 1 THEN _sp.return_rate_org ELSE <桩提取默认 0> END`
* `__ret_rate` = `CASE WHEN _sp.__cnt = 1 THEN _sp.return_rate ELSE <桩提取默认 0> END`
* `__is_venture` = `CASE WHEN _cp.__cnt = 1 THEN _cp.is_venture ELSE '0' END`
* `__yj_base` = `CASE __comm_type WHEN '11' THEN _cur.yj11 WHEN '12' THEN _cur.yj12 ... WHEN '36' THEN _cur.yj36 ELSE 0 END`（数值 `CASE`，187–253 行已证全定义）
* `__yj` = `CASE WHEN __ret_flag = '1' THEN _cur.cjje * __ret_org - _cur.cjje * __ret_rate ELSE __yj_base END`（经 `proof.rs` 核验 `DECIMAL` 精度与不溢出）
* `__jsf_final` = `CASE WHEN _cur.stock_kind = '0100' AND _cur.zqdm BETWEEN '609100' AND '609999' THEN (CASE WHEN _cur.gddmzm = '000000' AND _cur.scdm = '001' THEN COALESCE(_jf.jyfy_sum, 0) ELSE 0 END) ELSE _cur.jsf END`
* `__rate_flag` = `fnc_get_rate_info2` 展开结果（入参为 `_cur.fund_code, $p_i_date, '3', _b.bond_kind`）

目标表 `dat_fund_cjqs` 全 28 列投影清单（按 301–370 行逐列对齐）：

| # | 目标列 | 集合投影表达式（基于 `_cur` 与 `SymEnv`） |
| :--- | :--- | :--- |
| 1–7 | `fund_code`, `xwdm`, `main_xwdm`, `security_id`, `bs`, `trade_type`, `bcrq` | `_cur.fund_code`, `_cur.xwdm`, `_cur.main_xwdm`, `_cur.security_id`, `_cur.bs`, `_cur.trade_type`, `$p_i_date` |
| 8 | `cjsl` | `_cur.cjsl` |
| 9 | `cjje` | `_cur.cjje + CASE WHEN _cur.fffs = '0' THEN _cur.accrual ELSE 0 END` |
| 10 | `yhs` | `_cur.yhs` |
| 11 | `jsf` | `__jsf_final`（**含 `r_get_data.jsf := v_jsf` 覆写**） |
| 12–15 | `jiesf`, `zgf`, `ghf`, `ghf_dealer` | `_cur.jiesf`, `_cur.zgf`, `_cur.ghf`, `_cur.ghf_dealer` |
| 16 | `yj` | `__yj` |
| 17 | `fxj` | `CASE WHEN _cur.scdm = '001' THEN (CASE WHEN __is_venture = '0' THEN 0 ELSE _cur.fxj END) ELSE _cur.fxj END` |
| 18 | `accrual` | `CASE WHEN __rate_flag = '0' THEN _cur.accrual2 ELSE _cur.accrual END` |
| 19–21 | `pay_type`, `scdm`, `stock_kind` | `_cur.fffs`, `_cur.scdm`, `_cur.stock_kind` |
| 22 | `mrcb` | `_cur.mrcb + (CASE WHEN _cur.fffs = '0' THEN _cur.accrual ELSE 0 END) + CAST(_cur.bs AS DECIMAL(38,6)) * __yj`（**复用 `sql/rewrite/ast.rs` 对 `to_number` 的统一改写 `CAST(... AS DECIMAL(38,6))`**） |
| 23 | `accural_tax` | `abs(_cur.accrual - _cur.accrual2)` |
| 24–26 | `gddm`, `gddmzm`, `check_type` | `_cur.gddm`, `_cur.gddmzm`, `_cur.check_type` |
| 27 | `gh_type` | `_cur.date_source`（**注意目标列名为 `gh_type`，取自 `_cur.date_source`**） |
| 28 | `cjbz` | `_cur.cjbz` |

（成功分支内紧接本次集合 `SqlExec` 的实际返回值判定：仅当 `plan.tail_commit && rows_affected > 0` 时发出 `IrStmt` variant `TxCommit` trace，**仅在 `batch_commit = false` 时当场调用 `flow::tx(true)`（`COMMIT + BEGIN TRANSACTION`），`batch_commit = true` 时保持吞并延迟到外层 `call_with_sink` 提交**；随后 `RunSpan` 从 `region_0.close_pc + 1` 启动新段续跑后续 `c_fxj` / `c_get_unknown` / `QFII`。）

---

## 5. 测试分层与回归门禁（`cargo test` 单元守护 vs 独立基准）

### 5.1 默认 `cargo test` 单元测试矩阵（纯小夹具，秒级运行）
1. `should_drive_multiple_regions_with_run_span_and_preserve_suffix_absolute_gotos`：覆盖三类边界——① `batch_commit = false` 下区域 0 转换并当场提交后，后缀出错触发外层 `ROLLBACK` 仍保留区域 0 已提交行；② `batch_commit = true` 下区域 0 不中途 `flow::tx`，后缀回滚时区域 0 一并回滚（对齐原循环）；③ `entry_ok` 返回内核 `Err(EngineError)` 时直接上抛，仅 `Ok(Err(SkipReason))` 改跑原区域且后缀绝对 `GOTO`/异常处理正常；
2. `should_reject_outer_and_inner_when_static_cursors_are_nested`：验证嵌套静态游标既不转换外层区域，也不单独转换落在该外层 PC 区间内的内层 `OpenCursor`；
3. `should_keep_prc_trd_deal_purchase_single_region_conversion_unchanged`：在 `batch_commit = false` 下锁定 `prc_trd_deal_purchase` 仍为单区域 `Converted`，生成的 SQL、提交快照语义、报告与开启 `sink` 时的三条有序事件（`IrStmt` variant `SqlExec` $\rightarrow$ `TraceEvent::SqlExec { sql, binds, elapsed_us: 0 }` $\rightarrow$ `IrStmt` variant `TxCommit`，绝不发 `TxCommitIfWritten`，且 0 行时无 `TxCommit`）零漂移；
4. `should_inline_multi_out_lookup_tuple_and_reject_unregistered_or_partial_out`：验证注册按「`WHEN OTHERS` 包住整段成功路径 + 直线拷贝赋值 `OUT`/`RETURN`」的多 `OUT` 查表函数后正确展开；未注册返回 `Unregistered`/`UnsupportedCall`；某异常分支漏赋一个 `OUT`、向上 `Raise`、或正常路径拷贝本身可抛错时返回 `MultiOutCallImpure`；
5. `should_lower_jyfy_aggregate_with_exact_four_case_semantics`：覆盖 `jyfy_temp` 四种精准预期——① `IF` 成立 + `gddmzm='000000' & scdm='001'` 命中 $\rightarrow$ `SUM`；② 0 行 $\rightarrow$ `0`；③ `IF` 成立但 `gddmzm IS NULL` 或 `<>'000000'` 或 `scdm<>'001'` $\rightarrow$ **`0`（覆盖 `_cur.jsf`）**；④ `IF` 不成立 $\rightarrow$ **保留 `_cur.jsf`**；
6. `should_fallback_when_v_par_bond_has_zero_overlapping_or_query_error`：构造 **3 条夹具**——① 缺失 1 行（`no_data_found`）、② 同 `security_id` 两段重叠日期均覆盖 `$p_i_date`（`too_many_rows`）、③ **计划期对 `v_par_bond` 全定义证明失败（`UnprovenQueryTotalness`）**。①②由运行期零异常守卫整段退回原解释器；③区域不转换，原解释器执行到 `WHEN OTHERS`。运行期守卫若抛 `EngineError`，按 §2.1 上抛，不作为本测试的退回路径。三条都验证 `v_temp_stock_kind` 跨行残留与对应 `p_o_succeed` 文案与 `Off` 完全一致；
7. `should_reject_sys_dummy_block_when_not_total_and_verify_decimal_mul_sub`：验证 `sys_dummy` 块无法证全定义时整段退回（不误作坏行过滤），以及 `cjje * return_rate_org - cjje * return_rate` 的小数精度与 38 位宽溢出拒绝边界；
8. `should_enforce_write_buffer_immediate_policy_for_qfii_cross_table_and_fxj_close`：在 `--batch-commit` 下（同时测 `SetOrientedMode::Off` 与 `BatchAtomic`）验证编译期默认 `BufferPolicy::Immediate` 对全部例程生效。`QFII` 分两路断言：① 688 行 `INSERT dat_fund_cjqs` 失败进入 754–765 行，只调 `inst_log`，不改 `qfqscjhbrbb_temp`，`split_err_msg` 保持原值，`split_flag` 保持原值，744–745 行的 `UPDATE ... '成功'` 不执行；② 595–605 / 629–637 / 664–673 行的 `SELECT` 失败各自写入对应 `split_err_msg` 后 `GOTO next_row`，走不到这条 `INSERT`，表上错误文本与 `Off` 一致。两路的 `COMMIT` 仍随 `--batch-commit` 在调用出口统一提交。另构造 `c_fxj` 内 `OPEN c_fxj FOR` 之后第 1 行写失败，断言扫描未被 `OpenCursor` 提前截断且保留 `CLOSE c_fxj; GOTO error_row2` 截断语义；
9. `should_require_zero_bad_rows_when_inst_log_is_non_noop`：替换 `inst_log` 为写日志表实现，验证存在坏行（含目标列 `NOT NULL` 违例或 `numeric` 溢出）时退回或日志表行数与 `Off` 一致（且 `p_o_succeed` 非空路径不误放行集合化）；
10. `should_convert_region0_of_byfund_and_match_off_on_small_corpus`：以包含 `c_get_data` + `c_fxj` + `QFII`（含非空 `qfqscjhbrbb_temp` 混合成功/失败行）的缩小夹具验证 `region_0`（`c_get_data`）报 `Converted`、`Off` vs `BatchAtomic` 四张目标表及 `p_o_succeed` 逐行全等。

### 5.2 独立黄金与分段剖面门禁（非默认 `cargo test`）
* **前置分段计时剖面门禁**：在同一种构建口径（同时记录 debug 与 release 两组数字）下，对 `prc_trd_hz_byfund` 增加分段计时输出：**前缀（2 次 `UPDATE` + `DELETE`） / 区域 0（`c_get_data`） / `c_fxj` 动态游标 / `QFII`**，用以实证评估各段加速比，**不以未分段的单次 wall-clock 推算替代实测**；
* **100k 黄金回归任务**：纳入独立黄金脚本，要求在 100k 输入下 SpinDuck（`Off` / `BatchAtomic`）与 openGauss 7.0.0-RC3 四张目标表全列排序逐行全等、`OUT p_o_succeed = 0`，且功能测试不把 100k 耗时阈值作为正确性断言。

---

## 6. 实证落地顺序（V3A $\rightarrow$ V3B）

1. **M0（基线剖面）**：先在独立黄金基准中补齐 `prc_trd_hz_byfund` 四段耗时分解（Prefix / `c_get_data` / `c_fxj` / `QFII`）与 release 构建基准，确认 `c_get_data` 与 `c_fxj` 真实占比；
2. **M1（V3-6 安全底线先行）**：在全例程 IR 编译期默认把 `IrStmt::SqlExec.buffer_policy` 设为 `Immediate`、仅安全扫描通过标 `Buffered`，并在 `writebuf::take_insert` 生效（测试 8 全绿），确保 `--batch-commit` 在任何模式组合下不破坏 `QFII` / `c_fxj` 语义；
3. **M2（V3A 多区域核心能力）**：落地 `RunSpan` 多区域驱动（`batch_commit` 分流提交与 `entry_ok` 错误分层，V3-1）、真实 `live_out_check`、`LookupTuple`（V3-2）、显式 `SymEnv` 与聚合/字段覆写（V3-3、V3-4）、`v_par_bond` 零异常守卫与全定义证明（V3-5），完成 `prc_trd_hz_byfund` 区域 0 集合化并通过全部 10 项单元守护测试与 100k 黄金对账；
4. **M3（V3B 长尾收益评估）**：根据 M0 的 release 分段剖面，针对性推进 Rust 本地高频标量求值与安全段 `Appender` 直写（若 `c_fxj` 动态游标在实测中占比显著，另立专项评估对其内层写入走 `Appender` 或模板化）。

---

## 附录 A：PR #56 中使用的 3 个外部桩函数源码（仅供参考，摘要器仅解析目录实有体）

在 `9946a2b` 主干上以下 3 个函数未提供源码定义；PR #56（`golden/v2_set_oriented_verify-complex/sql/routines/`）为完成双端对账所使用的确定性桩实现如下：

```sql
-- 1. fnc_com_get_seat_para：单表点查 + 全部 OUT 与 RETURN 在正常与 WHEN OTHERS 路径均赋值
CREATE OR REPLACE FUNCTION fnc_com_get_seat_para(
  p_i_date VARCHAR2, p_i_scdm VARCHAR2, p_i_fund_code VARCHAR2,
  p_i_xwdm VARCHAR2, p_i_stock_kind VARCHAR2,
  p_o_flag OUT VARCHAR2, p_o_org OUT NUMBER, p_o_rate OUT NUMBER
) RETURN VARCHAR2 IS
  v_type  par_fund_seat_para.commision_type%TYPE;
  v_flag  par_fund_seat_para.return_rate_flag%TYPE;
  v_org   par_fund_seat_para.return_rate_org%TYPE;
  v_rate  par_fund_seat_para.return_rate%TYPE;
BEGIN
  SELECT t.commision_type, t.return_rate_flag, t.return_rate_org, t.return_rate
    INTO v_type, v_flag, v_org, v_rate
    FROM par_fund_seat_para t
   WHERE t.fund_code = p_i_fund_code
     AND t.market_code = p_i_scdm
     AND t.seat_code = p_i_xwdm
     AND t.stock_kind = p_i_stock_kind
     AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  p_o_flag := v_flag; p_o_org := v_org; p_o_rate := v_rate;
  RETURN v_type;
EXCEPTION
  WHEN OTHERS THEN
    p_o_flag := '0'; p_o_org := 0; p_o_rate := 0;
    RETURN '00';
END;

-- 2. fnc_get_rate_info2：PureExpr 纯分支函数
CREATE OR REPLACE FUNCTION fnc_get_rate_info2(
  p_fund_code VARCHAR2, p_i_date VARCHAR2, p_i_type VARCHAR2, p_bond_kind VARCHAR2
) RETURN VARCHAR2 IS
BEGIN
  IF p_bond_kind = '0201' THEN RETURN '0'; END IF;
  RETURN '1';
END;

-- 3. fnc_get_fund_data_mode：PureExpr 常量函数
CREATE OR REPLACE FUNCTION fnc_get_fund_data_mode(
  p_bcrq VARCHAR2, p_gddm VARCHAR2, p_sub_gddm VARCHAR2, p_xwdm VARCHAR2
) RETURN VARCHAR2 IS
BEGIN
  RETURN '1';
END;
```
