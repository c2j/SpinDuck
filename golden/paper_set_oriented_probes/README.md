# 论文探针包（paper_set_oriented_probes）

把「游标循环 → 集合化」这个方向的**论文案例**和**开源基准例程**翻译成 GaussDB PL/SQL，
逐个跑 SpinDuck 的解释执行（Off）与集合化（batch-atomic / strict），记录判定、
「Off ↔ 集合化」是否逐值相等、Off 与参照 SQL 是否一致，用来定位可转换形状的边界。
逐探针的差距结论见 [REPORT.md](REPORT.md)；面向决策的汇总版（含逐篇论文的可行性与价值评估）见
`docs/performance-optimization/docs/spinduck-set-oriented-gap-report.md`。

> **这不是 GaussDB 黄金用例。** `golden/case_*` 的语义承诺是「与 GaussDB 基线逐行一致」；
> 本包没有 GaussDB 参照侧。所以目录名不带 `case_` 前缀，也不在
> `spinduck/tests/cases_native.rs`、`golden_gauss.rs` 的显式清单里——CI 不会自动执行本包。

## 两个工程

| 工程 | 位置 | 数据 | 承载的案例 |
|---|---|---|---|
| `main` | 本目录 | 手工种子：TPC-H / TPC-DS 最小列子集 + 几张应用表 | 定标阶梯；Froid 图 1；Aggify；CIDR 2024 / PRISM 的 `getManufact`；Hirn & Grust 的 `walk()`；DBridge、QBS、Sloth 的模式；SQL-ProcBench 逐字例程；引擎侧探针 |
| `tpch` | `tpch/` | SQL 生成的 TPC-H 形状数据（8000 张订单、24000 行明细，确定性） | Hirn & Grust 案例集里基于 TPC-H 的函数；Froid §11.1 的 TPC-H 谓词 UDF；PRISM 的谓词提升场景 |

两个工程的表名有重叠（都有 `orders`、`customer`），所以各自是独立的临时库，不共用。

## 来源与忠实度

每个探针在 `probes.tsv` 的 `paper` 列和源文件头都写明出处。四种忠实度，不要混读：

| 标注 | 含义 | 涉及探针 |
|---|---|---|
| **逐字** | 论文或基准的代码一字未改（只做 CRLF→LF、剥离文件尾部的示例查询） | Froid 图 1 与 §11.1（`f11_froid_tpch.sql`、`f11_froid_tpch_dates.sql`）；Hirn & Grust 案例集 `markov-pl.sql`（`e1_hirn_grust_walk.sql` 的函数体）；SQL-ProcBench 4 个文件（`procbench/`）；仓库测试夹具 `caller_dummy`（T2.f） |
| **仅改 X** | 为绕开一个**已定位的引擎缺口**做的最小改动，文件头写明改了什么、为什么 | `walk()` 的随机数一步（引擎算不对中缀 MOD 与除法）；`goto_margin_r`（DATE 变量绑定）；`prc_goto_late_b`（表别名与记录名撞名）；三个日期谓词的 `_r` 版（PL 表达式里的 CAST、`DUAL`）；C1 的被调函数（参数名遮蔽表名）；D1'（列别名被变量替换）；D2'（游标源由表值函数改为表） |
| **按论文改写** | 原文是 PL/pgSQL、T-SQL 或应用侧 Java；按同一语义改写成 GaussDB PL/SQL，改动逐条记在文件头 | GOTOs 案例集的 6 个函数；Aggify 图 1、图 2；DBridge、QBS、Sloth 的模式 |
| **自造** | 论文只给方法或纯定标用 | A3–A6、全部 P*/T* 定标探针、X* 引擎侧探针 |

上游出处：

- **Hirn & Grust 案例集**（SIGMOD 2021 论文的配套仓库）：
  <https://github.com/One-WITH-RECURSIVE-is-Worth-Many-GOTOs/Code>，master，2026-11-28 下载。
  18 个真实世界 PL/SQL 函数，其中 8 个基于 TPC-H；另 10 个依赖 PostgreSQL 的数组、复合类型、
  几何类型或 jsonb（光线追踪、Barnes-Hut、电子表格求值等），无法改写成 GaussDB PL/SQL，未收录。
- **SQL-ProcBench**（Gupta & Ramachandra，PVLDB 14(8) 2021 的配套基准）：
  <https://github.com/microsoft/SQL-ProcBench>，`src/PLSQL`，main 分支，2026-11-27 首次下载（压缩包 179,133 字节；11-28 重新下载后大小相同），MIT 许可。
  文件头记有上游文件 sha256 的前 16 位；剥离下来的示例查询在 `cases/procbench_examples/`。
- **仓库夹具**：`spinduck/tests/set_oriented_dummy.rs` 的 `fn caller()` 与 `BASE_DDL`，逐字抽取。

## 目录

```
paper_set_oriented_probes/
├── README.md / REPORT.md
├── probes.tsv                 # main 工程的探针清单（10 列，见下）
├── run_probes.sh              # 运行器：两个工程各走一遍 compile --dir → deploy → 数据 → 逐探针比对
├── spinduck.toml              # §12.1 项目声明；入口不写 [[entry]]（独立例程用 --entry 裸名）
├── codeweb.toml               # compile --analyze 取图用
├── sql/ddl/tables.sql         # 最小列子集表 + 结果表
├── sql/ddl/set_oriented_dummy_fixture.sql   # T2.f 的夹具表（逐字）
├── sql/routines/*.sql         # 论文案例（A/B/C/E）、定标阶梯（p0_/t0_）、论文模式（d09_/q10_/s11_）、引擎侧探针（a0_/x2_/x4）
├── sql/routines/procbench/    # SQL-ProcBench 逐字拷贝 + 三个「仅改一处」的版本
├── cases/seed.sql             # main 的种子（全部确定性）
├── cases/oracle.sql           # main 的参照表
├── cases/procbench_examples/  # 从逐字文件剥离出来的上游示例查询（备查）
├── tpch/                      # tpch 工程：probes.tsv、spinduck.toml、sql/、cases/gen.sql、cases/oracle.sql
└── reports/                   # run_probes.sh 的产物（见下）
```

## 怎么跑

```bash
# 先构建 CLI（本包不替你构建）
(cd ../../spinduck && cargo build --features cli)
bash run_probes.sh                                   # 约 3 分钟
SPINDUCK_BIN=/path/to/spinduck bash run_probes.sh    # 指定二进制
SQLPROCBENCH_SRC=/path/to/SQL-ProcBench/src/PLSQL bash run_probes.sh   # 追加：基准整体解析体检
KEEP_WORK=1 bash run_probes.sh                       # 保留临时工程（含 duckdb）
```

不需要容器，不访问网络。运行态（`.spinduck/`、`.codeweb/`、duckdb 文件）只落在临时目录。

## 产物与语义合同

| 产物 | 入库 | 内容 |
|---|---|---|
| `reports/matrix.tsv` | 是 | 差距矩阵，两个工程的全部探针 |
| `reports/summary.txt` | 是 | 汇总数字。报告里的数字以它为准 |
| `reports/admit_<工程>_<模式>.txt` | 是 | `compile --admit=report` 的判定行（编译期，不执行） |
| `reports/compile_<工程>.log`、`compile_fail.txt`、`run_env.txt` | 是 | 项目态编译体检原始日志、失败归集、被测二进制与 HEAD |
| `reports/duckdb_mechanisms.txt` | 是 | 在项目钉住的内核上实测报告里引用的 DuckDB 写法（TRY、窗口函数、USING KEY） |
| `reports/sqlprocbench_compile.txt` | 是（设了 `SQLPROCBENCH_SRC` 才生成） | 基准 PL/SQL 语料的解析层体检 |
| `reports/evidence/<工程>/<id>.txt` | 是 | 合并明细：集合化成功、Off 出错、与参照不一致、与期望不符、query 通道的探针 |
| `reports/work/<工程>/<id>.txt` | 否（`.gitignore`） | 全部探针的合并明细，每次运行重建 |

矩阵的列：

| 列 | 含义 |
|---|---|
| `compile` | 项目态编译体检结论（**文件级**：FAIL 行只带文件名，同文件里没出问题的探针也显示同一个错误码） |
| `via` | `plan` = `call --plan`；`file` = 计划不可用时回退 `call --file`；`file(ordered)` = 强制 `--file` 且被调方文件在前（对照 X1.plan）；`query` = `spinduck query` 直接跑 SQL |
| `off` | 解释执行的退出码 |
| `ba` / `st` | batch-atomic / strict 的集合化判定 |
| `ba_eq` / `st_eq` | 集合化结果与 Off 是否逐值相等（用引擎的 `compare` 归一后判值）。`EXIT(1vs1)` = Off 自己就报错、两种模式退出码一致 |
| `off_vs_oracle` | **Off 的结果对不对**：与参照 SQL 在同一个 DuckDB 上的结果比对。参照 SQL 分「论文给的」与「本包手写的」，`cases/oracle.sql` 里有标注 |
| `off_vs_expect` | 与清单里手写的期望值比对（用于数值语义这类没法用 SQL 参照的探针） |

读矩阵时的四条口径：

1. **`EQ` 不等于正确。** 它只说明集合化与解释器一致；解释器本身对不对看 `off_vs_oracle` 与 `off_vs_expect`。
2. **判定取首个失败守卫的拒因。** `Skipped/UnsupportedControlFlow` 是第一道门的兜底项，
   后面还有没有别的障碍要靠定标阶梯（T 系列）逐个特性去试。
3. **Off 报错的探针，其集合化判定没有意义**（区域还没执行到就已经失败）。
4. **`expect` 里的期望值是按 Oracle NUMBER 语义手写的**，不是从 GaussDB 实测得来的。

## 清单格式（probes.tsv，制表符分隔，10 列）

`id`、`kind`（call / call-ordered / sql）、`routine`、`args`（空格分隔）、`reset`（每轮调用前执行，多条用 ` ;; ` 分隔）、
`dump`（调用后导出结果表，多条用 ` ;; ` 分隔）、`paper`、`shape`、`oracle`（参照查询，只比对第一条 dump）、`expect`（函数 RETURN 的期望值）。

## 维护

- 新增探针：写例程（文件头注明出处与忠实度）→ 必要时补 DDL/种子 → 在 `probes.tsv` 加一行。
  数据必须是确定性的，否则 Off 与集合化两轮没法比。
- 预期编译不过的例程单独放一个文件（如 `x4b_mod_literal.sql`、`f11_froid_tpch_dates.sql`），
  否则会连累同文件里其它例程的注册与体检。
- 例程自己负责清理结果表的，`reset` 留空；否则把清理语句写进 `reset`。
- 本包只读仓库资产，不改 `spinduck/src`、不改 `golden/case_*`。
