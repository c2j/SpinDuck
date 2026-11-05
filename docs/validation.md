# SpinDuck 设计验证报告

> 本文件记录**实证过程与发现**，属案例证据，不属于设计本体。设计见
> [architecture.md](architecture.md)；两者单向引用：设计只引用本文件作为规则
> 来源与证据，本文件不承载设计决策的正文。
>
> 目标语料：`lib/fastaas/exam/清算拆分优化考题/基线代码`（136 文件 / ~16k 行）。
> 验证资产：`scripts/sim_prc_trd_hz_byfund.py`、`scripts/sim_prc_trd_deal_purchase.py`、
> 规则种子数据 `config/rewrite-rules.toml`。
>
> **内核版本口径**：目标内核为 **DuckDB 1.5.5**（设计 §5 / M1 锚定；版本号写法为 1.5.5，此前误写 1.5.5.0 导致 pip 解析失败）。本文全部
> 探针与模拟在 1.4.4 上执行——规则中的具体行为（nvl 缺失、UPDATE SET 限定列、
> UDF 多线程回调等）已于 2026-11-04 按 V6 清单在 1.5.5 上完成复测（见 V6 结果表）。

## V1. 语料级实证（前端）

- 构成：85 表、9 视图、2 序列、24 函数、6 独立过程、6 PACKAGE（~7.5k 行包体）
- `ogsql validate -d <corpus> --stats` 实测：**148/148 语句 VALID**，唯一 warning
  为 8 处 Oracle `(+)` 外连接标记（ogsql AST 已保留，可机械改写）→ 前端零缺口
- PL 特性面实测（grep 统计）：%TYPE(25 文件)、EXCEPTION/WHEN OTHERS(29)、
  GOTO(6)、过程体内 COMMIT(7)、显式游标+%ROWTYPE(8)、FOR rec IN(2)、
  NEXTVAL(3)、sys_dummy(5)、rownum(42)、to_char(12)、nvl(21)、decode(11)；
  FORALL/BULK COLLECT/DBMS_*/AUTONOMOUS/PIPE ROW/dbms_sql/MERGE/PRAGMA **均未使用**
- DDL 已为 PG 风格类型；视图含 `/*FORCE*/` hint；序列 MAXVALUE 超 BIGINT

## V2. AST 级实证（两个被验过程）

### prc_trd_hz_byfund（ogsql parse -j，945KB JSON）

- 26 变量（全 %TYPE 惰性解析）+ 3 游标（全带 `parsed_query`）+ 1 REF CURSOR 类型
- 语句树：Assignment/Open/Loop/Close/Block(含 exception_block)/SqlStatement/Commit/If；
  `Exit` 携带 `CursorAttribute.NotFound`；`Open.kind` 区分 `Simple` 与
  `ForQuery{query:"v_sql_txt"}` —— 与设计 §7 游标设计完全对应
- 实测发现**命名异常** `no_data_found` / `too_many_rows`（此前设计仅有 OTHERS）

### prc_trd_deal_purchase

- 函数调用全部位于 **PL 上下文**（赋值右值/条件），与 #1（SQL 上下文）互补
- 日期运算 `to_char(to_date(d,'YYYYMMDD') + n, 'YYYYMMDD')`、
  `||` 拼接 DECIMAL 列（`cjbh`）、`lpad(数值列)` 等隐式转换高频出现

## V3. 模拟验证（真实 DuckDB 1.4.4 端到端）

### 验证 #1：prc_trd_hz_byfund（scripts/sim_prc_trd_hz_byfund.py）

延迟事务 DELETE ✓ · UPDATE SET 内 UDF 桥接 ✓ · 45 列大游标（快照 schema）✓ ·
SELECT INTO 命名异常（文案与 GaussDB 原文一致）✓ · INSERT 内嵌 UDF ✓ ·
REF CURSOR 动态 OPEN FOR（运行期 ogsql 解析 VALID + 执行）✓

### 验证 #2：prc_trd_deal_purchase（scripts/sim_prc_trd_deal_purchase.py）

宏注册 + 日期运算改写（20240105+7→20240112）✓ · 游标 WHERE 过滤（候选 3 行）✓ ·
OTHERS + inst_log + GOTO continue（跳过 2 行）✓ · 解释器直调过程级函数（嵌套）✓ ·
rownum→LIMIT ✓ · INSERT 落表（accrual_daily=14.290）✓

## V4. 实证修订记录（R1–R15 → 已回流至规则数据或设计）

以下发现的**机制性结论**已并入设计文档；**具体规则值**已落 `config/rewrite-rules.toml`。
此处保留发现-证据对照，供规则变更时追溯。

| # | 发现（证据） | 去向 |
|---|---|---|
| R1 | DDL DEFAULT 带 `::character varying` 建表失败 | rules `[ddl] strip_casts` |
| R2 | DuckDB 1.4 无 `to_number` | rules `[functions] TO_NUMBER` |
| R3 | DuckDB 1.4 无 `nvl`（原设计误判） | rules `[functions] NVL`；确立"规则以实测为准"原则 |
| R4 | 不支持 `UPDATE .. SET t.col`，支持 `UPDATE tbl AS t SET col` | rules `[patterns]` |
| R5 | 含 UDF 查询多线程回调不安全（`SET threads=1` 后正常；Rust UDF 需复测） | 设计 §8.2 约束③ |
| R6 | 游标结果列全为别名（`pay_type→fffs`），可不含源表列 | 设计 §7.3 决策 2 |
| R7 | 命名异常 no_data_found/too_many_rows 实际使用 | 设计 §6 异常语义 |
| R8 | 记录字段赋值目标是 `ColumnRef`，与列引用同形 | 设计 §3.4 AssignTarget 消歧 |
| R9 | 动态 SQL 必须复用同一改写器（nvl 修正同样命中） | 设计 §7.1 / §4 |
| R10 | sys_dummy 纯 PL 表达式查询折叠有价值 | 设计 §7.4 |
| R11 | Oracle 日期运算高频（to_char 12 文件） | rules `[dates]` |
| R12 | `\|\|` 拼接 DECIMAL 列、lpad 数值列 | rules `[implicit]` |
| R13 | 分流需"调用上下文"维度（PL/SQL） | 设计 §8 分派矩阵 |
| R14 | 表达式级函数宏路径正式化 | 设计 §8 / rules 注释 |
| R15 | rownum→LIMIT 须保留子查询 ORDER BY | rules `[patterns]` |

## V5. 覆盖面与遗留

已覆盖：静态游标、REF CURSOR 动态 OPEN、%ROWTYPE 读写、GOTO-as-continue、
命名异常、SELECT INTO 0/2 行探测、sys_dummy 折叠、UDF 桥接、UDF 线程安全、
宏路径、解释器直调、嵌套函数、日期运算、rownum、隐式转换、存根过程调用、延迟事务。

M10 终验前需补：PACKAGE 级变量状态、FORALL、滚动游标、REF CURSOR 作 OUT 参数、
Rust UDF 线程安全复测。

模拟器定位：Rust 实现落地后，以同口径（同 schema/数据/期望输出）改造为
M10 黄金比对的对照基线；目标语料集与样例数据通过项目配置声明，不入引擎代码。

## V6. 1.5.5 复测清单与结果（M3.5 门禁基线）

**复测完成（2026-11-04，DuckDB 1.5.5 / Python 3.14）。** 目标版本号写法为
`1.5.5`（此前误写 1.5.5.0 导致 pip 解析失败）。结果与规则联动：

| 项 | 1.4.4 结论 | 1.5.5 复测结果 | 规则动作 |
|---|---|---|---|
| `nvl` | 缺失（R3） | **仍缺失** | coalesce 映射保持 verified |
| `to_number` | 缺失（R2） | **仍缺失** | CAST 映射保持 verified |
| `decode` | 未单独探针 | **缺失** | DECODE→CASE 映射 verified |
| `to_char`（Oracle 格式串） | 未单独探针 | **缺失**（含格式串） | strftime 映射 verified |
| `UPDATE .. SET t.col` | Parser Error（R4） | **仍不支持** | 剥离限定前缀规则保持 |
| `strptime/strftime + INTERVAL` | 正常（R11） | **正常** | 日期宏路径保持 |
| TRUNCATE | 缺失 → DELETE 改写 | **原生支持** | **删除该改写规则**（规则数据化收益实证） |
| DEFAULT `'0'::character varying` | 建表失败（R1） | **原生可解析** | **strip_casts 清空**（规则数据化收益实证） |
| `CREATE SEQUENCE` MAXVALUE 超 i64 | 未探针 | **Parser Error** | 截断规则 verified |
| `'x'\|\|DECIMAL` 拼接 | 模拟中手动 str()（R12） | **原生可用** | concat 规则关闭；仅 lpad(数值) 仍需 CAST |
| 含 UDF 查询多线程回调（SELECT 列表） | 不安全（R5） | **threads=4/8 通过** | 收窄：见下 |
| 含 UDF 查询多线程回调（WHERE 关联子查询） | 不安全（R5） | **threads=4/8/16 仍失败** | 约束保留但按位置收窄 |
| 两个模拟器全链路 | 全部 ✓ | **1.5.5 全部 ✓**（typing→sqltypes API 兼容层） | 模拟器成为 1.5.5 回归资产 |
| 命名参数 `$name` | 未探针（设计原用 `?`） | **原生支持**：按名绑定、同名多次引用消重、`$v_` 前缀与列名共存 | 设计修订：变量替换采用命名占位符 `$v_<var>`（见设计 §3.4） |

**R5 修订（UDF 线程安全，按执行位置收窄）**：1.5.5 下 SELECT 列表中的 UDF 在
多线程下安全；**WHERE 关联子查询位置的 UDF 回调仍会在并发下失败**（复现 1.4.4
症状）。设计 §8.2 约束③由"含 UDF 的语句一律单线程"收窄为："UDF 出现在过滤/
子查询位置的语句强制 threads=1；SELECT 列表位置放开多线程"。Rust UDF 落地后
仍需按 M6.5 复测（Python 回调结论不外推）。

## V7. GaussDB 物化/回写实证（M4.5 门禁）

**环境**：本机容器 `opengauss/opengauss:5.0.0`，映射 `15432→5432`。openGauss 默认
sha256 口令，纯 Rust `postgres` crate 不支持；容器侧改为
`password_encryption_type=0` + `pg_hba.conf` 追加 `host all all 0.0.0.0/0 md5`
（直接改 `/var/lib/opengauss/data/*.conf` 后 `docker restart`），并以
`ALTER USER gaussdb IDENTIFIED BY '…' REPLACE '…'` 重存口令后 md5 认证可用。
**结论：通道选择 PG 线协议成立；生产对接需在 GaussDB 侧确认 md5 或改用 TLS +
SCRAM 实现（v1 不支持 sha256，属已知限制）。**

E2E（`cargo test --features gauss gauss_e2e`，`tests/gauss_e2e.rs`）两个用例：

1. `gauss_e2e_materialize_process_sync_back`：GaussDB `sd_dat_in(a,n)` 两行
   `('x',2),('y',3)` → `materialize` 全量入 DuckDB（2 行，文本承载）→ 过程
   `prc_agg` 执行 `INSERT INTO sd_dat_out SELECT 'sum', SUM(n)` → `sync_append`
   回写 → GaussDB 侧 `SELECT` 读回 `('sum','5')`。**物化→处理→回写→校验闭环成立。**
2. `gauss_e2e_materialize_with_where_clause`：`materialize(.., Some("bcrq = '20240105'"))`
   只落 1 行（`20240106` 被过滤），**按分区键增量物化成立**（§13.1 增量口径）。

**实证约束（回流设计 §13）**：

- 物化/回写均以**文本值**承载：数值/日期类型由 DuckDB 目标列类型完成转换；
  文本交接对 `NUMBER`→`DECIMAL` 精度无损（文本原样），但 `CLOB`/`CHAR` 语义
  损失仍按 §9 规则条目承担。
- 回写为 **append**（`INSERT`），不做 upsert/delete-then-insert；幂等由调用方
  （分区重建/`WHERE` 切片）保证，v1 不承诺事务跨端一致性。
- DuckDB 侧表需**预先建好**（`load` 通道），`materialize` 不推断 schema；列顺序
  按 `SELECT *` 列序对齐，两侧列名不一致时由调用方在 `load` 阶段对齐。
- 容器实测显示 openGauss `INT` 读回为文本 `"5"`（`simple_query` 文本协议），
  物化/回写不依赖二进制协议，故跨版本兼容性风险低。

## V8. Rust 侧 SQL 上下文函数调用（M6.5 门禁）

**结论：宏路径（§8.2 路径③）在 1.5.5 上成立且无需 UDF 重入。**

| 探针 | 结果 |
|---|---|
| `CREATE MACRO f(p) AS (p * 2)` 后 `SELECT f(21)` | 42（`INTEGER`），登记即生效，无需重连 |
| 宏在过程体 SQL 内可见（`INSERT INTO dat2 SELECT fnc_double(n) FROM dat`） | 42，游标/INSERT-SELECT 同一会话内可见 |
| 宏体经 `[functions]` 规则映射（`nvl` → `coalesce`） | `f(NULL)=0`、`f(7)=7` |
| 含局部变量/多语句的函数 | **不进入** `duckdb_functions()`；PL 上下文直调仍返回 2 |
| 过程（无返回） | 不进入内核函数命名空间 |
| 宏体类型 | 不 CAST 到声明返回类型：`INTEGER` 实参 → `INTEGER` 结果（避免 DECIMAL(38,6) 精度/语义损失） |

**未落地项与依据**：② 内联需编译期跨语句 SQL 重写（与动态 SQL 注入面同批，
M8）；① UDF 需同 Database 第二连接，`SqlExecutor` 接缝未暴露该能力，属接缝
签名变更（Ask first），故推迟。过程级函数在 SQL 上下文调用会得到
`KERNEL-0010`（Catalog Error），属明确失败。

## V9. 语料批量编译体检（M7 门禁）

工具：`spinduck compile <path>...`（`src/cli`，编译期不触碰内核 → 与 DuckDB 版本解耦）。
语料：`lib/fastaas/exam/清算拆分优化考题/基线代码`（清算拆分考题基线，136 个 SQL 文件）
与 `lib/fastaas/exam`（含历史收集，187 文件）。

| 语料 | 文件 | 通过 | 失败 | 注册例程 |
|---|---|---|---|---|
| 基线代码（首轮） | 136 | 121 | 15 | 28 |
| 基线代码（当前） | 136 | **136** | **0** | **54** |
| exam 全树（当前） | 187 | 183 | 4 | 72 |

首轮失败原因 → 修复动作（每条都带回归测试）：

| 失败码 | 数量 | 根因 | 动作 |
|---|---|---|---|
| ENG-0026 | 8 | PL 表达式缺 `Parenthesized`/`UnaryOp(-)`/`InList`/`Between` | 编译期脱糖为既有 IR（BETWEEN→`>= AND <=`，IN→等式析取，一元负→`0 - x`）；`tests/ir_expr_forms.rs` |
| ENG-0020 | 6 | ①空 `ELSE` 时 `if_next` 标签未 mark；②`<<label>>` 语句标签未 mark（GOTO 目标） | 编译期统一 mark；`should_compile_if_without_else_branch`、`should_resolve_goto_to_labeled_statement` |
| ENG-0056 | 2 | `FOR rec IN <query>` 未实现 | 脱糖为隐式游标（OPEN/FETCH/%NOTFOUND/CLOSE），内核探针 `should_run_for_loop_over_query_on_real_kernel` 验证 v_sum=6 |
| BIZ-0132 | 1 | 1 参 `TO_CHAR` 被规则误判为错误 | 1 参 → `CAST(x AS VARCHAR)`（默认格式差异登记为已知限制） |
| ENG-0022 | 1 | `EXECUTE IMMEDIATE '<静态文本>'` 未编译 | 有 `parsed_query` 时按静态 SQL 下沉；运行期拼接仍显式拒绝（ENG-0062，注入面 M8） |
| ENG-0022（新） | 1 | PL `CASE` 语句未实现 | 简单 CASE 求选择子一次后脱糖为 IF 链；搜索 CASE 直接取条件；`should_desugar_pl_case_statement` |

**顺带修出的语义缺陷（守护测试锁定）**：`BinOp::Ne` 对 `compare()` 结果二次取反，
导致 `<>`/`!=` 恒等于等值判断（`'07' <> '02'` 返回 false）。修复见
`src/runtime/frame.rs`，守护测试 `should_evaluate_ne_as_inequality_not_inverted`
与语料 IN/NOT IN 用例。

**剩余 4 例（显式拒绝，非静默）**：

| 文件 | 原因 | 归类 |
|---|---|---|
| `0907sql/…`、`all_sq/…PKG_IMPORT_EXCEL.sql`、`final2/…` | 集合元素赋值 `t_res(i) := v`（PL 集合类型，非 UTF-8 只是伴随特征） | 集合/数组类型 M9+ |
| `all_sq/…PKG_SPLIT_TRADE_STEP3_SH.sql` | 带参游标 `OPEN c(args)`（`PlOpenKind` 参数化游标） | 带参游标 M9+ |

**非 UTF-8 语料**：GBK 文件以有损 UTF-8 解码（SQL 关键字/标识符为 ASCII，仅注释
与中文字面量受影响），报告行标注 `(non-utf8)`。实测该 47 个文件解码后全部通过
编译，说明有损解码在编译期够用；若后续需要保真字符串字面量，再引入显式 GBK
解码（新增依赖需 Ask first）。
