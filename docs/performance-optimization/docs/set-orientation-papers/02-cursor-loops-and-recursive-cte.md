# 路线二：游标循环消除与递归 CTE 集合化（Cursor Loops & Recursive CTEs）

针对 SpinDuck 中最核心的痛点——**`FOR rec IN cur LOOP ... END LOOP` 逐行游标迭代（十万行耗时 316.2s）**，学术界提供了两条不依赖外部机器码 JIT、直接把循环变成数据库单次集合执行的深度方案：**自定义聚合合成（Aggify）** 与 **Trampoline / `USING KEY` 递归 CTE 编译**。

---

## 1. Aggify：将游标循环合成为自定义聚合（SIGMOD 2020）

- **论文**：*Optimizing Cursor Loops in Relational Databases*（`arXiv:2004.05378`，亦称 *Aggify*）
- **作者**：Surabhi Gupta, Sanket Purandare, Karthik Ramachandra（Microsoft Research India）

### 解决的痛点（"The Curse of Cursors"）
游标循环在数据库内部强制采用 tuple-at-a-time 执行模型，导致三大恶果：
1. 无法享受向量化流水线执行；
2. 迫使游标结果集先物化到临时存储，增加 I/O 与内存开销；
3. 长时间持有行锁/表锁，拖垮并发吞吐。

### 核心技术路径
1. **循环依赖与状态分析**：解析游标 `SELECT` 驱动的循环体，识别其中跨迭代累加/更新的状态变量（如余额累加器、阶段计数、条件聚合指标）。
2. **自定义聚合函数（UDAF）合成**：
   - `Initialize()` ← 循环前局部变量初始化；
   - `Accumulate(row)` ← 循环体单次迭代逻辑；
   - `Merge(s1, s2)` ← 若满足结合律/可分区键独立性（按 `PARTITION BY` 键分组），自动支持并行归约；
   - `Terminate()` ← 循环退出后的结果输出。
3. **查询重写**：将整段命令式游标循环重写为一条基于该 UDAF 的 `SELECT agg_loop(...) FROM src GROUP BY ...` 集合查询，彻底消除游标物化并把计算推入关系流水线。

### 对 SpinDuck 的落地映射
- **高匹配场景**：按资金账号/产品代码分组顺序计算累计净值、分段费率或仅输出汇总记录的游标过程（例如 `prc_trd_hz_byfund` 类型负载）。
- **DuckDB 1.5.5 结合点**：SpinDuck 可利用 Rust 侧已编译好的 IR/本地函数注册为 DuckDB 表/聚合回调，或对常见“逐行累算 + 单行产出”模式生成窗口函数（`SUM() OVER (PARTITION BY ... ORDER BY ...)`）替代手写游标。

---

## 2. Compiling PL/SQL Away & One WITH RECURSIVE is Worth Many GOTOs

- **论文 1**：*Compiling PL/SQL Away*（**CIDR 2020**，`arXiv:1909.03291`），Christian Duta, Denis Hirn, Torsten Grust
- **论文 2**：*One WITH RECURSIVE is Worth Many GOTOs*（**SIGMOD 2021**），Denis Hirn, Torsten Grust（图宾根大学）

### 核心哲学
传统观点认为“PL/SQL 慢是因为解释器慢”。该系列工作指出：真正杀死性能的是 **SQL 引擎与过程式运行时之间数十万次的上下文切换（Context Switching）**。如果能把带有循环和跳转的 PL/SQL 完全编译成标准 SQL，就能零切换地在向量化引擎内部跑完。

### 核心技术：CPS / Trampoline 到 `WITH RECURSIVE` 的系统性映射
1. **基本块分解（Basic Block Decomposition）**：将过程代码切分为带标签 $L_0, L_1, \dots, L_k$ 的基本块（与 SpinDuck 现有的扁平化 `IrProg` + `PC` 跳转天然一致！）。
2. **状态元组化**：将当前标签 `label` 与所有局部变量 `(v1, v2, ..., vn)` 打包为递归表的一行记录。
3. **Trampoline 风格递归 CTE**：
   ```sql
   WITH RECURSIVE run(label, v1, v2, ...) AS (
     -- 初始基本块 L0
     SELECT 0 AS label, init_v1, init_v2, ...
     UNION ALL
     -- 根据上一轮 label 分派下一基本块（把 WHILE / IF / GOTO 变成 CASE 分支）
     SELECT
       CASE r.label
         WHEN 1 THEN CASE WHEN cond THEN 2 ELSE 3 END
         WHEN 2 THEN 1
         ...
       END AS label,
       next_v1, next_v2, ...
     FROM run r
     WHERE r.label <> -1  -- 终止标签
   )
   SELECT * FROM run WHERE label = -1;
   ```
4. **跨行向量化并行**：当外层需要对 10 万个输入参数同时调用同一过程时，只需在递归种子表中放入 10 万行（带 `row_id`），DuckDB 即可利用**同一条 `WITH RECURSIVE` 计划向量化推进 10 万个状态机**！

### 传统 SQL:1999 `WITH RECURSIVE` 的致命瓶颈
尽管思想极其优雅，但标准 SQL 的 `UNION ALL` / `UNION` 语义要求**把所有历史迭代产生的中间行全部保留并不断追加到工作表（Working Table）中**：
- 如果过程循环执行 500 轮，中间表就会膨胀 500 倍；
- 对于状态不断被原地覆盖的迭代（如余额更新、图最短路径、滚动试算），内存和 Hash 去重开销不可接受。

---

## 3. A Fix for the Fixation on Fixpoints & DuckDB `USING KEY`（CIDR 2023 / SIGMOD 2025）

- **论文 1**：*A Fix for the Fixation on Fixpoints*（**CIDR 2023**），Denis Hirn, Torsten Grust
- **论文 2**：*How DuckDB is USING KEY to Unlock Recursive Query Performance*（**SIGMOD 2025**），Björn Bamberg, Denis Hirn, Torsten Grust
- **工程落地**：已正式合入 **DuckDB v1.3.0+**（SpinDuck 当前依赖的 **DuckDB 1.5.5** 已原生支持！）。

### 核心突破：`WITH RECURSIVE ... USING KEY (...)`
1. **打破单调不动点束缚**：不再把递归中间结果看作只增不减的集合，而是将其视为一个**以指定列为主键的状态字典（Keyed State Table）**。
2. **物理实现机制**：DuckDB 在物理执行层使用**聚合哈希表（Aggregate Hash Table）**维护工作状态。每次递归迭代产生新行时，根据 `KEY` 直接**原地覆写/更新（In-place Overwrite）**对应键的旧状态，完全不保存无用的历史迭代快照！
3. **语义表现力**：
   ```sql
   WITH RECURSIVE proc_state USING KEY (batch_row_id) AS (
     SELECT batch_row_id, 0 AS step, init_val AS acc FROM input_rows
     UNION
     SELECT s.batch_row_id, s.step + 1, updated_acc
     FROM proc_state s
     WHERE s.step < max_steps
   )
   SELECT batch_row_id, acc FROM proc_state;
   ```
   - 通过 `recurring` 表别名或按 `batch_row_id` 键控，空间复杂度从 $O(N \times T)$ 骤降至 **$O(N)$**；
   - 彻底打通了 **“基于 `IrProg` 扁平化 PC 状态机 → 编译为 DuckDB `USING KEY` 递归 CTE”** 的工程通道。

---

## 4. DuckPL：DuckDB 原生过程解释与架构对照

- **项目**：**DuckPL**（Denis Hirn, Rusty Conover 等，面向 DuckDB 的 PL/SQL 兼容原生扩展）
- **核心设计**：当过程包含必须按序执行的复杂副作用（DML、异常捕捉、动态控制流）而无法纯 SQL 化时，采取“**嵌入式栈式流解释器 + 极简语法无关 IR + 表达式缓存**”，直接在进程内贴近 DuckDB 内存执行，避免每步触发完整 SQL 解析。

### 对 SpinDuck 的双重启示
1. **确认 SpinDuck 当前底层路线的合理性**：SpinDuck 的 `IrProg` + `MemoizingExecutor` + `LookupExecutor` 与 DuckPL 处于同一演进象限，说明 Rust IR 解释器本身（2.7%）设计紧凑高效，**P0/P1 不应推倒重写解释器**。
2. **解锁 P2“游标集合化下推”的精确适用分工**：
   - **静态无副作用/纯试算循环**：优先编译为 DuckDB 原生 `INSERT ... SELECT ... JOIN` 或 **`WITH RECURSIVE ... USING KEY`**，彻底绕开 Rust 逐行循环。
   - **含 `EXCEPTION` 跳过与多表写副作用的复杂过程**：保留 Rust 轻量解释骨架，改走 **路线三（Batching）**，用批处理算子把逐行 I/O 压缩为列式批量通道。
