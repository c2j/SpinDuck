# 论文探针差距报告

> 被测：`spinduck 0.4.0`，分支 `fix/infix-mod-reject`（= #68 之上叠加「中缀 MOD 显式拒绝 + % 取余」，未提交），2026-11-28 复跑。
> 复现：`bash run_probes.sh`（约 3 分钟）；原始产物在 `reports/`，汇总数字在 `reports/summary.txt`。
> 面向决策的汇总版（含逐篇论文的可行性与价值评估）：`docs/performance-optimization/docs/spinduck-set-oriented-gap-report.md`。
> 口径见 [README.md](README.md)——**等价性基准是 Off 自己**，没有 GaussDB 参照侧；Off 本身对不对另看参照列与期望列。

## 0. 结论

1. **解析层不是差距。** 公开基准 SQL-ProcBench 的 79 个 PL/SQL 文件，71 个能被 SpinDuck 编译
   （存过 44/45、标量 UDF 21/21、触发器 6/6）。
2. **集合化这一层，论文与基准的案例 0 个可转换。** 79 个探针里 batch-atomic 下 `Converted` 6 个，
   全是定标探针和仓库自己的测试夹具；`compile --admit` 的总账是 main `targets=58 converted=6`、
   tpch `targets=14 converted=0`。strict 模式下两个工程都是 0。
3. **只要 Off 能跑通，集合化（含回退）就没有出现结果不一致。** 77 个例程探针里 63 个逐值相等，
   其余是 Off 自身报错、两种模式退出码一致（含中缀 MOD 4 例——2026-11-28 起编译期显式拒绝）。
4. **论文案例带来了一份不依赖 Off 自身的参照，于是第一次能评价 Off 对不对。** 10 个带参照的探针
   全部一致（其中 3 个的参照是**论文作者写的 SQL**，另有 1 个是论文给出的改写结果，其余 6 个是我
   手写的等价 SQL）——除法语义修复后，`G.savings` 的占比也从 3.14 对齐到 3.135917。手写期望里
   的中缀 `MOD`（原静默 NULL / 无返回值）2026-11-28 起改为**编译期显式拒绝**（BIZ-0126），
   与参照「语法错误」对齐；`%` 取余按参照实测支持（X4.pct：`7 % 3` = 1）。
5. **论文案例还暴露出 5 处新的引擎缺口**（§5）：表别名被当成循环记录变量、DATE 型变量绑定进 SQL 后
   变成 `Date32(n)` 文本、PL 表达式里的类型转换不支持、带 OUT 参数的过程不能在区域里调用。
   加上上一轮的 8 处（其中除法 4 处、中缀 MOD 3 处已转显式拒绝/修复，`DUAL` 一条经参照核实不是
   缺口），引擎侧未修缺口一共 6 处。
6. **可转换形状的边界没有变化**（§2）：INSERT 必须套在跳行异常块里；不能有 NULL 字面量；
   循环内 IF 赋值只在夹具那一种组合下被接受；标量 FETCH、跨迭代累计、嵌套游标、每行 UPDATE、
   循环里逐行调函数各自单独致命。

## 1. 方法

两个工程、79 个探针（77 个例程调用 + 2 个查询通道）：

- **main**（64 个）：手工种子。定标阶梯、Froid 图 1、Aggify、CIDR 2024 / PRISM 的 `getManufact`、
  Hirn & Grust 的 `walk()`、DBridge / QBS / Sloth 的模式、SQL-ProcBench 逐字例程、引擎侧探针。
- **tpch**（15 个）：SQL 生成的 TPC-H 形状数据（8000 张订单、24000 行明细）。Hirn & Grust 案例集里
  基于 TPC-H 的函数、Froid §11.1 的 TPC-H 谓词 UDF、PRISM 的谓词提升场景。

每个例程探针三轮：Off → batch-atomic → strict，每轮前按需 `reset`，轮后导出结果表快照，
用引擎的 `compare` 归一后比对。另有两列评价 Off 本身：

- **参照**（`off_vs_oracle`）：`cases/oracle.sql` 里的 SQL 在同一个 DuckDB 上跑出的结果。
  来源分两种，文件里有标注——「论文给的」（Hirn & Grust 案例集的 `service-tr.sql`、`ship-tr.sql`，
  QBS 图 3 的查询）与「本包手写的」（案例集另外四个函数的 SQL 版本依赖 PostgreSQL 的行数组，
  DuckDB 跑不了；以及 DBridge、Froid 谓词的等价式）。
- **期望**（`off_vs_expect`）：清单里手写的函数返回值。只有数值语义的探针用，期望按 Oracle NUMBER 语义写。

可信度的前提与上一版相同：T0（最简规范基底）和 T2.f（仓库夹具逐字移植）在 batch-atomic 下都是
`Converted` 且与 Off 相等，探针能识别“转成功”。

代码来源的四种忠实度（逐字 / 仅改 X / 按论文改写 / 自造）见 [README.md](README.md)。

## 2. 定标：可转换形状的边界

与上一版结论一致。规范基底 T0 = FOR-IN + 跳行 trap 内一条 INSERT（取自仓库夹具的形状），
每个探针只加一个特性（batch-atomic 判定；全部与 Off 逐值相等）：

| 探针 | 加了什么 | 判定 | 原因 |
|---|---|---|---|
| T0 / T0.r | 基底；显式游标 `FETCH c INTO r` | **Converted** | — |
| **T0.s** | `FETCH c INTO v1, v2` | UnsupportedControlFlow | `region/mod.rs::recognize` 要求 `Fetch { record: true }` |
| P0 系列（5 个） | 去掉跳行 trap | UnsupportedWrite（标量写法是 UnsupportedControlFlow） | `analyze.rs`：`allow_filter = BatchAtomic && has_trap` |
| T0.null | INSERT 参数用 NULL 字面量 | UnprovenWriteCompleteness | 写完备性证明不收 NULL 字面量 |
| T2–T2e（5 个） | 循环内 IF 赋值的各种变体 | UnsupportedControlFlow | IF 覆写门只认 batch-atomic、门体恰一条乘/减赋值、变量须是已识别的查找写汇 |
| T3 | 跨迭代累计 | UnsupportedControlFlow | 循环携带状态不被接受 |
| T4 | 嵌套游标 | UnsupportedControlFlow | `extract`：区域内再遇静态游标即整体拒绝 |
| T8n | 点查不带异常块 | UnsupportedControlFlow | — |
| T8 / T7 | 点查后 `SUBSTR` 写入定长列 / INSERT 里调用纯表达式函数 | UnprovenWriteCompleteness | — |
| T9 | 循环内每行再 UPDATE | UnsupportedWrite | `analyze/body.rs`：区域内第二条 `SqlExec` 即拒 |
| **T8b** | 点查数值列，带 `WHEN OTHERS` 兜底 | **Converted** | — |
| **T10** | 循环内每行 COMMIT | **Converted** | batch-atomic 合并循环内提交 |
| **T2.f** | 仓库夹具（四次带兜底点查 + `sys_dummy` CASE + IF 数值覆写） | **Converted** | — |

## 3. 论文与基准案例逐项

batch-atomic 下的判定。「参照」「期望」两列评价的是 Off，不是集合化。

### 3.1 Hirn & Grust 案例集（SIGMOD 2021 的配套仓库，基于 TPC-H 的函数）

| 探针 | 案例 | Off | 集合化 | 参照 / 期望 |
|---|---|---|---|---|
| G.service.fn / G.service | `service_level`：聚合 + IF/ELSIF，无循环；驱动逐客户调用 | OK | UnsupportedControlFlow / UnresolvedFnSite | **参照 = 论文 SQL，一致**（300 行） |
| G.ship | `preferred_shipmode`：三次聚合 + 三路 IF/ELSIF | OK | UnresolvedFnSite | **参照 = 论文 SQL，一致**（300 行） |
| G.global.fn / G.global | `long_distance`：数组遍历改游标循环 + 提前 RETURN | OK | UnsupportedControlFlow / UnresolvedFnSite | 参照（手写），一致；期望 PASS |
| G.late | `order_kept_waiting`（TPC-H Q21）：**忠实驱动**，内层游标的表别名与记录名都叫 `s` | **ERR** `ENG-0044 bind record s undefined` | — | 表别名被当成记录变量（§5.2 第 4 条） |
| G.late.b | 同上，只把记录名 `s` 改成 `sup` | OK | UnsupportedControlFlow | 参照（手写），一致（800 行） |
| G.margin.fn | `goto_margin`：**忠实版**，推进查询里绑定 DATE 变量 | **ERR** `KERNEL-0010 invalid date field format: "Date32(8058)"` | — | DATE 变量绑定（§5.3 第 1 条） |
| G.margin | `goto_margin_r`：WHILE + 键集翻页 + 运行最值，只改日期比较的写法 | OK | UnsupportedCall（带 OUT 参数的过程） | 参照（手写），一致（100 行） |
| G.savings | `goto_savings`：计数 FOR + 每轮三次查表 + 累加 | OK | UnsupportedCall | **参照（手写），一致**（3.135917；除法修复前是 3.14，§5.1） |

8 个基于 TPC-H 的函数里 6 个收录（`pack`、`schedule` 依赖集合类型，未收录）。
**Off 的结果全部与参照一致**（`G.savings` 在除法语义修复后对齐）。

### 3.2 Froid（PVLDB 2018）

| 探针 | 案例 | Off | 集合化 | 参照 / 期望 |
|---|---|---|---|---|
| B1.fn / B1.drv | 图 1 `total_price`（逐字）及驱动 | OK | UnsupportedControlFlow / UnresolvedFnSite | — |
| B1.s / B1.q | §2.1 的调用查询：例程内 `INSERT … SELECT` / `query` 通道 | OK / **ERR**（`query` 不注册例程） | UnsupportedControlFlow / — | — |
| FR.price.fn | §11.1 `discount_taxprice`（嵌套调用） | OK | UnsupportedControlFlow | 期望 PASS |
| FR.q3 | §11.1 `q3conditions`：调用两个 UDF、多个提前返回；驱动逐行调用（500 行） | OK | UnresolvedFnSite | 参照（手写），一致 |
| FR.d | 三个日期谓词的**逐字版**：PL 表达式里 `CAST(x AS DATE)` | **ERR** `ENG-0026` | — | PL 表达式不支持类型转换（§5.3 第 2 条） |

### 3.3 PRISM（PVLDB 18(1)）与 Batching（CIDR 2024）

| 探针 | 案例 | Off | 集合化 | 参照 / 期望 |
|---|---|---|---|---|
| C1.fn / C1.drv | 两篇论文共用的 `getManufact`（仅改参数名的版本）及驱动 | OK | UnsupportedControlFlow / UnresolvedFnSite | — |
| PR.hoist | 算法 4 的场景：UDF 出现在 `INSERT … SELECT` 的 WHERE 里 | **ERR** `invalid date field format: "Date32(8063)"` | — | UDF 的 DATE 实参（§5.3 第 1 条） |

### 3.4 Aggify（SIGMOD 2020）

| 探针 | 案例 | Off | 集合化 |
|---|---|---|---|
| A1.fn / A1.drv | 图 1 `minCostSupp`（按正文重构）及驱动 | OK | UnsupportedControlFlow / UnresolvedFnSite |
| A2.fn / A2.drv | 图 2 累计 ROI（按正文重构）及驱动 | OK | 同上 |
| A3 / A4 / A5 / A6 | §6.1 顺序相关状态 / §6.3.1 嵌套游标 / §8.2 计数型 FOR / §4 写中间表 | OK | UnsupportedControlFlow |

### 3.5 Hirn & Grust「Compiling PL/SQL Away」（CIDR 2020）

| 探针 | 案例 | Off | 集合化 | 期望 |
|---|---|---|---|---|
| E1.fn / E1.fn0 | `walk()`：步数由数据的提前 RETURN 决定，每步三次查表（含窗口函数） | OK | UnsupportedControlFlow | PASS（1 与 0） |
| E1.drv | 规范基底里逐行调用 `walk()` | OK | UnresolvedFnSite | — |

### 3.6 DBridge（SIGMOD 2017 演示）、QBS（PLDI 2013）、Sloth（SIGMOD 2014）

三篇的案例都是应用侧 Java，这里写成库侧的等价游标循环（按论文描述改写）。

| 探针 | 案例 | Off | 集合化 | 参照 |
|---|---|---|---|---|
| D09.filter | DBridge 图 1：循环里过滤。论文改写成 `WHERE isFinished <> 1` | OK | UnsupportedControlFlow | **一致** |
| D09.exists | DBridge §2.1：用循环判断元组是否存在 | OK | UnsupportedControlFlow | **一致** |
| Q10 | QBS 图 1 `getRoleUser`：嵌套循环写的连接。论文综合出的查询见图 3 | OK | UnsupportedControlFlow | **参照 = 论文 SQL，一致** |
| S11 | Sloth 图 1：四个先后执行的点查，无循环 | OK | UnsupportedControlFlow | — |

### 3.7 SQL-ProcBench（逐字与仅改一处）

| 探针 | 例程 | Off | 集合化 |
|---|---|---|---|
| C0.fn / D0.fn | `sudf_20b` / `sudf_8` **逐字** | **ERR**（名字解析，§5.2） | — |
| D3 | `proc62` 逐字 | **ERR**（`dbms_sql.return_result` 缺件） | — |
| D1.fn / D2.drv / D5.fn | 仅改列别名 / 仅改游标源 / 逐字 | OK | UnsupportedControlFlow |

拒因分布（batch-atomic，70 个 Skipped）：`UnsupportedControlFlow` 45、`UnresolvedFnSite` 10、
`UnsupportedWrite` 6、无判定行 4、`UnprovenWriteCompleteness` 3、`UnsupportedCall` 2。

## 4. 解析层：SQL-ProcBench 整体体检

`spinduck compile <SQL-ProcBench>/src/PLSQL`（`reports/sqlprocbench_compile.txt`）：

| 类别 | 文件数 | 通过 | 失败原因 |
|---|---|---|---|
| Stored Procedures | 45 | 44 | `SET NOCOUNT ON`（T-SQL 残留）×1 |
| Scalar UDFs | 21 | 21 | — |
| Triggers | 6 | 6 | — |
| Table Valued UDFs | 7 | 0 | `BULK COLLECT` ×4、集合方法 `.extend` ×2、PL 表达式里的 `LIKE` ×1 |

解析过了不等于能跑：45 个存过里 21 个用 `SYS_REFCURSOR` + `dbms_sql.return_result` 返回结果集，
后者目前是未支持例程，缺件即红。

## 5. 引擎侧缺口（未修 6 处；除法 4 处 + 中缀 MOD 3 处已于 2026-11-28 修复/显式拒绝）

每条都有入库证据（`reports/evidence/`、`reports/compile_*.log`）。上一轮报告里的 8 处是第 5.2 节的
前三条加 5.4 节；其余由本轮的论文案例暴露。§5.1 的除法 4 处已修复并纳入守护测试
（`tests/guard/guard_runtime_division_gaussdb.rs`）；中缀 MOD 3 处已转编译期显式拒绝（BIZ-0126，
`tests/guard/guard_pl_mod_reject.rs`）；`DUAL` 一条经参照实例核实**不是缺口**
（GaussDB A 模式同样没有 `DUAL`，只有 `sys_dummy`），已从清单移除。

### 5.1 求值层：PL 除法与中缀 MOD 已对齐参照（2026-11-28）

**除法（已修复）**：PL 表达式里的整数除法与定标不符 GaussDB A 模式语义。已在参照实例
（openGauss 5.0.0 容器）实测对齐——整数 `/` 走 float8（15 位有效数字），DECIMAL `/` 走
PostgreSQL `select_div_scale`；修复落在 `runtime/eval/numeric.rs`，期望矩阵入守护测试
（`tests/guard/guard_runtime_division_gaussdb.rs`），G.savings 案例的守护版在
`tests/guard/guard_hirn_grust_savings.rs`。

| 现象（修复前） | 修复前 | 参照实测 | 探针 |
|---|---|---|---|
| 整数相除被截断 | `10 / 3` → `3`；`p_a / p_b`（1, 3）→ `0` | 3.33333333333333 / 0.333333333333333 | X4.int、X4.dec |
| 小数位跟着左操作数走 | `1 / 100.0` → `0.0` | 0.01000000000000000000 | X4.small |
| 上两条的合成 | `(1 - 2 / 3) * 100` → `100` | 33.3333333333333 | X4.expr |
| 真实函数里的占比 | `goto_savings` 的节省比例 3.14 | 3.135917 | G.savings |

**中缀 MOD（已转显式拒绝）**：中缀 `MOD` 在参照的 SQL 与 PL 里**都是语法错误**
（含 `7 MOD (3)`），SpinDuck 却静默吃掉——不是算错，是「该拒绝却静默」。
2026-11-28 起在词元层显式拒绝（BIZ-0126；ogsql-parser 对这些形态是有损解析、
AST 无痕迹，只能在词元层检出，见 `src/gate/dialect.rs`）：

| 现象（拒绝前） | 拒绝前 | 参照（openGauss 5.0.0 实测） | 探针 |
|---|---|---|---|
| 赋值语句里的中缀 `MOD` | `x := (v MOD 3)` → `NULL` | 语法错误（`mod(7,3)` 函数形式才是取余） | X4.mod |
| 经由 SQL 的中缀 `MOD` | `SELECT 7 MOD 3 INTO v` → `NULL` | 语法错误 | X4.modsql |
| RETURN 里的中缀 `MOD` | `RETURN (v MOD 3)` → **没有返回值**（只打印 `CALL f OK`） | 语法错误 | X4.retmod |

字面量上的中缀 `MOD`（`RETURN 7 MOD 3;`）此前经 `ENG-0022` 拒绝，现统一为 `BIZ-0126`
（编译失败的行为不变，错误码与参照语义对齐）；X4.lit 是预期失败的对照探针。

**`%` 取余（已支持）**：参照 `%` 可用（`7 % 3` = 1、`-7 % 3` = -1、`7.5 % 2` = 1.5、
`-7.5 % 2` = -1.5，符号随被除数），本仓此前对 PL 表达式里的 `%` 报 `ENG-0027`。
新增 `BinOp::Mod`：整数走 i64 rem、DECIMAL 走定标后 rem（小数位取操作数较大者），
除零与 `/` 同报 BIZ-0124；SQL 通道 `%` 原生透传内核（X4.pct：PASS）。

### 5.2 名字解析：没区分 SQL 标识符的位置

| # | 现象 | 证据 |
|---|---|---|
| 1 | 参数名与表名同名时，FROM 里的表被替换成变量（`from item` → `FROM $v_item`） | 基准 `sudf_20b` 逐字（C0）；只改参数名即通过（C1） |
| 2 | SELECT 别名与局部变量同名时，别名被替换成变量（`AS cnt` → `AS $v_cnt`） | 基准 `sudf_8` 逐字（D0）；只改别名即通过（D1） |
| 3 | INSERT 列清单里的列名被同名循环变量替换（`result_seq(i, sq)` → `( $v_i , sq )`） | X2 |
| 4 | **表别名被当成同名的循环记录变量**（`supplier s` 与 `FOR s IN …`） | G.late：运行期 `ENG-0044`；体检期可见 `FROM supplier AS $v_s` |

### 5.3 类型与绑定

| # | 现象 | 证据 |
|---|---|---|
| 1 | **DATE 型变量绑定进 SQL 后变成 `Date32(n)` 文本**，内核报 `invalid date field format` | G.margin.fn；PR.hoist（UDF 的 DATE 实参经物化表传入时同样触发） |
| 2 | PL 表达式里的类型转换不支持（`CAST(变量 AS DATE)`），`ENG-0026` | FR.d |

### 5.4 计划面与伪表

| # | 现象 | 证据 |
|---|---|---|
| 1 | SQL 里的裸名函数调用对注册顺序敏感；`--plan` 把入口文件排在最前 | X1.plan 失败，X1.ord（同一例程，被调方文件在前）成功 |
| 2 | 项目态体检拒绝同文件内函数的 SQL 调用 | `p0_staircase.sql`、`a0_xfile_call.sql`、`g01_goto_tpch.sql`、`x3_dual.sql`；仓库自己的 `case_split_agg` 同样 |
| 3 | `dbms_sql.return_result` 未支持 | D3 |
| 4 | `spinduck query` 不注册例程，SQL 里调 UDF 报函数不存在 | B1.q、D5.q |
| 5 | 记录变量声明在游标之前时，`c%ROWTYPE` 被当成缺件 | T0.rf；语料写法（游标在前）无此问题 |
| 6 | **带 OUT 参数的过程不能在区域里调用** | G.margin、G.savings：`UnsupportedCall` |

> 上一版把「`DUAL` 不被当内建表」列为缺口：经参照实例核实（2026-11-28，openGauss 5.0.0），
> GaussDB A 模式同样没有 `DUAL`（`SELECT 1 FROM DUAL` 报错，内建伪表是 `sys_dummy`），
> SpinDuck 的拒绝与参照一致，不再是缺口。

## 6. 差距与论文思路的对应

按「每条能解开多少探针」排序，是我的判断，不是论文结论。

| 差距 | 解开哪些 | 可借鉴的思路 |
|---|---|---|
| 循环内 IF 守卫的单条写入（含过滤） | D09.filter、T2–T2e；QFII 形态 | 谓词下推：`INSERT … SELECT … WHERE cond`（DBridge §2.1 的 selections）；CASE 合并（Froid）；state 表上的谓词屏蔽（CIDR 2024） |
| 无 trap 的区域 | P0 系列、D09.filter、`prc_agg_fund` | 区域内没有跳行出口就等于“全有或全无”：先写暂存表并校验，有问题在零写入时回退（现有 `HybridPreflightFailed` 的口径） |
| 循环里逐行调用函数 | 10 个 `UnresolvedFnSite`（A/B/C/E 的 `.drv`、G.service、G.ship、G.global、FR.q3、X1.ord） | 留在过程侧、对整批行调用（PRISM 的 outlining + CIDR 2024 的 batching）；单语句函数按 Froid 摘要成关系表达式；带循环的函数先按 Aggify 聚合化 |
| 标量 `FETCH INTO v1, v2` 不被识别 | T0.s、A1–A3、A6、D1、D2、`prc_agg_fund` | 纯实现限制：把标量目标映射成 `_cur.<列>` |
| 嵌套游标 | T4、A4、Q10、G.late.b | Aggify §6.3.1 的两遍法；QBS 把嵌套循环识别成连接 |
| 跨迭代累计 | T3、A3、A2 | Aggify 的状态模型 `P_i = f(P_{i-1}, T_i)`；「赋值或保持」对应 `last_value(v IGNORE NULLS) OVER (…)`，求和型对应窗口 `SUM` |
| 键集翻页 + 运行最值 | G.margin | 我为它手写的参照 SQL 就是用窗口函数写的（`ROW_NUMBER` + 运行 `MIN`/`MAX`），可作为该形态的改写模板 |
| 循环内混合写入 | T9、D2.drv、QFII | 先算“每行走到哪一步”的分类表，再按类别各发一条集合语句；逐行 UPDATE 暂存成 `(主键, 新值)` 后 `UPDATE … FROM` |
| 数据依赖步长的 WHILE | E1 | 只有递归 SQL 一条路；论文自己的实测收益有限（见汇总报告），建议划为边界 |

## 7. 局限

- ~~没有 GaussDB 参照侧~~ 已部分补上：§5.1 的除法取值已在参照实例（openGauss 5.0.0 容器）实测
  并对齐（2026-11-28），守护测试锁的是实测值；§5 其余条目的“应该怎样”仍是按 PL/SQL 通常规则讲的，
  未逐条真机对照。
- 参照列只覆盖 10 个探针，其中 6 个的参照是我手写的 SQL（可能写错——`margin` 的参照我就写错过一次，
  修正后与 Off 一致）。
- Aggify 图 1、图 2 是按正文重构的（arXiv 版里代码是位图）。
- `UnsupportedControlFlow` 只给出第一道门的拒因；论文案例过了第一道门之后还会撞上什么，没有逐个追。
- 只测了确定性的小数据，没有测性能，没有构造触发守卫类拒因（Top-1 并列、路由判别式）的数据。
- Hirn & Grust 案例集里 10 个依赖 PostgreSQL 特有类型的函数没有收录。

## 附录 A：证据索引

| 内容 | 位置 |
|---|---|
| 汇总数字 | `reports/summary.txt` |
| 差距矩阵（79 个探针） | `reports/matrix.tsv` |
| 编译期判定（两个工程、两种模式） | `reports/admit_*.txt` |
| SQL-ProcBench 整体解析体检 | `reports/sqlprocbench_compile.txt` |
| DuckDB 机制实测（项目钉住的内核） | `reports/duckdb_mechanisms.txt` |
| 转成功、出错、与参照或期望不符的探针明细 | `reports/evidence/{main,tpch}/` |
| 探针清单与逐探针出处 | `probes.tsv`、`tpch/probes.tsv` |

代码层面的对应位置：

| 结论 | 位置 |
|---|---|
| 识别要求记录写法 | `spinduck/src/ir/set_oriented/region/mod.rs`，`recognize` |
| 跳行 trap 是批写前提 | `spinduck/src/ir/set_oriented/analyze.rs`，`allow_filter` |
| 区域内第二条写即拒 | `spinduck/src/ir/set_oriented/analyze/body.rs`，`statement` |
| IF 覆写门的准入条件 | `spinduck/src/ir/set_oriented/analyze/body/arith_walk.rs` |
| 体检口径（函数级错误不受抑制） | `spinduck/src/gate/prepare.rs` 文件头 |
| 内建表只有 `sys_dummy` | `spinduck/src/verify/deploy.rs`，`ENGINE_PROVIDED_TABLES` |

## 附录 B：复现命令

```bash
(cd ../../spinduck && cargo build --features cli)          # 需先初始化 lib/ogsql-parser 子模块
bash run_probes.sh
SQLPROCBENCH_SRC=<SQL-ProcBench>/src/PLSQL bash run_probes.sh
```

## 附录 C：DuckDB 机制实测（项目钉住的内核，1.5.5）

经 `spinduck query` 在项目实际使用的内核上执行，原始输出在 `reports/duckdb_mechanisms.txt`。

- `TRY(expr)` 可用：把运行期转换错误转成 NULL（`TRY(CAST('x' AS INTEGER))` 得 NULL）。
  但它返回的 NULL 与表达式本身的 NULL 无法直接区分；我试的哨兵写法
  `TRY(COALESCE(expr, 哨兵)) IS NULL` 对真 NULL 同样判为错误，不成立。
- `(DO NOT THROW expr)` 是语法错误。
- `last_value(v) IGNORE NULLS OVER (…)` 是语法错误；可用写法是 `last_value(v IGNORE NULLS) OVER (…)`，
  `lag(v IGNORE NULLS) OVER (…)` 也可用。
- 窗口 `SUM … ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` 可用。
- `WITH RECURSIVE … USING KEY` 配 `UNION ALL` 和配 `UNION` **都可以**；同键覆盖，每个键只留最终状态。
  （上一版报告写“必须配 UNION”，那是在 1.4.4 上测出的结论，在项目钉住的内核上不成立。）
