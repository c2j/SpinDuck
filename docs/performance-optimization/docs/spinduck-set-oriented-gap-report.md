# SpinDuck 集合化差距报告：论文调研与探针验证

> **日期**：2026-11-28（第 3 版：按 `docs/performance-optimization/docs/papers/` 收集的论文逐篇补齐案例并评估）
> **被测**：`spinduck 0.4.0`，`git_head=229c039`（`spinduck/src` 无未提交改动）
> **范围**：游标循环及循环内逻辑 → 集合化 SQL 的**可转换面**，以及论文案例暴露出的解释器正确性问题。
> 不含性能；没有 GaussDB 参照侧。
> **证据**：探针包 `golden/paper_set_oriented_probes/`，复现命令
> `bash golden/paper_set_oriented_probes/run_probes.sh`（约 3 分钟）。逐探针明细见包内 `REPORT.md` 与 `reports/`。

---

## 1. 摘要

1. **最要紧的发现不在集合化，在解释器的数值语义——已全部对齐参照（2026-11-28）。**
   修复前 PL 表达式里的除法算错且不报错：`10 / 3` 得 `3`，`1 / 100.0` 得 `0.0`，`(1 - 2 / 3) * 100`
   得 `100`。已按 GaussDB 参照实例（openGauss 5.0.0）实测对齐：整数 `/` 走 float8（15 位有效数字），
   DECIMAL `/` 走 PostgreSQL `select_div_scale`；期望矩阵入守护测试
   （`tests/guard/guard_runtime_division_gaussdb.rs`）。中缀 `MOD` 的定性修正：参照上 SQL 与 PL
   都是**语法错误**，SpinDuck 原静默返回 `NULL` / 无返回值——已转编译期显式拒绝（BIZ-0126，
   `tests/guard/guard_pl_mod_reject.rs`）；`%` 取余按参照实测支持（`7 % 3` = 1，符号随被除数）。
2. **集合化这一层，论文与基准的案例仍然 0 个可转换。** 78 个探针里 batch-atomic 下 `Converted` 6 个，
   全是定标探针和仓库自己的测试夹具；strict 模式下是 0。
3. **论文案例第一次提供了不依赖 Off 自身的参照。** 10 个带参照的探针里 Off 的结果全部一致
   （除法修复前 9 个；不一致的那 1 个 `G.savings` 修复后已对齐），其中 3 个的参照是论文作者写的 SQL。
4. **逐篇评估的结论（§7）**：值得完整支持的是四件事——循环内 IF 守卫的写入、无异常块的循环、
   把被调函数留在过程侧对整批行调用、以及 Froid 式的标量函数内联；不建议投入的是“把循环编译成
   递归 SQL”（三篇论文都指向它，但论文自己的实测收益有限）。DBridge、QBS、Sloth 的案例是应用侧
   Java，对 SpinDuck 有意义的是它们识别出的关系模式，不是工具本身。
5. **论文案例还暴露出 6 处新的引擎缺口**，加上此前的 8 处共 14 处（§6.4）。截至 2026-11-28：
   除法 4 处已修复；中缀 MOD 3 处已转编译期显式拒绝；`DUAL` 一条经参照核实**不是缺口**；未修
   6 处。其中三条不修就没法往下走：表别名被当成循环记录变量、DATE 型变量绑定进 SQL、
   PL 表达式里的类型转换。
6. **收集的论文里有一份收错了**：`05_one_with_recursive_2021.pdf` 是一篇数学论文
   （arXiv:2103.05966，分数 Kolmogorov 方程），不是 Hirn & Grust 的 *One WITH RECURSIVE is Worth
   Many GOTOs*；对应的翻译笔记顶部那段摘要是按标题写的。`06_fixation_on_fixpoints_2023` 只有笔记、
   没有 PDF。这两篇我用论文配套的源码仓库和公开资料补上了（§3）。

## 2. 论文目录核查

`docs/performance-optimization/docs/papers/` 下 9 份 PDF、10 份翻译笔记。我只读，没有改动。

| 文件 | 核查结果 |
|---|---|
| `01_froid_2018.pdf` | 正确。arXiv 扩展版。图 1 的 T-SQL 与第 11 节的真实负载 UDF 可读 |
| `02_prism_2024.pdf` | 正确。PVLDB 18(1) |
| `03_aggify_2020.pdf` | 正确。arXiv 扩展版。图 1、图 2 的代码仍是位图，按正文重构 |
| `04_compiling_plsql_away_2020.pdf` | 正确。图 3 的 `walk()` 代码在 PDF 里是位图，但配套源码仓库里有逐字版本 |
| `05_one_with_recursive_2021.pdf` | **收错**：内容是 arXiv:2103.05966，一篇数学论文。正确论文的配套源码在 GitHub（见 §3.2） |
| `06_fixation_on_fixpoints_2023` | 只有翻译笔记，没有 PDF；笔记的“前两页预览”是空的 |
| `08_inlining_vs_batching_2024.pdf` | 正确。CIDR 2024 |
| `09_dbridge_2017.pdf` | 正确。SIGMOD 2017 演示论文，4 页 |
| `10_qbs_2013.pdf` | 正确。PLDI 2013 |
| `11_sloth_2014.pdf` | 正确。SIGMOD 2014 |

## 3. 论文调研

### 3.1 检索方式与覆盖

用 arXiv API 按摘要词、标题词、作者分别检索并逐条筛。结论：arXiv 上这个方向覆盖率很低，作者索引很弱
（`au:Hirn AND au:Grust` 只返回 1 篇）；图宾根组、CMU 组的核心论文不在 arXiv；arXiv 上没有专门做
「游标循环 → 集合 SQL」的 LLM 重写论文。你收集的 PDF 补上了其中大部分。

### 3.2 逐篇核实结果

只列读过全文的。“支持边界”一栏是核实到的事实。

| 论文 | 出处 | 支持边界（已核实） | 论文自证的适用面与收益 |
|---|---|---|---|
| **Froid** | arXiv:1712.00498，PVLDB 2018 | 支持构造只有 DECLARE/SET、SELECT、IF/ELSE、RETURN、嵌套 UDF、EXISTS/ISNULL。**没有循环，没有异常** | Azure 前 100 个库共 85,329 个标量 UDF，59.8% 可内联；两个客户负载 85% 与 92.5%。逐行调用时快 1–3 个量级 |
| **PRISM** | PVLDB 18(1) | 复杂片段不内联，留在过程侧；只内联对优化器有益的片段；把 WHERE 里的 UDF 谓词提升 | 相对 Froid 式内联平均快 1.29×（DuckDB）与 298.73×（SQL Server）。循环内联成递归 CTE 的 Q14 比过程式执行慢 18 倍 |
| **Aggify** | arXiv:2004.05378，SIGMOD 2020 | 定理 4.2 只覆盖**不修改数据库持久状态**的游标循环。支持嵌套游标、`TRY…CATCH`、对局部表的 DML；带 ORDER BY 时要求顺序保持 | Azure 约 5,720 个库里 77,294 个 UDF 内的游标全部可改写；Adempiere 的 25 个文件 107 个游标循环里 78 个可改写（表 2 逐行合计）。TPC-H 上 Q2/Q14/Q18/Q21 至少快一个量级；真实负载 L1–L8 多数快 2–22×，**含大量临时表插入的 L2、L6 收益很小** |
| **Compiling PL/SQL Away** | arXiv:1909.03291，CIDR 2020 | 只解决函数求值的上下文切换；全文不涉及游标、INSERT、commit | 递归 SQL 相对过程式执行省的运行时间“≥ 40%”，且**调用次数 × 迭代次数 ≥ 32 之后才稳定划算** |
| **One WITH RECURSIVE…** | SIGMOD 2021（PDF 收错，读的是配套源码） | 把任意 WHILE/GOTO/分支编译成 `WITH RECURSIVE`；要求函数是**不改库的纯函数** | 配套仓库 18 个真实世界函数，8 个基于 TPC-H，10 个依赖 PostgreSQL 特有类型（几何、数组、jsonb） |
| **Batching** | CIDR 2024 | 把 UDF 逐语句翻成对 `state` 表（每次调用一行）的 `UPDATE`，条件路径用 `WHERE p AND v` 屏蔽。**循环与 GOTO 引用了别的论文，自己不处理** | 简单 UDF 上内联更好；复杂 UDF 上批处理比内联最高快 93.4× |
| **DBridge 2017** | SIGMOD 2017 演示 | 应用侧 Java（JDBC/Hibernate）。识别循环里的过滤、投影、连接、聚合、嵌套循环实现的 group by、存在性判断、循环内的嵌套标量查询。**只翻译读操作，不翻译更新**；全文没有“循环裂变” | 图 1 的程序在 10 万行、20% 选择率下快 2.3×、传输量少 2.5× |
| **QBS** | PLDI 2013 | 应用侧 Java。用循环不变量 + 程序综合把只读循环翻成 SQL。**内核语言不建模 NULL 三值逻辑，也不建模更新** | 两个开源项目里 75 个代码片段，量级提升 |
| **Sloth** | SIGMOD 2014 | 应用侧 Java。惰性求值 + 延迟合批，优化的是**应用与数据库之间的网络往返** | 页面加载时间中位数快 1.3–2.2×，最高 3.1× |
| SQL-ProcBench | PVLDB 14(8) 2021 的配套基准（论文本身未收集） | 真实世界过程式 SQL 的使用情况研究 + 三种方言的基准 | — |

### 3.3 对现有综述的修正

`docs/performance-optimization/docs/set-orientation-papers/` 里有五处需要更正：

| 位置 | 现有说法 | 核实结果 |
|---|---|---|
| `02-…md` §1 | SpinDuck 可“利用 Rust 侧已编译好的 IR/本地函数注册为 DuckDB 表/聚合回调” | **做不到**：项目钉住的 `duckdb` crate `=1.10505.0` 源码里只有标量函数与表函数注册 API，没有任何聚合函数接口 |
| `02-…md` §3 | `USING KEY` 一节称其打通了从 IrProg 状态机到 DuckDB 递归 CTE 的工程通道 | 机制属实，且已在项目内核上实测可用；但 PRISM 与 Compiling PL/SQL Away 的实测都说明递归 SQL 相对过程式循环的收益有限（§3.2） |
| `README.md` 横评矩阵 | Compiling PL/SQL Away 是“全量下沉 DuckDB 的理论原型” | 该文不涉及游标、INSERT、commit，对循环痛点没有直接指导 |
| `03-…md` §3 | “DBridge: Translating Imperative Code to SQL（SIGMOD 2017）”的作者是 Chavan、Guravannavar 等 | 这一篇的作者是 Emani、Ramachandra、Deshpande、Sudarshan。Chavan 等是它引用的 ICDE 2011 演示论文的作者 |
| `03-…md` §3 | DBridge 提出“循环裂变（Loop Fission / Splitting）” | **2017 这一篇全文没有这个说法**（“fission”“loop split”零命中）。它只翻译读操作，不翻译更新。“先裂变再批写”是综述自己的引申。它的完整技术在另一篇 SIGMOD 2016 论文里，那篇没有收集，我没有核实 |

另：SIGMOD Record 2026-03 的 “Partial UDF Inlining” 是 PRISM 那篇的修订版，不是新论文。

## 4. 探针验证

两个独立工程、78 个探针（76 个例程调用 + 2 个查询通道），每个跑三轮：Off → batch-atomic → strict。

| 工程 | 探针数 | 数据 | 内容 |
|---|---|---|---|
| `main` | 63 | 手工种子 | 定标阶梯；Froid 图 1；Aggify；`getManufact`；`walk()`；DBridge / QBS / Sloth 的模式；SQL-ProcBench 逐字例程；引擎侧探针 |
| `tpch` | 15 | SQL 生成的 TPC-H 形状数据（8000 张订单、24000 行明细） | Hirn & Grust 案例集里基于 TPC-H 的函数；Froid §11.1 的 TPC-H 谓词 UDF；PRISM 的谓词提升场景 |

代码来源分四档，在每个探针的清单行和源文件头都标了：

| 档 | 对象 |
|---|---|
| 逐字 | Froid 图 1 与 §11.1；Hirn & Grust 案例集的 `walk()`；SQL-ProcBench 的 4 个文件；仓库测试夹具 `caller_dummy` |
| 仅改 X | 为绕开一个**已定位的引擎缺口**做的最小改动（§6.4 的每一条都有对应的“逐字版失败、仅改版通过”） |
| 按论文改写 | 原文是 PL/pgSQL、T-SQL 或应用侧 Java，按同一语义改写成 GaussDB PL/SQL |
| 自造 | 定标阶梯与引擎侧探针 |

**可信度**：最简规范基底 T0 与仓库夹具 T2.f 在 batch-atomic 下都是 `Converted` 且与 Off 相等，探针能识别“转成功”。

**三列口径，不要混读**：

- `ba_eq`：集合化与 Off 是否逐值相等。76 个里 65 个相等，其余 11 个是 Off 自身报错、两种模式退出码一致。
- `off_vs_oracle`：Off 的结果与参照 SQL 是否一致。参照分“论文给的”（3 个）和“我手写的”（7 个）。
- `off_vs_expect`：与手写期望值是否一致，期望按 Oracle NUMBER 语义写。

## 5. 解析层：SQL-ProcBench 整体体检

`spinduck compile <SQL-ProcBench>/src/PLSQL`：

| 类别 | 文件数 | 通过 | 失败原因 |
|---|---|---|---|
| Stored Procedures | 45 | 44 | `SET NOCOUNT ON`（T-SQL 残留）×1 |
| Scalar UDFs | 21 | 21 | — |
| Triggers | 6 | 6 | — |
| Table Valued UDFs | 7 | 0 | `BULK COLLECT` ×4、集合方法 `.extend` ×2、PL 表达式里的 `LIKE` ×1 |

解析过了不等于能跑：45 个存过里 21 个用 `SYS_REFCURSOR` + `dbms_sql.return_result` 返回结果集，后者是未支持例程。

## 6. 发现

### 6.1 集合化可转换形状的边界

与第 2 版一致，本轮新增的论文案例没有改变它。规范基底 T0 = FOR-IN + 跳行 trap 内一条 INSERT。
只加一个特性就让转换失败的：标量 `FETCH INTO v1, v2`（第一道门 `recognize` 只认整行记录）、去掉跳行 trap
（`allow_filter = BatchAtomic && has_trap`）、NULL 字面量、循环内 IF 赋值（只认夹具那一种组合）、
跨迭代累计、嵌套游标、点查不带异常块、循环内每行 UPDATE、循环里逐行调用函数。
逐项的探针与原因见包内 `REPORT.md` §2。

### 6.2 逐篇论文：案例与当前支持

batch-atomic 下的集合化判定。「Off 对不对」看参照/期望列。

| 论文 | 案例 → 探针 | Off 能跑吗 | Off 对不对 | 集合化 |
|---|---|---|---|---|
| Froid | 图 1 `total_price` → B1.fn/B1.drv/B1.s/B1.q；§11.1 谓词 UDF → FR.price.fn/FR.q3/FR.d | B1.q（`query` 通道）与 FR.d（PL 类型转换）跑不了，其余能跑 | FR.q3 500 行与参照一致；FR.price.fn 期望通过 | 被调函数在循环里：UnresolvedFnSite |
| PRISM、Batching | `getManufact` → C1.*；谓词提升 → PR.hoist | PR.hoist 跑不了（UDF 的 DATE 实参） | — | UnresolvedFnSite |
| Aggify | 图 1、图 2 → A1.*、A2.*；§4/§6.1/§6.3.1/§8.2 → A3–A6 | 能跑 | 无参照 | UnsupportedControlFlow / UnresolvedFnSite |
| Compiling PL/SQL Away | `walk()` → E1.* | 能跑 | 期望通过 | UnsupportedControlFlow / UnresolvedFnSite |
| One WITH RECURSIVE… | 案例集 6 个 TPC-H 函数 → G.* | 两个忠实版跑不了（别名撞名、DATE 绑定） | **5 个与参照一致（2 个对照论文 SQL），1 个不一致** | 0：UnresolvedFnSite 3、UnsupportedControlFlow 5、UnsupportedCall 2 |
| DBridge 2017 | 过滤、存在性判断 → D09.* | 能跑 | **与参照一致** | UnsupportedControlFlow |
| QBS | 嵌套循环连接 → Q10 | 能跑 | **与论文综合出的查询一致** | UnsupportedControlFlow |
| Sloth | 四个先后点查 → S11 | 能跑 | 无参照 | UnsupportedControlFlow |
| SQL-ProcBench | `sudf_20b`、`sudf_8`、`proc62` 逐字 → C0/D0/D3；三个仅改版 → C1/D1/D2/D5 | 三个逐字版跑不了（名字解析 ×2、`dbms_sql.return_result`） | — | 0 |

拒因分布（两个工程合计，70 个 Skipped）：`UnsupportedControlFlow` 45、`UnresolvedFnSite` 10、
`UnsupportedWrite` 6、无判定行 4、`UnprovenWriteCompleteness` 3、`UnsupportedCall` 2。

### 6.3 对现有语料的影响

黄金语料的 7 个游标循环（结构来自源码；区域 0 的结论来自 `docs/goals/set-oriented-v3.md`）：

| 循环 | 写法 | 主体结构 | 卡在哪一类 |
|---|---|---|---|
| byfund `c_get_data`（区域 0） | 记录 FETCH | 带兜底点查、`sys_dummy` CASE、IF 数值覆写、跳行 trap 内 INSERT | **已可转**（batch-atomic） |
| byfund `c_get_venture_data` + `c_fxj` | 外层记录、内层标量 | 嵌套；内层游标源是运行期拼接的动态 SQL；行内 COMMIT；trap 跨层 GOTO | 嵌套 + 动态源 + 标量 FETCH |
| byfund `c_get_unknown` | 记录 FETCH | 循环体只调日志过程 | 没有写汇，不适用 |
| byfund QFII | FOR-IN | 多次点查（trap 里 `GOTO next_row`）；INSERT 之后 UPDATE 另一张表 | 循环内条件写入 + 混合写入 |
| deal_purchase `c_get_purchase` | 记录 FETCH | 点查；数据相关的 IF；跳行 trap 内 INSERT | 条件写入（结论依赖数据） |
| `prc_agg_fund`（case_split_agg） | 标量 FETCH | INSERT 里调函数；无跳行 trap | 标量 FETCH + 无 trap + 函数调用 |

**考题语料（`lib/fastaas`，54 个例程）我没有接触**——它是私有子模块，本工作树未初始化。

### 6.4 引擎侧缺口（未修 6 处；除法 4 处 + 中缀 MOD 3 处已于 2026-11-28 修复/显式拒绝，`DUAL` 一条撤销）

每条都有入库证据（`golden/paper_set_oriented_probes/reports/evidence/`）。前 6 处是本轮新增。

**求值层：除法与中缀 MOD 已对齐参照（2026-11-28）**

除法 4 处已于 2026-11-28 按参照实例实测修复（整数 `/` 走 float8、15 位有效数字；DECIMAL `/` 走
`select_div_scale`；`G.savings` 占比对齐到 3.135917），期望矩阵入守护测试，此处只留修前记录：

| 现象（修复前） | 修复前 | 参照实测（openGauss 5.0.0） | 探针 |
|---|---|---|---|
| 整数相除被截断 | `10 / 3` → `3`；`p_a / p_b`（1, 3）→ `0` | 3.33333333333333 / 0.333333333333333 | X4.int、X4.dec |
| 小数位跟着左操作数走 | `1 / 100.0` → `0.0` | 0.01000000000000000000 | X4.small |
| 上两条的合成 | `(1 - 2 / 3) * 100` → `100` | 33.3333333333333 | X4.expr |
| 真实函数里的占比 | `goto_savings` 的节省比例 3.14 | 3.135917 | G.savings |

中缀 `MOD` 的定性修正：参照实例上 SQL 与 PL 里中缀 `MOD` **都是语法错误**（`MOD(7,3)` 函数形式
才是取余），SpinDuck 原静默吃掉——不是算错，是「该拒绝却静默」。2026-11-28 起词元层显式拒绝
（BIZ-0126，`gate/dialect.rs`；ogsql 对这些形态是有损解析、AST 无痕迹）：

| 现象（拒绝前） | 拒绝前 | 参照（openGauss 5.0.0 实测） | 探针 |
|---|---|---|---|
| 赋值里的中缀 `MOD` | `x := (v MOD 3)` → `NULL` | 语法错误 | X4.mod |
| 经 SQL 的中缀 `MOD` | `SELECT 7 MOD 3 INTO v` → `NULL` | 语法错误 | X4.modsql |
| RETURN 里的中缀 `MOD` | `RETURN (v MOD 3)` → 没有返回值 | 语法错误 | X4.retmod |

字面量上的中缀 `MOD`（`RETURN 7 MOD 3;`）此前经 `ENG-0022` 拒绝，现统一为 `BIZ-0126`；
X4.lit 是预期失败的对照探针，不算缺口。`%` 取余已按参照实测支持（新增 `BinOp::Mod`：
`7 % 3` = 1、`-7 % 3` = -1、`7.5 % 2` = 1.5，符号随被除数；**零模 `MOD(n, 0) = n`**
返回被除数而非除零——V59.1 复审修正；除零报错 BIZ-0124 只保留给 `/ 0`；
SQL 通道经 `[patterns] mod_zero_returns_dividend` 改写同口径；X4.pct / X4.pct0：PASS）。
非阻断登记同步：`7 mod +3`（一元 + 不是值起始）已随词元扫描对齐参照（语法错误）；
`i64::MIN % -1` = 0 已特判；runner 期望列支持 `[错误码]` 对账。

**已知差距（#68 复审非阻断登记，2026-11-28，勿「顺手修复」）**

| # | 差距 | 参照 | 本仓 | 状态 |
|---|---|---|---|---|
| ① | 内联链式除法运算（`2/3 + 2/3`） | 1.33333333333333（全程 float8，输出时 %.15g） | 1.333333333333334（每步先固化 15 位再算） | 钉现状守护 `should_pin_inline_chained_division_sum_as_known_gap`；经 NUMBER 变量中转的链式两侧一致（…334） |
| ② | 整数槽直赋除法（`v INTEGER := 1/2`） | 0（float8 → int 截断固化） | 0.5（INTEGER 声明槽不固化类型） | 钉现状守护 `should_pin_integer_slot_direct_assignment_division_as_known_gap` |
| ③ | `rscale > 38` 的除法（如 `1/1e-30` 形） | 按 38 位精度出值 | 显式 BIZ-112（定标上界） | 显式报错符合 AGENTS fail-loud；登记为已知分歧 |
| ④ | SQL 通道除法显示位宽 | `%.15g` | DuckDB shortest | 将来集合化转换区支持除法形状时暴露；SQL 通道现与本仓口径一致，不动 |
| ⑤ | 设计 §4.5 情形 A（PL 整数除法下沉） | — | 已下线 | 设计文档 §3.3 已加注（V58）；恢复下沉需先补 float8 语义证明 |

**名字解析：没区分 SQL 标识符的位置**

| # | 现象 | 证据 |
|---|---|---|
| 1 | 参数名与表名同名时，FROM 里的表被替换成变量 | 基准 `sudf_20b` 逐字（C0）；只改参数名即通过（C1） |
| 2 | SELECT 别名与局部变量同名时，别名被替换成变量 | 基准 `sudf_8` 逐字（D0）；只改别名即通过（D1） |
| 3 | INSERT 列清单里的列名被同名循环变量替换 | X2 |
| 4 | **表别名被当成同名的循环记录变量**（`supplier s` 与 `FOR s IN …`） | G.late：运行期 `ENG-0044`；体检期可见 `FROM supplier AS $v_s` |

**类型与绑定**

| # | 现象 | 证据 |
|---|---|---|
| 1 | **DATE 型变量绑定进 SQL 后变成 `Date32(n)` 文本** | G.margin.fn；PR.hoist（UDF 的 DATE 实参） |
| 2 | PL 表达式里的类型转换不支持（`CAST(变量 AS DATE)`），`ENG-0026` | FR.d |

**计划面与伪表**

| # | 现象 | 证据 |
|---|---|---|
| 1 | SQL 里的裸名函数调用对注册顺序敏感；`--plan` 把入口文件排在最前 | X1.plan 失败，X1.ord 成功 |
| 2 | 项目态体检拒绝同文件内函数的 SQL 调用 | `p0_staircase.sql` 等 4 个文件；仓库自己的 `case_split_agg` 同样 |
| 3 | `dbms_sql.return_result` 未支持 | D3 |
| 4 | `spinduck query` 不注册例程，SQL 里调 UDF 报函数不存在 | B1.q、D5.q |
| 5 | 记录变量声明在游标之前时，`c%ROWTYPE` 被当成缺件 | T0.rf |
| 6 | **带 OUT 参数的过程不能在区域里调用** | G.margin、G.savings |

> 上一版把「`DUAL` 不被当内建表」列为缺口：经参照实例核实（2026-11-28，openGauss 5.0.0），
> GaussDB A 模式同样没有 `DUAL`（`SELECT 1 FROM DUAL` 报错，内建伪表是 `sys_dummy`），
> SpinDuck 的拒绝与参照一致，从缺口清单撤销。

## 7. 逐篇评估：完整支持的可行性与价值

评级口径：

- **可行性**：高 = 现有机制内可完成，不需要新的证明类；中 = 需要新的分析或改写通路，但有现成部件；
  低 = 需要新的框架，或项目钉住的内核能力不够。
- **价值**：高 = 黄金语料里已有实例且论文自证的收益大；中 = 两者有其一；低 = 论文自证的收益有限，
  或本仓库不存在它优化的那项成本。考题语料我没看到，这一列对它不成立。

“论文自证”一栏的数字都出自 §3.2 表的最后一列。

| 论文 | 完整支持 = | 可行性 | 价值 | 判断 |
|---|---|---|---|---|
| **DBridge 2017** | 循环内 IF 守卫的收集/写入 → `WHERE` 过滤；循环判存在 → `EXISTS` | **高**：IF 守卫的单条写入可以降成 `INSERT … SELECT … WHERE cond`，不需要新的证明类。两个已知障碍是无 trap（P0）与循环内 IF（T2） | **高**：循环内条件写入是业务存过最常见的形态之一（QFII 就是）。论文自证快 2.3×，但那是应用侧去掉往返的数字 | 做。是论文案例里最便宜、覆盖面最广的一类 |
| **PRISM、Batching** | 不内联复杂 UDF，把它留在过程侧，对一批行调用一次 | **中偏高**：本仓库已有 FnSite 元组下沉与函数物化前导，集合化计划只是拒绝携带函数调用（`UnresolvedFnSite` 的定义）。等价性风险低——函数仍按原 IR 求值。前置：UDF 的 DATE 实参（PR.hoist） | **中高**：10 个 `UnresolvedFnSite` 探针是它的直接对象。但函数内部仍逐行求值，收益是省掉循环自身的逐行往返。论文自证 1.29×（DuckDB） | 做。是让“调用函数的循环”转起来的最省力的一条 |
| **Froid** | 把多语句标量 UDF（SELECT INTO + IF/ELSE + 嵌套调用 + 多个 RETURN）摘要成关系表达式并入集合写 | **中**：部件大半已有（符号环境、查表摘要、IF 覆写门）；缺的是从“带 WHEN OTHERS 的查表函数”推广到一般多语句函数。前置：PL 类型转换（FR.d）（除法语义已修） | **高**：论文自证 59.8%–92.5% 的真实标量 UDF 都在这组构造内，逐行调用时快 1–3 个量级；本仓库语料里标量函数被逐行调用很普遍（byfund 的区域 0 里就调了两个，其中一个带 4 个 OUT 参数） | 做，排在上两条之后 |
| **Aggify** | 识别“游标循环把各行归约成标量”的函数，改写成聚合或窗口表达式 | **中偏低**：需要跨迭代状态的数据流分析；项目钉住的 `duckdb` crate **没有聚合函数注册接口**，只能用内置聚合与窗口函数组合表达，一般情形落到递归 CTE | **中**：论文自证适用面广，但那些循环不写库，收益来自去掉逐行往返。本仓库的函数多是单次查表，不是游标归约 | 不单独立项；作为 Froid 式内联的前置，等语料里出现这类函数再做 |
| **One WITH RECURSIVE…** | 把数据依赖步长的循环编译成 `WITH RECURSIVE` | **低**（作为优化目标）：需要整套翻译框架 | **低**（作为优化目标）：论文自证要调用次数 × 迭代次数 ≥ 32 才划算，且只省“≥ 40%”；案例集里这类函数多是科学与几何计算。**高**（作为测试资产）：函数 + 作者手写的 SQL + 测试数据，是现成的解释器正确性测试集——本轮的除法缺口就是它揪出来的 | 不作为优化目标。**建议把案例集接成解释器的回归测试**（已接 6 个）；换掉收错的 PDF |
| **Compiling PL/SQL Away** | 同上 | **低** | **低到中**：同上；`walk()` 这类“逐行调用”的收益走 PRISM 路线更划算 | 划为边界 |
| A Fix for the Fixation on Fixpoints | 不适用（没有过程式案例，提供的是 `USING KEY` 机制） | 作为“逐行状态机”的载体：中 | 取决于语料里试算型循环的占比（未知） | 机制本身已在项目内核上实测可用，留给上一行需要时用 |
| **QBS** | 嵌套循环 + 等值 IF → 连接 | **中**：综合与验证太重，可取的是识别模式。它不建模 NULL 三值逻辑与更新 | **中**：嵌套游标在报表类存过里常见（byfund 的 `c_fxj`）。论文自证量级提升 | 做，但按“嵌套游标”这个能力做，不按 QBS 的方法做 |
| **Sloth** | 把多次独立点查合成一次 | **不建议**按惰性求值做：它优化的是网络往返，进程内引擎没有这项成本，且已有查表缓存。S11 这个形态真正的障碍是“点查不带异常块”（T8n） | **低**：论文自证的 1.3–2.2× 全部来自减少往返 | 不做惰性求值；无异常块的点查并入“无 trap 的区域”一起考虑 |

把上面横向归并，论文案例在 SpinDuck 里落到 10 项能力上。这张表比逐篇的表更好排期：

| 能力 | 论文来源 | 卡住的探针 | 当前 | 可行性 | 价值 |
|---|---|---|---|---|---|
| A 循环内 IF 守卫的单条写入（含过滤） | DBridge、Froid、Batching | D09.filter、T2–T2e、QFII | UnsupportedControlFlow | 高 | 高 |
| B 无跳行 trap 的区域 | — | P0 系列、D09.filter、`prc_agg_fund` | UnsupportedWrite | 高 | 高 |
| C 被调函数留在过程侧、对整批行调用 | PRISM、Batching | 10 个 `UnresolvedFnSite` | UnresolvedFnSite | 中偏高 | 中高 |
| D 标量 `FETCH INTO v1, v2` 进入识别 | — | T0.s、A1–A3、A6、D1、D2、`prc_agg_fund` | UnsupportedControlFlow | 高 | 中（黄金语料 7 个循环里 2 个；SQL-ProcBench 14 处全是） |
| E 多语句标量 UDF 内联 | Froid | B1.drv、FR.q3、C1.drv、G.service、G.ship | UnresolvedFnSite | 中 | 高 |
| F 嵌套游标循环 | Aggify §6.3.1、QBS | T4、A4、Q10、G.late.b、`c_fxj` | UnsupportedControlFlow | 中 | 高（`c_fxj`） |
| G 逐语句批计划（state 表 + 谓词屏蔽） | Batching | A–F 的合集 | — | 中偏低（新的改写通路） | 高（中期方向） |
| H 跨迭代累计 | Aggify、Fixpoints | T3、A2、A3、G.margin | UnsupportedControlFlow | 中 | 中 |
| I 无异常块的多点查 | Sloth | S11、T8n | UnsupportedControlFlow | 中 | 中 |
| J 数据依赖步长的循环 → 递归 SQL | Compiling PL/SQL Away、One WITH RECURSIVE…、Fixpoints | E1、G.margin | — | 低 | 低（边界） |

两点说明：

- **G 是唯一能同时解开 A–F 的路**，也是工作量最大的一条。论文的批处理只以只读 UDF 为对象，写操作要另外
  设计保序。建议等 A–E 落地、看清剩余形态之后再决定做不做。
- **动态游标源**（`OPEN c FOR v_sql_txt`，`c_fxj`）不在这 10 项里：收集的论文都假设 SQL 是静态的，
  没有一篇处理它。按取值特化是我的工程设想，不是论文结论，需要先证实取值个数确实很小。

## 8. 建议的顺序

1. **先修正确性，再谈集合化。** 数值语义排第一，**已完成（2026-11-28）**：除法按 GaussDB 实测
   修复（float8 / select_div_scale）、中缀 `MOD` 转编译期显式拒绝（BIZ-0126）、`%` 取余按实测
   支持；守护测试均已入。已知差距（内联链式、整数槽直赋等 5 项）见 §6.4 登记。
   下一批：表别名被当成记录变量、DATE 变量绑定、PL 表达式里的类型转换。
   这些都有逐字或仅改一处的探针对，可以直接当守护测试。
2. **再扩形状，按 §7 的 A → D → B → C → E → F。** A 与 D 最便宜；B 解开的是“没写异常块的普通循环”；
   C 让调用函数的循环转起来；E 与 F 的工作量中等。
3. **把 Hirn & Grust 案例集接成解释器的回归测试。** 现成的函数 + 作者手写的 SQL，是本轮唯一一份
   能评价 Off 对不对的资产。接进来 6 个；`savings` 在除法修好后参照比对已转一致
   （探针包复跑 `off_vs_oracle=EQ`），纳入守护测试已获同意（见 §9 第 4 条）。
4. **明确不做**：惰性求值（Sloth）、把循环编译成递归 SQL、LLM 直接重写进运行时。

每一项动手前都要按 AGENTS.md 先请示的触点：`recognize` 的行为变更、守卫语义变更、新增 `SkipReason`，
以及把探针转成 `tests/guard/` 下的守护测试。

## 9. 需要你决定的事项

1. ~~**数值语义的确认。**~~ **已完成（2026-11-28）**：已在 GaussDB 参照实例（openGauss 5.0.0 容器，
   A 模式）实测——整数相除**不截断**，走 float8（15 位有效数字）；`1 / 100.0` = 0.01；
   中缀 `7 MOD 3` 在 SQL 与 PL 里都是语法错误，`MOD(7,3)` 函数形式 = 1。除法已按实测修复，
   取值矩阵入守护测试（`tests/guard/guard_runtime_division_gaussdb.rs`）。
2. **先修哪一档。** 我的建议是 §8 的顺序。
3. **收错的 PDF。** `05_one_with_recursive_2021.pdf` 需要换成正确的论文；`06` 缺 PDF。
4. **案例集的去向。** ~~要不要把 6 个 TPC-H 函数与论文 SQL 的比对正式纳入 `tests/guard/`？~~
   **已开始落地（2026-11-28）**：`goto_savings`（除法语义的出处案例）已入
   `tests/guard/guard_hirn_grust_savings.rs`（例程与参照 SQL 逐字移植、数据缩小为确定性夹具，
   进 CI）；其余 5 个 TPC-H 函数待后续 PR 逐个纳入。
5. **考题语料的普查。** 我没有 `lib/fastaas` 的访问条件。§7 的价值一列对它不成立，拿到之后应该用
   `compile --admit` 对 54 个例程跑一遍拒因分布，再回来排 A–J 的顺序。
6. **三个“是否在语料里出现”**：带 OUT 参数的过程调用、`dbms_sql.return_result`、`DUAL`。
   不出现就不必做。

## 10. 局限

- 没有 GaussDB 参照侧。§6.4 里“应该怎样”的说法是按 Oracle/PL/SQL 的通常规则讲的，没有在真机上对照。
- 参照列只覆盖 10 个探针，7 个的参照是我手写的 SQL，可能写错——`margin` 的参照我写错过一次，
  修正后与 Off 一致。
- Aggify 图 1、图 2 是按正文重构的；DBridge、QBS、Sloth 的案例是应用侧 Java，库侧写法是我的改写。
- `UnsupportedControlFlow` 只给出第一道门的拒因，论文案例过了第一道门之后还会撞上什么，没有逐个追。
- 只测了确定性的小数据，没有测性能，没有构造触发守卫类拒因的数据。
- Hirn & Grust 案例集里 10 个依赖 PostgreSQL 特有类型的函数没有收录。论文目录的编号到 11 为止，缺 07。
- 2026-11-28 起本报告附带除法语义修复（产品代码：`runtime/eval/numeric.rs` + 除法下沉下线）；
  探针包已用修复后二进制复跑（`summary.txt`/`matrix.tsv` 为修复后数字），全量门禁
  （fmt / clippy / `cargo test --features cli`）通过。

---

## 附录 A：证据索引

| 内容 | 位置 |
|---|---|
| 汇总数字（探针数、Converted、拒因分布、参照与期望的通过情况） | `golden/paper_set_oriented_probes/reports/summary.txt` |
| 差距矩阵（78 个探针，逐探针） | `golden/paper_set_oriented_probes/reports/matrix.tsv` |
| 编译期判定（两个工程 × 两种模式） | `golden/paper_set_oriented_probes/reports/admit_*.txt` |
| SQL-ProcBench 整体解析体检 | `golden/paper_set_oriented_probes/reports/sqlprocbench_compile.txt` |
| DuckDB 机制实测（项目钉住的内核） | `golden/paper_set_oriented_probes/reports/duckdb_mechanisms.txt` |
| 转成功、出错、与参照或期望不符的探针明细 | `golden/paper_set_oriented_probes/reports/evidence/{main,tpch}/` |
| 探针清单与逐探针出处 | `golden/paper_set_oriented_probes/probes.tsv`、`tpch/probes.tsv` |

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
(cd spinduck && cargo build --features cli)          # 需先初始化 lib/ogsql-parser 子模块
bash golden/paper_set_oriented_probes/run_probes.sh
SQLPROCBENCH_SRC=<SQL-ProcBench>/src/PLSQL bash golden/paper_set_oriented_probes/run_probes.sh
```

探针包里的案例来自三个外部来源，都没有提交进仓库，按需下载：

- Hirn & Grust 案例集：<https://github.com/One-WITH-RECURSIVE-is-Worth-Many-GOTOs/Code>
- SQL-ProcBench：<https://github.com/microsoft/SQL-ProcBench>
- 仓库测试夹具：`spinduck/tests/set_oriented_dummy.rs`

## 附录 C：DuckDB 机制实测（项目钉住的内核，1.5.5）

经 `spinduck query` 执行，原始输出在 `reports/duckdb_mechanisms.txt`。

- `TRY(expr)` 可用，把运行期转换错误转成 NULL。但它返回的 NULL 与表达式本身的 NULL 无法直接区分：
  `TRY(COALESCE(expr, 哨兵)) IS NULL` 对真 NULL 同样判为错误。
- `(DO NOT THROW expr)` 是语法错误。
- `last_value(v) IGNORE NULLS OVER (…)` 是语法错误；可用写法是 `last_value(v IGNORE NULLS) OVER (…)`，
  `lag(v IGNORE NULLS) OVER (…)` 与窗口 `SUM` 也可用。
- `WITH RECURSIVE … USING KEY` 配 `UNION ALL` 和配 `UNION` 都可以；同键覆盖，每个键只留最终状态。
  （第 2 版写“必须配 UNION”，那是在 1.4.4 上测的，在项目内核上不成立。）
