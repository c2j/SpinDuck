# PR #48 集合化（set_oriented）能力边界验证报告

> 复现入口见同目录 [README.md](./README.md)；一键脚本 [run_verify.sh](./run_verify.sh)。
> 本报告所有论断均以 `reports/` 下实测日志为据，论断范围与证据范围一致。

## 1. 验证对象与口径

| 项 | 值 |
|---|---|
| 被测 PR | #48 `feat/set-oriented-v1-pr` |
| 被测 head | `f62787d95fa82fb442147809978b0250ea0bb48c` |
| CI 状态 | run `43674843043` 在该 head 全绿（fmt/clippy/test/audit） |
| 内核 | DuckDB 1.5.5（bundled，精确 pin） |
| 构建 | debug（`cargo test --features cli`，cli 蕴含 native） |
| 运行环境 | in-memory、单线程调用、macOS arm64 |
| 方法论参照 | `docs/performance-optimization/docs/set-orientation-papers/`（三条路线） |

PR #48 的机制（读自 head 源码，未改动）：对**整段程序**做 6 指令模式匹配
（`OPEN 静态游标 → FETCH record → JumpIf NOTFOUND → SqlExec INSERT → Jump 回环 → CLOSE`），
SQL 形态走 token 白名单，目标表约束在**事务内**经 `duckdb_tables()/views()/indexes()`
逐调用重证（无缓存），原始 IR 保留；默认关闭（`[engine].set_oriented`），
拒绝与 `batch_commit` 组合，失败即 `ROLLBACK` 不重放。产出形如：

```sql
WITH sd_cur AS (SELECT {cols} FROM {src})
INSERT INTO {tgt} ({cols}) SELECT sd_cur.{cols} FROM sd_cur
```

这正是论文 03 §5「**模式 C：纯集合插入**」的下推形态（`INSERT ... SELECT`，
不含 JOIN、不含 `WITH RECURSIVE ... USING KEY`）。

## 2. 方法论：差分等价测试

每个变体在**同一份独立快照**上分别以 `set_oriented=false`（OFF）与 `true`（ON）
运行，逐值比对**业务观察面**：

- 目标表行 bag（含重复行与 NULL，按调试串排序后逐行比对）；
- OUT 参数（`BTreeMap` 逐项）；
- 错误契约（错误码 + 消息关键片段）。

同时断言 ON 侧的 `Status::Converted` / `Status::Skipped{reason}` 与预期一致。
探针采用失败收集模式（`thread_local FAILS`），单点不符不中断矩阵，最终汇总断言。
等价不变量：**只要 ON 选择 Converted，业务观察面必须与 OFF 逐值一致；
选择 Skipped 时回退原 IR，亦必须一致。**

## 3. 等价性结论

`reports/correctness.log`：**47 项 PASS + 3 项 CHAR（等价 OK），共 50 个差分探针，
零分歧**（`test result: ok. 4 passed; 0 failed`）。覆盖重复行、NULL、空游标、
混合大小写引号标识符等等价敏感场景。结论：在本探针覆盖面上，
**集合化改写保持业务语义不变**。

## 4. 准入边界矩阵（按论文路线归类）

### 4.1 正例（Converted，6 项）

`plain_copy`、`mixed_case_unquoted`、`quoted_select_columns`、
`insert_column_reordered`、`select_extra_unused_column`、`empty_source`。
即「静态游标 + 纯列投影 + 单条 INSERT」套路被识别并下推。

### 4.2 路线一：表达式 / 查询形态边界（去相关与代数内联）

| 变体 | Skipped 原因 |
|---|---|
| select_expression / select_where / select_alias / select_qualified / select_star / select_order_by / select_limit / select_distinct / select_join / parameterized_cursor | `UnprovenExpression` |
| insert_expression / insert_literal / update_instead_of_insert / delete_instead_of_insert | `UnsupportedWrite` |

含义：SELECT 侧只接受**纯列投影**（可重排、可多余列），任何表达式 / 过滤 /
排序 / 去重 / JOIN / 参数化游标都拒绝；写侧只接受 `INSERT ... VALUES(纯列)`。
对应 Froid/QBS 的「可综合形态」边界——PR #48 只综合最简固定套路。

### 4.3 路线二：跨轮状态依赖边界（游标消除 / 递归 CTE）

| 变体 | Skipped 原因 |
|---|---|
| if_branch_in_loop / select_into_in_loop / loop_carried_accumulator / out_param_assigned_in_loop / explicit_fetch_into_scalars / two_inserts_in_loop / nested_loop | `UnsupportedControlFlow` |
| record_name_collides_param | `LiveOutState` |

含义：循环体一旦含分支、跨轮累加、OUT 写回、显式 `FETCH INTO` 标量、
多写、嵌套循环即拒绝。对应 Aggify/`USING KEY` 要解决的**状态依赖**——
PR #48 **不处理**任何跨行状态，只取无状态纯插入子集。

### 4.4 路线三：副作用 / 事务 / 组合开关边界（批处理与程序重写）

| 变体 | Skipped 原因 |
|---|---|
| commit_inside_loop | `TransactionBoundaryInsideLoop` |
| call_empty_proc_in_loop | `UnsupportedCall` |
| execute_immediate_in_loop | `DynamicSql` |
| exception_handler_outer | `ExceptionScopePresent` |
| batch_commit_combo | `BatchCommitEnabled` |
| source_equals_target | `TargetReadDependency` |

含义：事务边界、子调用、动态 SQL、异常作用域、与 `batch_commit` 组合、
源=目标自依赖均拒绝。对应 CIDR2024/DBridge 的副作用安全分析——
PR #48 以**保守拒绝**而非批处理通道处理这些情形。

### 4.5 目标表约束 / 类型证明（事务内重证）

| 变体 | Skipped 原因 |
|---|---|
| type_mismatch_int / nullable_to_notnull / unique_index / primary_key / check_constraint / default_value / extra_target_column | `UnprovenTargetConstraints` |
| decimal_columns / date_columns / double_columns / view_source / missing_source / temp_table_source | `UnknownObjectDependency` |

含义：列类型须落在 DDL 白名单（bigint/integer/smallint/tinyint/varchar/boolean），
且目标无唯一索引 / 主键 / CHECK / 默认值 / 多余列、源非视图 / 临时表 / 缺失。
`decimal/date/double` 因 DDL 解析不支持而落到 `UnknownObjectDependency`（见 §7）。

### 4.6 大小写 / 引号边界（CHAR，等价 OK）

| 变体 | 选择 |
|---|---|
| quoted_mixed_case_columns | `Skipped(UnsupportedWrite)` |
| quoted_insert_columns | `Skipped(UnsupportedWrite)` |
| quoted_mixed_case_table | `Skipped(UnprovenExpression)` |

三者均**等价 OK**（拒绝后回退原 IR，业务面不变）。注意 `quoted_select_columns`
（SELECT 侧小写引号列）反而 **Converted**——见 §7 差异①。

## 5. 性能实证：RBAR 消除方向

`reports/perf.log`（debug、in-memory、单线程，三轮墙钟取中位数）：

| 规模 N | OFF 中位 (ms) | ON 中位 (ms) | 加速比 | 业务 SqlExec OFF→ON |
|---|---|---|---|---|
| 3 | 2.87 | 11.84 | **0.24×（更慢）** | 3 → 1 |
| 1000 | 446.6 | 11.11 | ~40× | 1000 → 1 |
| 20000 | 8854 | 18.21 | ~486× | 20000 → 1 |
| 100000 | 45210 | 49.70 | ~910× | 100000 → 1 |

trace 往返计数（`probe_trace_roundtrip_counts`，N=1000）：
OFF `SqlExec=1000 Fetch=1001`；ON `SqlExec=1 Fetch=0`。

结论：

- **方向成立**：逐行 SQL 下发 O(N) 被消除为 O(1) 的 `INSERT...SELECT`，
  规模越大收益越大（100k 行 ~910×，与论文「消除 RBAR 跨边界往返」一致）。
- **存在固定开销交叉点（约 25 行）**：ON 侧含 2 次元数据查询（事务内重证，
  不入 trace），debug 下约 10ms 固定成本；N=3 时 ON 反而更慢。
  小表场景集合化无收益甚至负收益——这是准入开关默认关闭的合理性佐证。
- 口径限制：以上为 **debug 构建**绝对值，仅证明**方向与量级**，
  不作为 release 性能承诺；release 绝对值需另行测量。

## 6. 论文路线完成度映射

| 路线 | 代表论文 / 机制 | PR #48 状态 |
|---|---|---|
| 一 · 去相关与代数内联 | QBS 程序综合（固定套路模板推导） | **部分实现**：仅综合「游标+纯投影+单 INSERT」一种 6 指令套路 |
| 一 | PRISM 选择性内联 / 模式 C 纯集合下推 | **部分实现**：`INSERT...SELECT` 下推已落地（无 JOIN） |
| 一 | Froid 通用标量 UDF / 命令式块内联 | **未实现**：表达式 / 分支 / 多语句一律拒绝 |
| 二 · 游标消除 / 递归 CTE | Aggify（游标→UDAF） | **未实现** |
| 二 | `WITH RECURSIVE` Trampoline / DuckDB `USING KEY` | **未实现**：跨轮状态一律拒绝 |
| 三 · 批处理 / 程序重写 | CIDR2024 Batching（93.4×） | **未实现**：显式拒绝 `batch_commit` 组合 |
| 三 | Sloth 惰性合批 / DBridge 循环裂变 | **未实现** |

**总判定**：PR #48 是论文谱系中**最保守、最窄**的一个落点——
路线一「模式 C 纯集合插入」的单模板综合 + 下推，辅以事务内约束重证保证语义安全。
它**没有**触及路线二（状态游标）与路线三（批处理通道），
对一切超出固定套路的情形以**保守拒绝 + 回退原 IR**处理。
这与「语义先于速度、默认关闭、可回退」的设计取向一致；
能力边界清晰，完成情况与「v1 单模式集合化」的目标相符。

## 7. 与设计文档（§8.6）的措辞差异（2 项，均不影响等价性）

1. **SELECT 侧小写引号列会 Converted**：设计文字暗示引号列应拒绝，
   实测 `quoted_select_columns` 被转换——因 SQL 改写管线对 SELECT 侧
   小写引号列做了归一。业务面等价（正例断言通过），属措辞与实现口径差。
2. **不支持类型标 `UnknownObjectDependency` 而非 `UnprovenTargetConstraints`**：
   `decimal/date/double` 列使目标 DDL 解析失败返回 `None`，
   故归到对象依赖未知而非约束未证。拒绝行为正确，仅原因标签与设计文字不同。

两项均为**标签 / 措辞**层面差异，不改变「拒绝即回退、等价不变」的语义结论。

## 8. 真实语料验证（纯 CLI，case_corpus_purchase）

除玩具探针外，另用 `golden/case_corpus_purchase/sql/routines/` 的**原始考题语料**
做纯 CLI 验证（不写 Rust）：特殊项目 toml（`[engine].set_oriented`）+
`compile --analyze`/`deploy`/`call` 指令，逐例程捕获 `SET_ORIENTED` 判定行并差分 ON/OFF。
复现入口 [corpus/README.md](./corpus/README.md)，脚本
[corpus/run_corpus_verify.sh](./corpus/run_corpus_verify.sh)，
实测 [corpus/reports/](./corpus/reports/)。

| 例程 | ON 判定 | 业务输出（ON==OFF） |
|---|---|---|
| `fnc_com_getday` | `Skipped(UnsupportedControlFlow)` | `RETURN 20250716` |
| `fnc_com_if_deal_date` | `Skipped(UnsupportedControlFlow)` | `RETURN 0` |
| `FNC_GET_PURCHASE_JS_DAYS` | `Skipped(ExceptionScopePresent)` | `RETURN 0` |
| `pkg_split_trade_step3_sh.prc_trd_deal_purchase` | `Skipped(ExceptionScopePresent)` | `OUT p_o_succeed = 0` |

**四个真实语料例程全部 Skipped，ON/OFF 业务输出逐字一致。** 独立函数是单 RETURN
表达式（非六指令套路 → `UnsupportedControlFlow`）；JS_DAYS 与主过程含
`EXCEPTION WHEN OTHERS`（→ `ExceptionScopePresent`）。这与 §4 的玩具边界矩阵相互印证：
真实金融过程的表达式 / 分支 / 异常 / 多写形态都落在准入边界之外，
PR #48 集合化**不在生产语料上触发**。

口径：deploy 只建表结构、空种子，主过程走零次游标迭代的确定性成功路径
（`p_o_succeed='0'`）；集合化判定是纯结构的、在 call 起点生成，不依赖业务数据。
带业务数据的多维断言属黄金原生断言层（`cases_native`），不在本纯 CLI 包范围。
外部依赖 `fnc_com_get_seat_para` 语料内未提供，compile 报 missing=1（计划仍写出、
failed=0），脚本剥离 `[[missing]]` 后部署——缺件门只核表/视图/序列，空种子下该函数不被调用。

## 9. 100k 同源三方基准（Part A 可转换 / Part B 原始过程）

§5 是进程内探针的方向性证据；本节是**纯 CLI、100k 同源数据、含 openGauss 参照**
的端到端三方基准，分两个互补工作负载。复现入口
[bench/README.md](./bench/README.md)（Part A）与
[bench_partb/README.md](./bench_partb/README.md)（Part B）。

### 9.1 Part A：purchase 同形可转换过程（集合化真正触发）

被测过程是从 purchase 核心抽取的**纯列投影搬运**（9 列，映射到 DDL 白名单类型），
落在准入面内 → `Converted`。三方在**同一份 100k 源数据**（DuckDB 生成 → CSV →
参照 `\copy` 同源载入）上各跑 3 轮取中位数，目标表排序后三方逐行比对。
实测 [bench/reports/bench_summary.log](./bench/reports/bench_summary.log)：

| 方 | 中位墙钟 (ms) | 各轮 (ms) | 口径 |
|---|---|---|---|
| SpinDuck OFF（解释器游标 RBAR） | 83115 | 81865 / 83115 / 84088 | CLI call 端到端 |
| SpinDuck ON（集合化 INSERT...SELECT） | 924 | 890 / 950 / 924 | CLI call 端到端 |
| openGauss 原生（plpgsql 游标 RBAR） | 1364 | 1364 / 1410 / 1345 | 纯函数执行 |

- **OFF→ON 加速比 89.9×**（同为 CLI 端到端口径）；固定开销（noop call 中位）约 183ms。
- **数据一致性**：三方目标表排序后逐行比对 = OK，各 100000 行；ON `converted`、OFF 无判定行。
- 口径：SpinDuck 为 CLI 端到端墙钟（含固定开销），参照为纯函数执行（`clock_timestamp`）；
  ON 墙钟主要由固定开销 + 落盘提交构成，净集合化执行远小于墙钟（§5 进程内 100k 约 50ms 佐证）。
  参照用 `FUNCTION + FOR 隐式游标`（openGauss `CREATE PROCEDURE` 不接受 PG `$$` 体，V10/V13）。

### 9.2 Part B：原始 purchase 主过程（集合化不触发）

跑**真实语料原始过程** `pkg_split_trade_step3_sh.prc_trd_deal_purchase`，100k 同源种子
（命中游标过滤 + F001 资金路径，确定性路径 `kind_id='0'`，运行时不调外部函数）。
实测 [bench_partb/reports/partb_summary.log](./bench_partb/reports/partb_summary.log)：

| 方 | 100k 墙钟 (ms) | 集合化判定 | 目标行数 |
|---|---|---|---|
| SpinDuck OFF | 429536 | feature 关，无判定行 | 100000 |
| SpinDuck ON | 469232 | **skipped ExceptionScopePresent** | 100000 |
| openGauss 原生参照 | **不可基线（V21）** | — | — |

- **原始过程 Skipped ⇒ ON≡OFF**：目标 `dat_trd_repurchase` 逐行一致、业务输出逐字一致——
  **零收益、零分歧**。判定是编译期结构分析，与数据量无关。OFF/ON 墙钟同量级
  （~7.2min vs ~7.8min，差异为 debug 单轮测量噪声与库缓存状态，**非**转换效应——
  Skipped 下两者执行完全相同的 RBAR 路径）。
- **参照限制（V21）**：原始过程须经 `CALL`，amd64 postmaster 在 procedure `CALL` 崩溃，
  参照无法对完整原始过程基线（Part A 参照用 `FUNCTION` 不受此限）。

### 9.3 两工作负载合起来的结论

- 集合化的收益**只在准入面内**兑现：Part A 同形可转换过程 100k 取得 ~90× 端到端加速且三方逐行一致。
- 真实生产语料（Part B 原始过程）落在准入面外 → Skipped → ON≡OFF，**当前零收益**。
  要惠及真实过程，需扩展准入面（路线二/三）或把过程重构为纯投影搬运形态。
- 这与「语义先于速度、默认关闭、可回退」一致：宁可保守拒绝，不在复杂过程上冒语义风险。

## 10. 结论

- **等价性**：50 个差分探针零分歧 + 4 个真实语料例程 ON/OFF 逐字一致 + 100k 三方逐行一致，集合化改写保持业务语义。
- **能力边界**：准入面 = 静态游标 + 纯列投影 + 单条纯列 INSERT + 白名单类型 +
  无约束目标 + 无副作用 / 状态 / 事务 / 组合开关；超出即保守拒绝。
  真实语料四例程全部落在边界外（全部 Skipped）。
- **性能**：RBAR 消除方向成立（进程内 100k 行 ~910×，debug），约 25 行存在固定开销交叉点。
  100k 同源三方端到端基准（§9）：Part A 同形可转换过程 OFF→ON ~90×、三方逐行一致；
  Part B 原始过程 Skipped ⇒ ON≡OFF、零收益零分歧（参照受 V21 限制不可基线）。
- **完成度**：实现论文路线一「模式 C」单模板下推；路线二、三未实现（设计内）。
- **CI**：head `f62787d` 五门禁全绿；未改动任何人类已有测试。