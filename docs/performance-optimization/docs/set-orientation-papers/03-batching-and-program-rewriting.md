# 路线三：跨边界批处理与程序静态重写（Batching & Program Rewriting）

本专题聚焦过去几年数据库系统界极具实战意义的转向——**从“强行把一切展开为巨型 SQL 内联”转向“保留过程控制流结构，但以批（Batch）为单位集合化下推”**。这一范式与 SpinDuck 现有 `WriteBuffer`、`LookupExecutor` 及 `FnSite` 下沉架构最为契合，实施风险最低、收益最可控。

---

## 1. Dear UDFs, Inlining isn't working out... Let's try Batching（CIDR 2024）

- **论文**：*Dear User-Defined Functions, Inlining isn't working out so great for us. Let's try batching to make our relationship work. Sincerely, SQL*（**CIDR 2024**）
- **作者**：Kai Franz, Samuel Arch, Denis Hirn, Torsten Grust, Todd C. Mowry, Andrew Pavlo（CMU & 图宾根大学 & DuckDB 团队）

### 颠覆性核心论点
1. **为什么 Inlining 会在复杂过程上失效**：
   - 将多语句 UDF 全量内联为嵌套子查询 / `LATERAL` 后，语句内部原本清晰的顺序边界消失，转变成包含大量关联列和条件表达式的巨型关系树；
   - 优化器在尝试对其进行去相关（Decorrelation）与 Join 重排时，优化时间急剧飙升，且极易产出退化的执行计划甚至编译失败。
2. **什么是 UDF Batching（批处理执行）**：
   - **保留原过程逐语句（Statement-by-statement）的执行结构**，不再试图把整个函数揉成一棵巨型单 SQL 树；
   - 取而代之的是，把外层查询流入的所有参数行收集为一个**参数批次表（Batch Table, 如 2,048 或 10,000 行）**；
   - 过程内部的**每一条 SQL 语句**依次接收整个 Batch Table 作为输入，通过标准 Join / GroupBy / Filter 等**集合算子一次性处理整批数据**，把结果写回 Batch Table 的对应列中。
3. **震撼的实证对比**：
   - 在简单 UDF 上，Inlining 依然最优（无物化 Batch 表的开销，且能穿透做谓词下推）；
   - 在**复杂/深嵌套 UDF 负载**上，**Batching 最高比 Inlining 快达 93.4×**，因为它把优化器每次面临的问题规模从“整函数联合去相关”降维成了“逐条单语句的平凡 Join”。
   - 论文最终主张：**混合策略（Hybrid）**——简单表达式用 Inlining，复杂/带状态过程用 Batching。

### 对 SpinDuck 的直接落地启示
SpinDuck 的架构天然靠近 **Batching**，但目前只做了一半，CIDR 2024 指明了另外一半的巨额红利：
1. **完善 `FnSite` 批量下沉**：SpinDuck 已有 `FnSite` 元组下沉（`MAX_TUPLES=100,000`），但仅在 SQL 上下文触发；当游标循环内调用复杂自定义函数时，可将游标窗口（如每 2,048 行）打包成批次表一次性下沉求值，避免建删 10 万次临时表。
2. **`WriteBuffer` 协议升级**：当前 `--batch-commit` 虽然攒了 10,000 行，却仍拼接成超长 SQL 文本让 DuckDB 解析。改用 **`duckdb::Appender` 列式二进制批写入**，并让带 `fn_sites` 的 `INSERT` 先在 Rust 侧预求值再入批，正是纯正的 Batching 思想。
3. **同构 `UPDATE` 攒批**：循环内按主键逐行 `UPDATE` 暂存为 `TEMP TABLE (pk, new_cols)`，刷盘时执行单条集合式 `UPDATE target SET ... FROM tmp WHERE target.pk = tmp.pk`。

---

## 2. Sloth：基于惰性求值的动态跨边界合批（SIGMOD 2014）

- **论文**：*Sloth: Being Lazy Is a Virtue (When Issuing Database Queries)*（**SIGMOD 2014**）
- **作者**：Alvin Cheung, Samuel Madden, Armando Solar-Lezama（MIT）

### 核心机制
1. **延迟物化（Lazy Evaluation）**：当命令式代码发出数据库查询请求时，并不立即阻塞并同步发往数据库，而是返回一个轻量“Future / 延迟代理对象”。
2. **依赖链追踪与合并**：直到程序真正需要读取该结果用于分支判断、输出或写入前，才触发求值；此时把积累在循环或跨函数调用中的多个查询统一合并为一个批量查询或连接查询下发。
3. **优势**：无需全量静态证明循环可 SQL 化，哪怕循环内夹杂复杂控制流，也能把大量零散往返压缩为少量批次往返。

### 对 SpinDuck 的直接启示
- 在 SpinDuck 游标循环内部，往往先从多张不同参数表各做一次点查（当某张表因区间条件复杂未命中 `LookupExecutor` 时，就会退化为逐行 SQL）。
- 借鉴 Sloth 思路，可在进入游标主循环前，**对游标主驱动表提前预取（Prefetch）前瞻窗口键集合**，一次性批量探测未进内存缓存的维表，再推进循环体执行。

---

## 3. DBridge：静态程序重写实现集合化与异步批提交（ICDE 2011 / SIGMOD 2017）

- **论文 1**：*DBridge: A Program Rewrite Tool for Set-Oriented Query Execution*（**ICDE 2011**）
- **论文 2**：*DBridge: Translating Imperative Code to SQL*（**SIGMOD 2017**）
- **作者**：K. Venkatesh Emani, Karthik Ramachandra, Mahendra Chavan, Ravindra Guravannavar, S. Sudarshan（IIT Bombay & Microsoft Research）

### 核心技术栈
DBridge 构建了完整的编译器式静态分析流水线来消除应用与存储过程中的“N+1 循环查询与逐行更新”：
1. **控制流与数据流图分析（CFG / DFG）**：精确识别循环体内哪些语句存在循环携带依赖（Loop-Carried Dependence），哪些语句彼此独立。
2. **循环裂变（Loop Fission / Splitting）**：
   - 当一个循环内部既包含“可集合化的批量查询/写入”，又包含“必须逐行处理的副作用（如日志或异常跳过）”时，DBridge 将原循环**拆分为两个前后相继的循环**：
   - 第一个循环负责批量/集合化获取或计算结果并放入内存缓冲；
   - 第二个循环仅处理无法批量化的残余逻辑。
3. **跨边界异步批提交与集合改写**：
   - 将循环内的重复单行 `INSERT` / `UPDATE` 自动合并为 JDBC Batch / 表值参数；
   - 将可证明无跨轮依赖的只读循环直接重写为一条等价 SQL。

### 对 SpinDuck 的直接启示
SpinDuck 处理的金融存储过程常因 **“2% `A999` 异常跳行 + 多表落盘”** 导致无法整段下推。DBridge 的 **Loop Fission（循环裂变）** 给出了优雅解法：
- **阶段 A（只读求值与维表匹配裂变前置）**：把游标 `SELECT` 与多个无写入依赖的维表查找先裂变为一条 `SELECT ... LEFT JOIN dim1 ... LEFT JOIN dim2 ...` 大宽表流式拉取（由 `RowStream` 按 1,024 行分块提供）；
- **阶段 B（纯本地计算与异常隔离写入）**：Rust 解释器从 1,024 行流中直接读取已经 Join 好的维表字段，本地完成日期/费率运算后送入 `WriteBuffer`（`Appender` 批写 + 异常行回退单行 `BatchSkip`）。
- 这样**既 100% 保留了 `A999` 异常行精确跳过语义（Golden SHA-256 逐字节一致），又消除了循环内全部参数表 SQL 往返**！

---

## 4. QFusor 与 WeBridge：算子融合与过程合成补充视角

| 系统 | 来源 | 核心技术 | 对 SpinDuck 的参考边界 |
| :--- | :--- | :--- | :--- |
| **QFusor** | ICDE / EDBT | 将标量/聚合/表值 UDF 与 SQL 算子融合，并借助 Tracing JIT（PyPy）消除边界 | 验证了“多函数管道跨边界序列化是主要杀手”；SpinDuck 作为 Rust 原生编译引擎，通过本地化 `eval.rs` + `Appender` 可零依赖获得类似融合收益 |
| **WeBridge** | SJTU IPADS (2023) | 通过混合符号执行与推测执行，把零散查询/更新自动合成为高吞吐存储过程 | 印证了将“多条独立 DML / 查询合并为一次跨边界批请求”在真实 OLTP/批处理负载中的普适价值 |

---

## 5. 面向 SpinDuck 的集合化工程决策清单（P0 – P2）

根据以上四篇核心批处理与重写论文，SpinDuck 在 P0–P2 阶段的集合化改造应严格遵循如下优先级决策树：

1. **优先做廉价消除（P0）**：`sys_dummy` 编译期折叠（Froid 简化版）+ 静态提取写表名与 `TraceNoop` 零拷贝（消除无谓边界调用与分词扫描）。
2. **局部计算轮廓提取留驻 Rust（P1 · PRISM Outlining）**：高频日期/字符串函数留在 `eval.rs` 本地直算 + `SpinValue::Date`。
3. **写路径全面 Batching（P1 · CIDR 2024）**：`WriteBuffer` 接入 `duckdb::Appender` 列式二进制通道，`INSERT fn_sites` 先本地预求值再入批。
4. **读-算-写循环裂变与集合下推（P2 · DBridge + Aggify + DuckDB `USING KEY`）**：
   - 模式 A（主游标 + 纯维表查 + 异常处理写）：**Loop Fission**，维表查找前置并入主游标 `LEFT JOIN`，循环体仅剩 Rust 本地运算 + `Appender` 批写；
   - 模式 B（同构主键 `UPDATE`）：攒批为 `TEMP TABLE + UPDATE FROM`；
   - 模式 C（纯集合插入或状态试算无异常跳行）：直接下推为 `INSERT ... SELECT ... JOIN` 或 DuckDB 原生 **`WITH RECURSIVE ... USING KEY`**，彻底绕过 Rust 逐行解释。
