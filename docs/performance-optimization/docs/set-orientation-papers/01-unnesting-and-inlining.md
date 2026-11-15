# 路线一：子查询去相关与过程代数内联（Unnesting & Inlining）

本专题梳理“将命令式过程逻辑、标量 UDF 与关联查询转化为纯关系代数，从而让优化器以集合算子执行”的四项关键研究，重点分析其适用边界与 **PRISM** 对传统全量内联的修正。

---

## 1. Unnesting Arbitrary Queries（BTW 2015）

- **作者**：Thomas Neumann, Alfons Kemper（慕尼黑工业大学 TUM）
- **核心地位**：HyPer、Umbra、Materialize 与 **DuckDB** 查询优化器的关联子查询去相关（Decorrelation）理论底座。

### 核心机制
1. **问题根源**：无论源自手写 SQL 还是 UDF 内联展开，关联子查询与 `LATERAL` 连接本质上都等价于一个 **Dependent Join（依赖连接，$\bowtie_{\text{dep}}$）**——左表每吐出一行，右子树就得带入外层属性重新求值一次（$O(N \times M)$ 复杂度）。
2. **通用下推算法**：不同于早期数据库仅对简单 `IN` / `EXISTS` 编写零碎启发式规则，Neumann & Kemper 提出了一套**完备、通用的等价重写代数规则**：
   - 将 $\bowtie_{\text{dep}}$ 作为一等逻辑算子留在计划树中；
   - 按结构归纳规则逐步将 $\bowtie_{\text{dep}}$ 向下穿过 `Selection`、`Projection`、`Aggregation`、`Union`、`Limit` 等算子；
   - 收集右子树实际引用的左表属性集合，将其去重后作为公共输入（Magic Set / Domain 抽取）与右表做一次集合运算，最终把 $\bowtie_{\text{dep}}$ 消解为标准的 **Hash Join / Left Join**。

### 对 SpinDuck 的意义
- DuckDB 1.5.5 已完整继承这套去相关引擎。这意味着 SpinDuck 在做 P2 集合化改写时，**只要能把过程内的循环查表表达成 `LATERAL` 或关联子查询，DuckDB 就能自动将其向量化并转为 Hash Join**，无需 SpinDuck 自己在 Rust 里手写 Join 物理算子。

---

## 2. Froid / BlackMagic：命令式 UDF 自动代数化内联（PVLDB 2018）

- **论文**：*Optimization of Imperative Programs in a Relational Database*（`arXiv:1712.00498`） / *BlackMagic: Automatic Inlining of Scalar UDFs into SQL Queries with Froid*
- **作者**：Karthik Ramachandra, Kwanghyun Park, K. V. Emani, Alan Halverson 等（Microsoft Research）
- **工程落地**：SQL Server 2019 起作为 **Scalar UDF Inlining** 正式上线。

### 核心转换规则
Froid 将命令式标量 UDF 视为由若干基本块（Basic Blocks）组成的控制流图，并按语句类型映射为关系代数表达式：

| 命令式语法结构 | 关系代数等价形式 | 说明 |
| :--- | :--- | :--- |
| 变量声明与赋值 `DECLARE / SET @v = expr` | `SELECT expr AS v` 包装为衍生表 + **`CROSS APPLY`** | 将状态突变转化为数据流中新增的一列 |
| 顺序语句序列 `S1; S2; ...` | 嵌套 **`APPLY` 链** | 前一步产生的绑定变量作为后一步的关联列向下传递 |
| 条件分支 `IF cond THEN S1 ELSE S2` | **`CASE WHEN cond THEN ... ELSE ... END`** 或带互斥谓词的 `UNION ALL` | 把控制流分支转化为数据流谓词选择 |
| `SELECT @v = col FROM T WHERE ...` | 标量子查询 + **`OUTER APPLY`** | 由优化器统一参与谓词下推与 Join 消除 |
| `RETURN expr` | 顶层输出投影 | 将最终关系表达式内联回调用方 `SELECT` / `WHERE` |

### 关键局限性（实证踩坑点）
Froid 在 SQL Server 生产环境落地后暴露出非常明确的边界：
1. **不支持复杂循环与副作用**：遇到 `WHILE` 循环带复杂状态更新、`TRY...CATCH` 异常块、动态 SQL、非确定性函数（如 `RAND()`）或表变量写操作时，必须静默回退到 RBAR 逐行执行。
2. **多层嵌套与长过程引发“计划爆炸”**：把上百行多分支过程完全展开为巨型 `APPLY + CASE` 表达式树后，逻辑计划节点数量指数级增长，极易触发优化器超时并生成劣质物理计划。
3. **破坏存储引擎优化**：内联后深层嵌套的 `CASE` / `OUTER APPLY` 常导致分区裁剪（Data Skipping）和谓词下推失效。

### 对 SpinDuck 的直接启发
- SpinDuck 目前仅支持“单 `RETURN` 无局部变量纯 `IN` 函数进 DuckDB `MACRO`”，过于保守；**P0 阶段可借鉴 Froid 思想**：
  - 将无真实表依赖的 `SELECT CASE ... INTO ... FROM sys_dummy` 直接在编译期折叠为 `IrStmt::Assign` 与条件表达式；
  - 将仅包含顺序局部变量赋值与简单 `IF-ELSE` 的短函数统一合并为一棵表达式树注册为 DuckDB `MACRO`。
- 同时吸取 Froid 的教训：**绝不将含游标循环、异常块（`EXCEPTION WHEN OTHERS`）和大量 DML 的过程强行内联为巨型 `APPLY` 树**。

---

## 3. PRISM：先轮廓提取（Outlining），再选择性内联（PVLDB 2024）

- **论文**：*The Key to Effective UDF Optimization: Before Inlining, First Perform Outlining*（PVLDB Vol. 18, No. 1, 2024）
- **作者**：Samuel Arch, Yuchen Liu, Todd C. Mowry, Jignesh M. Patel, Andrew Pavlo（CMU）
- **核心发现**：传统内联默认“内联得越多越好”，PRISM 用实证推翻了这一假设。

### 核心方法
1. **基于 SSA 区域 IR（Region-based SSA IR）的依赖剖析**：分析 UDF 内部哪些语句真正参与了**表扫描、过滤条件或 Join 键生成**，哪些语句仅仅是纯计算、格式转换或分支胶水代码。
2. **先 Outlining（轮廓提取 / 黑盒化）**：
   - 故意将**对查询优化器无益甚至有害**的片段（如复杂字符串/日期运算、冗长 `IF-ELSE` 计算、不可下推分支）保留或打包成极简原生算子，**对其向优化器“隐藏”内部复杂性**，防止查询图膨胀。
3. **后 Selective Inlining（选择性内联）**：
   - 仅将包含 SQL 表查询、可触发谓词下推、分区裁剪或 Join 消除的关键路径内联进调用查询，使优化器能够轻松完成去相关（Unnesting）。

### 实测效果（SQL-ProcBench）
- 相比传统全量 UDF 内联：
  - 在 **DuckDB** 上平均提速 **1.29×**（避免 DuckDB 优化器在膨胀计划树上退化）；
  - 在 **SQL Server** 上平均提速 **298.73×**（彻底消除大量全量内联失败或超时导致的计划劣化）。

### 对 SpinDuck 的直接启发
PRISM 完美印证并升级了 SpinDuck 设计文档中的分派策略：
- **哪些该留在 Rust 侧（Outlining）**：`to_char` / `to_date` / `substr` / `nvl` / `intervaltonum` 等高频标量函数与过程级 `TrapFrame` 异常控制，留在 Rust 侧配合 `SpinValue::Date` 本地直算，**根本不需要进 DuckDB**。
- **哪些该下沉 DuckDB（Inlining）**：含过滤/关联语义的维表查找与批量集合写入。

---

## 4. QBS：基于程序综合的集合化推导（PLDI 2013）

- **论文**：*Optimizing Database-Backed Applications with Query Synthesis*（PLDI 2013）
- **作者**：Alvin Cheung, Armando Solar-Lezama, Samuel Madden（MIT）

### 核心机制
针对应用/ORM 代码中普遍存在的“打开查询游标 → 循环内条件过滤/关联/聚合 → 写入结果”模式：
1. **静态分析循环边界**：从源代码中抽取出循环不变量（Loop Invariant）和前后置条件（Postcondition）。
2. **语法制导综合（Syntax-Guided Synthesis）**：在受限的关系代数（Selection / Projection / Join / Aggregation / Top-K）语法空间中自动搜索等价 SQL 表达式，并用 SMT 求解器验证语义等价性。
3. **整段替换**：用单条集合 SQL 替换整个命令式循环。

### 对 SpinDuck 的意义
虽然通用的 SMT 综合器在工程上维护成本过高，但 **QBS 的“循环不变量提取 + 模式验证”思路可直接简化为 SpinDuck 的规则化集合改写器**：在 IR 层对高频出现的 `FOR rec IN (SELECT ...) LOOP ... 维表点查 ... INSERT INTO ... END LOOP;` 骨架做模式识别与安全验证，自动改写为 `INSERT INTO ... SELECT ... JOIN ...`。
