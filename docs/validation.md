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

**未落地项与依据**（2025-11 首轮口径；② 一项已被 V15/V21.3 与 V23 推翻，见下）：
① UDF 需同 Database 第二连接，`SqlExecutor` 接缝未暴露该能力，属接缝签名变更
（Ask first），故至今仍推迟。本轮同时登记的"过程级函数在 SQL 上下文调用会得到
`KERNEL-0010`（Catalog Error）"**已不再是当前行为**——参数元组缓存化下沉（路径②）
随后落地，SQL 上下文可以调用已登记的过程级函数；`KERNEL-0010` 只覆盖"名字不在
例程目录且未宏化"的情形。复验证据与订正见 V23。

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

**剩余 4 例（显式拒绝，非静默）**（2025-11 首轮口径；2026-11-07 复跑为
**182/187、5 例**，新增归类见下方补记与 V23）：

| 文件 | 原因 | 归类 |
|---|---|---|
| `0907sql/…`、`all_sq/…PKG_IMPORT_EXCEL.sql`、`final2/…` | 集合变量**整体**赋值 `v_info_tab := split_string_into_table(…)`（PL 集合类型，非 UTF-8 只是伴随特征；探针里的元素赋值 `t(i) := v` 同落 `ENG-0022`，但不是语料命中形态） | 集合/数组类型 M9+ |
| `all_sq/…PKG_SPLIT_TRADE_STEP3_SH.sql` | 带参游标 `OPEN c(args)`（`PlOpenKind` 参数化游标） | 带参游标 M9+ |

**2026-11-07 复跑（`spinduck compile lib/fastaas/exam`，182/187、5 例）**：上表 4 例
原样复现（3× `ENG-0022` 分别落在 `0907sql/`、`all_sq/`、`final2/` 三份收集件，
1× `ENG-0048` 落在 `all_sq/`；基线树
`7_核心存储过程包/PKG_SPLIT_TRADE_STEP3_SH.sql` 里 `OPEN c_new_stock` 0 命中、收集件 3 命中，
基线版编译通过），另新增 1 例：

| 文件 | 报错 | 归类 |
|---|---|---|
| `all_sq/…/SPLIT_LOG_INFO.sql` | `ENG-0064` 声明了例程却未登记任何例程（解析器降级/静默丢弃守卫） | 非上述四类构造；守卫在 `src/engine.rs`（登记数为零即红），属显式失败 |

**非 UTF-8 语料**：GBK 文件以有损 UTF-8 解码（SQL 关键字/标识符为 ASCII，仅注释
与中文字面量受影响），报告行标注 `(non-utf8)`。实测该 47 个文件解码后全部通过
编译，说明有损解码在编译期够用；若后续需要保真字符串字面量，再引入显式 GBK
解码（新增依赖需 Ask first）。

## V10. 黄金对比门禁（M10）

**参照实现的选择。** 考题提供的 GaussDB 实例（`121.36.227.23:8000`，见考题
`文档/4-数据库与连接信息.md`）在本机不可达：TCP 连接被拒绝（`Connection
refused`），因此无法取得 GaussDB 基线。门禁改用**同内核血统的 openGauss 5.0.0**
（本机容器）作为参照实现，边界与差异在下方逐条登记。

**用例**：`golden/case_split_agg/`（表结构取自考题基线的最小可用子集：成交明细
`mid_yjqs_detail` + 指令表 `dat_inst_fund_cjqs`）。两侧表达同一逻辑但语法形态不同，
以结果表逐行比对为准：

| 侧 | 形态 |
|---|---|
| 参照（openGauss） | `FUNCTION` + `FOR r IN SELECT … LOOP` 隐式游标 + `$$ … $$ LANGUAGE plpgsql` |
| 被测（SpinDuck） | `PROCEDURE` + 显式 `CURSOR`/`OPEN`/`FETCH`/`%NOTFOUND`/`CLOSE` + `EXCEPTION OTHERS` |

**结果：两侧输出逐行一致**（`cjsl/cjje/fee` 全部相等），同时与提交入库的
`expected.json` 一致：

```
('20240105','F001','600000', 300.0000, 3250.00, 2.60)
('20240105','F002','000001',  50.0000, 1012.50, 0.81)
```

流程同时覆盖：openGauss 建表/灌数 → **物化进 DuckDB**（4 行）→ 注册 Oracle 形态
例程 → `CALL` → 回读结果表。即"物化 → 处理 → 对比"闭环在真实内核数据上成立。

**openGauss 5.0.0 参照侧语法实证（影响夹具写法，不影响引擎）**：

| 形态 | 结果 | 说明 |
|---|---|---|
| `CREATE FUNCTION f() … AS $$ … $$ LANGUAGE plpgsql` | 通过 | 参照侧主力形态 |
| `DO $$ … $$` | 通过 | 可用于一次性 PL 块 |
| `FOR r IN SELECT … LOOP` | 通过 | 隐式游标可用 |
| `CREATE PROCEDURE p() AS $$ … $$ LANGUAGE plpgsql` | **42601** `subprogram body is not ended correctly` | openGauss 的 `CREATE PROCEDURE` 不接受 PG 风格 `$$` 体 |
| `DECLARE c CURSOR FOR/IS SELECT …` | **42601** `syntax error at or near "SELECT"` | 函数内游标声明形式受限 |

结论：**差异只在参照侧的 DDL 语法层**；PL 求值语义（游标/循环/异常/参数）由被测侧
独立实现并与参照侧结果一致。

**本门禁抓出的被测侧缺陷**：

1. **小数字面量被降级为字符串**：AST `Literal::Float` 曾映射为 `SpinValue::Str`，
   使 `p_amt * 0.0008` 在 DuckDB 侧变成"数值 × 字符串"而报类型推断失败
   （`Binder Error: Could not choose a best candidate function`）。修复为十进制
   字面量（非标度值 + scale，指数形式显式降级为字符串）。守护测试
   `should_treat_decimal_literals_as_numbers_not_strings`。
2. **回写分页漏行**：`sync_append` 只取一批（1 万行）就结束，>1 万行时静默截断。
   改为循环取批直到耗尽（`SYNC_BATCH = 1000`）。

**已知限制**：`materialize` 逐行 INSERT（万级可行，百万级需批量通道，未做）；
参照侧 `expected.json` 依赖 openGauss 实时生成，若后续能访问 GaussDB，应重跑并
对比（替换参照实现不改变夹具与断言）。

## V11. 例程目录持久化（M7）

`spinduck_routines(name, kind, source, registered_at)` 随 `.duckdb` 文件持久化，
只存**可重放源 DDL**（设计理由见 architecture §3.6）。

| 探针 | 结果 |
|---|---|
| 文件库注册例程 → 关闭连接 → 新实例 `reload_catalog()` | 恢复 1 个例程，`CALL` 返回 OUT=15（`p_i*5`, 3→15） |
| 同一例程重复 `register_persistent` 两次 | 目录表仍 1 行（`ON CONFLICT DO UPDATE`） |
| 空库（无目录表）`reload_catalog()` | 返回 0，不报错 |
| 包体（PACKAGE BODY）多例程 | 共享同一源文本，恢复时按 `DISTINCT source` 去重 |

**未做的取舍**：不持久化 IR/AST 快照。理由——IR 含跳转地址与标签表，跨编译器版本
重放等价性不可证；源文本重编译才能保证"升级后行为 = 新版本定义的行为"，编译失败
也会显式暴露而不是运行期错乱。

## V12. 变异测试门禁（M10 节点）

**范围**（AGENTS §4.4）：`src/ir/mod.rs`、`src/sql/rewrite.rs`、`src/value.rs`
（注意：value 位于 `src/value.rs`，非设计文档早期写的 `runtime/value.rs`）。
工具 cargo-mutants 27.1.0，`--no-default-features`（不含 DuckDB：被测三模块的
行为不依赖内核；全量内核构建下每个变异体链接成本 ~90 s，292 个变异体 ≈ 7 h，
实测后改用轻量配置）。

| 轮次 | 配置 | 变异体 | caught | missed | unviable | timeout | score |
|---|---|---|---|---|---|---|---|
| 第 1 轮 | 全三模块（基线测试集） | 292 | 170 | 95 | 15 | 12 | 64.2% |
| 第 2 轮 | 补测试后（全三模块） | 301 | **207** | **66** | 15 | 13 | **75.8%** ✅ |

**第 1 → 2 轮新增的测试（全部先红后绿）**：

- `tests/compiler_contracts.rs`（15 例）：全算子映射、嵌套表达式变量收集、
  EXIT/CONTINUE 越界、深度护栏、循环标签、纯表达式体识别、记录字段消歧、
  `NULL;` ≠ `COMMIT;`、内建函数下沉（含规则改名与解析级变换）、FOR 循环逐行执行、
  一元正。
- `src/value.rs`：display 各变体（含负数十进制）、fits 全组合、非法数字字面量
  BIZ-0112、字符串→整数/布尔→VARCHAR coerce。
- `src/sql/rewrite.rs`：字符串内的函数样文本不变换、一语句多调用、字面量内逗号。

**门禁抓出的真实缺陷（守护测试永久保留）**：

1. `NULL;` 语句被编译为 `TxCommit`（误产生事务边界）。
2. `as_str_loose` 负数十进制渲染为 `0.0-5`（符号位置错误）。
3. `Literal::Float` 降级字符串导致 `x * 0.0008` 内核类型推断失败（V10 已录）。

**幸存变异体评审（66 missed + 13 timeout）**，按类归因：

| 类 | 数量 | 评审结论 |
|---|---|---|
| `collect_stmt/collect_expr` 收集分支（~14） | ir | 变量集的唯一用途是 SQL 文本改写；当变量同时出现在赋值目标（必入集合）时，删除这些分支与现有测试不可区分——属"测试可达性弱"而非缺陷。补充"变量仅出现在嵌套表达式"用例已覆盖一种组合，其余为等价变异 |
| `find_innermost_calls/split_top_level_commas` 偏移算术（~30，含 13 个 timeout 死循环） | rewrite | 引号/嵌套/逗号边界已补 3 类用例；剩余为多重嵌套组合下的中间态变异。timeout 类在真实查询上有内核超时兜底，列为已知残留 |
| 跳转 guard / 标签解析（~6） | ir | 运行语义由内核探针覆盖（native 门控，本配置未编译）；已列入 no-default-features 配置的固有限制 |
| `parse_decimal_literal` 组合校验（~4） | ir | 负数与指数降级已覆盖；剩余为 digit 校验的布尔组合 |
| `unquote`/`upper_eq_find` 等（~4） | rewrite | 现有用例已覆盖主路径，边界组合（成对不闭合引号）留待 M8 内联路径重构时一并处理 |
| value.rs 残留（2） | value | `fits` 的 Record 分支——RECORD 不参与 coerce 主流程，等价变异 |

**结论**：75.8% ≥ 70% 门禁达标；幸存变异体全部归类并登记，其中"等价变异"
类不改代码，"测试可达性弱"类作为下轮测试补强输入（AGENTS §4.4）。

## V13. 本地 GaussDB 容器（Oracle A 模式）重建验证 + 语料级黄金对比

**环境重建**。按 dota-adapter 交付脚本（`prototype-1.7/dota-scheduler/dota-adapter/docker/`）
以 `docker compose up -d` 重建 `dota-gaussdb`：openGauss 5.0.0，**Oracle A 兼容模式**
（`datcompatibility=A`，healthcheck 强制校验），md5 认证经镜像环境变量原生配置
（`GS_HOST_AUTH_METHOD=md5` + `password_encryption_type=1`——无需手改配置文件，
纠正 V7 时代 `password_encryption_type=0` 的手工改造），端口 5433，DOTA schema 自动应用。
SpinDuck 全部 GaussDB 通道（gauss_e2e / golden / golden_corpus）切换至该容器实跑。
复现包已收敛进仓库：容器脚本（compose + 入口）位于 `golden/gaussdb-docker/`，
完整复现说明见 `golden/README.md`；本节所述旧容器（`spinduck-og`，15432，
`password_encryption_type=0` 手工改造）已被其取代。

**新规则（rules-as-data）**：`[patterns] rownum_first_row = true`（config/rewrite-rules.toml）。
`rownum = 1` 首行谓词 → `LIMIT 1`（AST 级 AND 链剥除，OR/NOT 内保守保留；已有 LIMIT 时
保守还原）。语料 42 处 / 14 文件由此获得运行期可执行性。已知损失：平铺
`ORDER BY … rownum = 1` 在 Oracle 中先取首行再排序（结果不定），改写后按序取首行
（确定，通常即业务意图）；语料主流形态"内层 ORDER BY 子查询 + 外层 rownum"两侧语义一致。

**语料级黄金对比（新增，逐字源码）**：`golden/case_corpus_stock_kind/` 与
`golden/case_corpus_min_tax/`，源码逐字取自考题基线 `5_函数/`，不做任何语法改写：

| 函数 | 场景 | 结果 |
|---|---|---|
| `FNC_GET_STOCK_KIND`（%TYPE、rownum=1、内层 EXCEPTION OTHERS、BETWEEN 区间） | 命中/第二区间/未配置/市场无配置 ×4 | 两侧逐行一致 ✅ |
| `FNC_COM_GET_MIN_TAX`（substr 下沉、nvl→coalesce、子查询 ORDER BY DESC + rownum、日期 BETWEEN、DEFAULT NULL 参数） | 精确命中（过期行排除）/组内兜底/全局兜底/无配置归 0 ×4 | 两侧逐行一致 ✅ |

对比口径：数值文本归一（GaussDB NUMBER 按声明 scale 渲染 `0.0000`，SpinDuck `0`，
值相等不作格式差异失败）。

**门禁抓出的真实缺陷（语料级验证的价值实证）**：

1. **限定名误替换**：局部变量与列同名时，`SELECT t.stock_kind` 的列部分被改写为
   `t.$v_stock_kind` → 内核绑定错误 → 被函数内 `WHEN OTHERS` 吞掉后返回兜底值
   'AAAA'。该缺陷在 toy fixture 下不可见（无变量/列同名），语料函数一跑即现。
   修复：两个改写器（`rewrite_tokens` / `rewrite_tokens_with_rules`）均增加限定名
   保护——前一 token 为 `.` 时不替换。守护测试
   `should_not_substitute_qualified_name_parts_sharing_a_var_name`。
2. rownum 规则的回归守护：`should_rewrite_rownum_first_row_predicate_on_real_kernel`
   （平铺与子查询两种形态，真实内核）。

**最终门禁（本轮）**：142 测试全绿（114 lib + 15 编译契约 + 9 表达式 + 2 gauss E2E
+ 2 golden fixture + 2 golden corpus，clippy 0 警告）；语料编译 136/136 维持。

## V14. 语料级重量用例：核心过程 prc_trd_deal_purchase（A 模式容器逐行比对）

*（本节记录的两侧一致结果当时确实在 A 模式容器上跑到。2026-11-06 起黄金层的被比对象
改为调用链上的 `fnc_get_purchase_js_days`，这条过程的 20 列逐行结果改由
`[[case.assert]]` 在进程内内核锁定，参照实例不再 `CALL` 它——原因与分工见 V21.12。
本节覆盖点与守护测试仍是有效记录，未被撤回。）*

**用例**：`golden/case_corpus_purchase/`——考题调用链第 4 步
`pkg_split_trade_step3_sh.prc_trd_deal_purchase`（1090 行核心包中 315 行过程），
源码逐字副本入夹具（核心包 + 依赖函数 `FNC_GET_PURCHASE_JS_DAYS` /
`fnc_com_getday` / `fnc_com_if_deal_date` + 日志桩包）。

**结果：两侧 `dat_trd_repurchase` 逐行逐字段一致**（20 列），`p_o_succeed='0'`：

```
F001|0130000001|A0001|X001|20250715100001|2.000|0.000|10000.000|100|20250715|B|20251013|10500.000|10560.000|5.560|6.220|90|90|0501|S0001
F002|0130000002|A0002|X002|20250715100002|4.000|0.000|20000.000|200|20250715|B|20260111|21000.000|21120.000|5.560|6.220|180|180|0501|S0002
```

覆盖点：`%ROWTYPE` 整行 FETCH、记录字段直接下发 SQL、`<<error_row>>`/GOTO、
嵌套 `EXCEPTION OTHERS`、过程内 COMMIT（延迟事务）、`to_char(to_date(…)+n)`
日期算术、`INTERVALTONUM`、PL 内建 `substr`、依赖函数链、异常路径（第三行
A999 股东未维护 → inst_log 桩 → 跳过，两侧一致）。

**本用例（及为其铺路）抓出并修复的引擎缺口**（每项均有守护测试）：

| # | 缺口 | 修复 |
|---|---|---|
| 1 | 记录字段 `r.fee` 在 SQL 文本中残留 → 内核绑定错误 | `BindSrc::Field` + 前瞻三元组识别 + `$v_<rec>__<field>` 绑定 |
| 2 | FOR 循环记录变量未进变量集（`r.fee` 不被替换） | `collect_stmt` 补 `For` 分支 |
| 3 | `%ROWTYPE` + `FETCH c INTO rec` 走标量路径（"not a record"） | 声明期记录 `%ROWTYPE` 锚定游标，FETCH 单目标按整行记录赋值 |
| 4 | `to_char(to_date(d)+n,fmt)` 逐函数委托经 SpinValue 丢失 TIMESTAMP | `IrExpr::SqlFn`：`@` 标记函数整体下沉单条 SQL |
| 5 | `+(TIMESTAMP, INTEGER)` 内核无候选 | 渲染期 `日期 ± 数字` → `to_days(CAST(n AS INTEGER))`（Oracle 语义） |
| 6 | `INTERVALTONUM` 缺失 / 参数含外层括号 | 新增 `@intertonum` 变换（剥包裹括号 + 顶层 `-` 拆分 → `date_diff`） |
| 7 | `TO_DATE` 独立缺失 | 新增 `@to_date` → `strptime` |
| 8 | Oracle 格式令牌大小写混用（`'yyyymmdd'`）未转换 | `oracle_format_to_strftime` 大小写不敏感 |
| 9 | 重量级函数在测试默认 2MiB 栈上栈溢出 | 测试侧 256MiB 栈线程（引擎 MAX_DEPTH 守护与解析器递归是独立维度） |

另：`par_fund_seat_para.commision_fee_type` 为 varchar(1) 而费型码为 2 字符，属考题
脱敏件列宽缺陷，黄金口径取 `kind_id='0'` 确定性路径并在夹具注释与 golden/README 记录。

## V15. 语料级热点过程：prc_trd_hz_byfund（逐字依赖闭包 + 自适应种子）

**用例**：`golden/case_corpus_hz_byfund/`——考题调用链第 3 步、生产性能热点
`pkg_split_trade_step3_sh.prc_trd_hz_byfund`（763 行过程体）。
*（该用例的容器逐行比对资产随 V21 退役——黄金层依赖 A 模式容器，不在"Qoder 环境内
可执行"的口径内；本节记录的引擎能力与守护测试仍然有效，其 SQL 形态由
`spinduck/tests/corpus_sql_forms.rs` 与 `case_corpus_purchase` 的 `[[case.assert]]`
原生断言覆盖。）*

**资产纪律（本用例新增要求）**：依赖闭包从考题基线**逐字复制、仅目录重组**，
不修改任何源码——43 张表（含 `%TYPE` 锚定与视图基表）、4 个视图、16 个例程
（14 函数 + 2 个 `PROCEDURE` 形态桩）、1 个日志桩包。种子（`cases/datasets/seed.sql`）
为自实现，覆盖主聚合两笔成交、债券类型缺失分支、**风险金动态 SQL 段**
（`par_oper_venture` 一行）与 QFII 开关关闭。

**结果**：两侧 `dat_fund_cjqs` 逐行逐字段一致（25 列 × 2 行）；风险金段输出
`dat_fund_xw_venture` 逐行一致（9 列 × 2 行，`OPEN c_fxj FOR v_sql_txt` 动态
查询 + 函数下沉）。

**为跑通该过程实现/修复的引擎能力**（均有守护测试）：

| # | 能力/缺陷 | 处理 |
|---|---|---|
| 1 | **SQL 上下文例程调用**（`UPDATE SET f(col)`、游标 WHERE 中的函数、`INSERT VALUES` 中的函数）此前无法执行（M8 路径②） | 新增 `sql/sqlfn.rs` 参数元组缓存化下沉：编译期按调用点构造 `SELECT DISTINCT <args> [FROM …] [WHERE …]`（剔除自身谓词防自引用、拓扑处理上下文互引、相同调用去重），调用点内联为相关子查询；运行期逐元组调用例程填 `sd_fnN` 临时表 |
| 2 | 宏化表达式函数被误纳入下沉 | `Engine::macro_names`：已装宏的名字不再下沉（否则 `SELECT n` 无 FROM 报错） |
| 3 | **`%NOTFOUND` 空游标语义错误**：首 FETCH 无行时 `fetched>0 && !last_hit` 为假 → 循环体错误进入（记录未赋值即崩） | `CursorState` 增加 `attempted` 标志：`NotFound = attempted && !last_hit`（Oracle/PG 语义） |
| 4 | `decode (`（格式化后名字与括号间有空格）逃过解析级变换 | `find_innermost_calls` 允许名字与 `(` 之间空白 |
| 5 | `UPDATE tbl t SET t.col = …` DuckDB 不支持限定列名 | 规则 `[patterns] update_strip_alias`（token 级去别名，含 `t .` 空格式） |
| 6 | `SQLERRM` 未支持（异常日志拼接） | 编译期映射为帧变量 `__sqlerrm`，异常派发入处理器前写入错误消息 |
| 7 | 过程查询真实视图（`v_par_bond` 等），此前 DuckDB 侧为占位视图 → 异常分支与参照侧不同源 | harness 用**同一改写管线**把视图 DDL 落到 DuckDB（依赖顺序重试），失败才回落占位并显式打印 |
| 8 | 表 DDL 的 `DEFAULT to_char(sysdate(),…)`（A 模式系统函数） | harness 落 DuckDB 前剔除"非字面量 DEFAULT"（本侧数据由物化提供） |
| 9 | 动态 SQL 未走规则改名（`nvl` 残留） | 目录携带 SQL 规则函数表，动态路径先 `rewrite_tokens_with_rules` 再变换 |
| 10 | 派生表内调用的下沉上下文取错（把调用本身带进元组查询，自引用） | 上下文改为"调用所在 SELECT 块的 FROM/WHERE"（括号层级边界、FROM 右括号配平、子句边界全块扫描） |

**口径说明**：A 模式空字符串即 NULL（Oracle 语义），比对归一 `'' → NULL`。


## V16. 决策落地（PR#1 评审 P2）与守护证据

五项评审决策的实现与验证（守护测试均在 `tests/guard/`）：

| 决策 | 实现 | 证据 |
|---|---|---|
| COMMIT/ROLLBACK 不降级 | 最外层 `BEGIN`…`COMMIT`/`ROLLBACK` 包络；`TxCommit/TxRollback` 即时作用并重开 | `guard_tx_semantics`：COMMIT 后续写持久化；ROLLBACK 撤销 DML；未处理异常统一回滚 |
| 动态文本统一收口 | `EXECUTE IMMEDIATE` 与 `OPEN FOR` 共用 `prepare_dynamic_sql`（ogsql `parse_sql` 带错误通道 → 规则改名 → 变换 → 函数下沉）；撤销 ENG-0062 | `guard_execute_dynamic`：运行期拼接 INSERT 落行；乱码文本显式失败 |
| 内置规则默认加载 | `include_str!` 嵌入种子规则，`default()` 直接可用；`--rules`/项目规则叠加（含 `target_duckdb/probe_duckdb` 覆盖） | 零 `--rules` 时 `NVL→coalesce` 生效（compile 路径）；`rules probe` 漂移检测仍红（回归守护） |
| 口令经 `.env` 注入 | compose `${GS_PASSWORD:?}` + `127.0.0.1` 绑定 + `.env.example`；README/golden 文档去明文 | `git grep` 本仓库跟踪内容 = 0 命中（判定只对内，`lib/fastaas` 是子模块、属另一仓库）。历史提交仍含旧字面量，见下 |
| fastaas 子模块保留 | 现状（私有项目）；CI 可达性留待仓库私有化时评估 | — |

**口令去明文的边界（写清楚，避免读成"已经彻底清干净"）**：本轮改的是**当前内容**——
`git grep` 在 HEAD 上对旧开发口令字面量 0 命中，`.env` 由仓库 `.gitignore` 挡在库外、
`.env.example` 只留占位值，`docker-compose.yml` 用 `${GS_PASSWORD:?}` 强制注入。
但**旧提交里的字面量不会因为这次编辑而消失**：它仍存在于
`75ee3fa`、`b9878bc`、`8b3ca69`、`a5835b2`、`1e43fd7`、`b4a9c03` 的旧 blob 里（当时落在
`README.md`、`golden/README.md`、`golden/gaussdb-docker/docker-compose.yml`、
`spinduck/tests/gauss_e2e.rs`、`docs/validation.md`）。因此处置只有两条真实路径：① 轮换
开发机侧口令（该口令只用于本机 A 模式容器，`127.0.0.1` 绑定，不对外）；② 若要让默认分支
不携带这段历史，走 squash 合并（合并后的 `main` 只含一个干净 tree）。改写已推送分支的历史
属破坏性操作，需另行决策，本轮未做。

**回归**：运行期递归缺陷修复——`RuleRegistry::default()` ↔ `from_toml_str` 曾相互递归（内置规则引入后触发栈溢出），`from_toml_str` 改用显式空基座。

**上游问题登记**：ogsql-parser 对无法解析的动态文本**宽容接受且无错误返回**
（`Parser::parse()` 静默丢弃、`parse_sql` 对 `THIS IS NOT SQL` 返回零错误），
导致 SpinDuck 侧只能在内核执行期显式失败。已向 ogsql-parser 提 issue 请求
严格解析模式/错误通道（见 PR#1 评审回复）。


## V17. 第二轮复审修复 + 变异补充轮（部分）

**第二轮 7 项 Blocker 修复**（守护随项）：
① 9 处调试输出清除（库 3 + 测试 6）；② INT 算术改 checked 溢出（BIZ-0112，
与 DECIMAL 同口径，`should_report_int_overflow_not_wrap`）；③ 动态路径与静态
同口径——目录携带完整规则注册表（patterns 开关 + 函数表），`rownum_first_row`
/`update_strip_alias` 在动态文本同样生效，解析改用带错误通道的
`Parser::parse_sql`，`plan_sites` 传真实函数表；④ 恢复 `EXECUTE IMMEDIATE .. INTO`
（动态与静态同语义：0 行 NO_DATA_FOUND / >1 行 TOO_MANY，
`should_fetch_dynamic_execute_into_targets`）；⑤ CI 修正（`working-directory:
spinduck`、仅初始化 ogsql 子模块——fastaas 私有不拉取、golden job 复用
compose 同口径引导、口令步骤内生成不落仓库）；⑥ golden job 与本地 compose
一致（自定义 entrypoint + healthcheck 强校验 A 模式）；⑦ AGENTS §8/§9 文本
与实际门禁契约对齐（test=native，golden=gauss 容器）。

**变异补充轮（部分结果，跑批中断待续）**：
- 范围 `sqlfn.rs`+`render.rs`，`--features native`；已完成 **155/220**
  （caught 94 / missed 60 / timeout 1，**部分 score ≈ 39%**，显著低于 70% 目标）
- 跑批中断原因（两条结构性发现）：
  1. cargo-mutants 默认副本目录构建会因 `include_str!("../../config/…")`
     越过 crate 根而失败——内置规则使 crate **非独立可拷贝构建**，
     只能 `--in-place`（串行，且与 `--jobs` 互斥），全轮约 3 小时；
  2. 会话重启杀死后台跑批，结果停在 155/220。
- **60 个幸存者清单**在 `spinduck/mutants.out/missed.txt`（不入库），
  聚类显示边界比较与错误分支占比最高；已修复 ② 的 wrapping 溢出即来自
  该类漏检。后续：完成全轮 → 逐个补断言/书面豁免 → 复测达标 ≥70%。
- 待决（Ask first 候选）：内置规则嵌入与 crate 独立构建的矛盾——候选方案
  （a）规则文件移入 crate 目录（config 仓库资产保留为生成源）；（b）保留
  现状并规定变异/复制类工具一律 `--in-place`。


## V18. 第三轮复审修复批（自审—修复—再审）

**门禁实测**（`spinduck/`，2026-11-06）：
`cargo fmt --all -- --check` 干净；`cargo clippy --all-features --tests -- -D warnings` 干净；
`cargo test --features native` **207 passed / 0 failed**（库单测 159 + 集成与守护 48）。

| 评审问题 | 修复 | 证据（测试名） |
|---|---|---|
| §5：统一回滚失败把 BIZ 改判成 KERNEL | 主因保留原域与原码，次因并入消息 | `guard_error_domain::should_keep_business_code_when_rollback_also_fails`、`should_report_single_cause_when_rollback_succeeds`（假执行器注入 ROLLBACK 失败，断言 BIZ-0100 + 双因文本） |
| `compare()` 跨标度对齐 `checked_mul` 之前已溢出 | `pow10`+`checked_mul`，溢出显式 BIZ-0112 | `frame::should_report_overflow_on_wide_scale_compare_not_wrap` |
| 规则键 `[patterns] update_strip_alias` 无人解析、种子与代码漂移 | 解析 + overlay 双侧支持；`sql/patterns.rs` 成为 `[patterns]` 唯一实现；漂移守护 | `rules::should_parse_and_overlay_update_strip_alias`、`should_wire_every_seed_pattern_key`（新增布尔键必须接线，否则 panic） |
| `[dates.formats]` 只在注释里，与代码内格式串表可能漂移 | 载入 `RuleRegistry::date_formats`（含 overlay），代码/数据互验 | `rules::should_parse_seed_date_formats`、`should_keep_seed_date_formats_in_sync_with_strftime_mapper`、`should_overlay_seed_date_formats` |
| catalog 双函数表（`set_sql_functions` 死路 + 与规则漂移） | 目录只携带 `RuleRegistry`，`sql_functions()` 派生 | `catalog::should_derive_sql_functions_from_injected_rules`、`should_keep_sql_functions_consistent_with_default` |
| 静态/动态两套改写管线（阶段顺序不同、`INSERT..SELECT` 漏 rownum、动态漏 update 去别名） | 统一入口 `ir::rewrite_sql`（`apply_text_patterns` → plan → 改名 → AST 变换 → 物化）；动态路径改为复用 | `guard_sql_pipeline_parity::should_produce_same_sql_text_on_static_and_dynamic_paths`、`should_pass_update_and_delete_rownum_through_on_both_paths`、`should_strip_rownum_in_insert_select_on_both_paths` |
| `EXECUTE IMMEDIATE .. INTO` 缺 0 行/多行语义 | 与静态 SELECT INTO 同语义 | `guard_execute_dynamic::should_raise_no_data_found_on_dynamic_into_zero_rows`、`should_raise_too_many_rows_on_dynamic_into_multiple_rows` |
| 过程体内 `IF p_v IN ()` 被解析器降级为 `Empty`，`register` 静默返回 `Ok(0)`（例程消失） | 新增 ENG-0064「声明了例程却未登记任何例程」守护（包规格豁免） | `guard_register_silent_drop`（4 例：规格 Ok(0)/正常 Ok(1)/`IN ()` 命中 ENG-0064/纯 DDL Ok(0)）；`guard_ir_in_list_empty` 改为无条件断言 ENG-0064 |
| 过程内 COMMIT 曾被当 Nop，事务守护偏弱 | 中途提交即时生效并可证持久 | `guard_tx_semantics::should_keep_writes_before_mid_procedure_commit_after_later_failure` |
| CI：submodule 步骤路径错、口令明文入库、`cargo audit` 安装失败被 `\|\| true` 吞 | 三处步骤 `working-directory: .`；口令步骤 `openssl rand` 一次性生成注入；安装不吞错；四条 `zip 6.0.0` 告警 `--ignore` 并记理由 | 本地复现：`spinduck/` 内 `git submodule update --init lib/ogsql-parser` 退出 1，仓库根退出 0；`grep SpinDuckCi` 无明文（口令为一次性随机） |
| README/AGENTS 与实现脱节（ENG-0062、事务行、rownum 覆盖面、job 数量） | 文档回写 + §9 五个常驻 job 与 Platform Build 触发口径 | `docs/architecture.md` 决策 ⑦–⑪ |

**新增本轮自审发现并已修**：`compile` 子命令忽略项目 `[paths].rules`（语料体检与
call/golden 口径分裂）。修复：`ProjectConfig::discover`（自语料目录向上找
`spinduck.toml`）+ `rules_for_corpus`（内置种子为基座叠加项目规则）。
证据 `cli::run::should_overlay_project_rules_when_compiling`（子目录继承、无声明目录回落种子）。

**遗留（已在文档登记，非本轮静默跳过）**：
- §3 规模：`runtime/interp.rs`、`ir/mod.rs`、`sql/rewrite.rs`、`sql/sqlfn.rs` 仍超 500 行，
  拆分在下一节（V19）落地，行为不变、同组测试前后各跑一次。
- `[types]` 链（`spin_type`/`from_gauss_name`）当前无生产调用点：语料建表走参照侧 schema
  物化；`rules.rs` 模块文档与 `spin_type` 文档已如实标注，接入点在 DDL 翻译（V19/考题用例）。
- 探针层（§4.1）仍随规则实现放在单测内，`tests/probes/`、`golden/*/probes/` 只有 README；
  规则 `status` 注释（verified@1.5.5）与 `rules probe` 版本锚定是现有约束，独立探针文件待 M8。
- 变异测试（§4.4）全轮未完成（V17：部分 155/220 ≈39%），幸存者清单不入库；
  `include_str!` 使 crate 非独立可拷贝 → 变异只能 `--in-place` 串行，与编辑期互斥，
  安排在合入后的独立窗口执行。

## V19. §3 规模拆分轮（机械切分，行为零变更）

**门禁**：`cargo fmt --all -- --check` 干净；`cargo clippy --all-features --tests -- -D warnings`
干净；`cargo test --features native` **207 passed / 0 failed** —— 与拆分前同一数字（本轮未增删
任何测试，测试集合逐条同名）。

**方法**：只按职责边界移动代码（函数体一字不改），跨文件可见性用 `pub(super)` / `pub(crate)`
收口；每完成一个文件立即跑同一组测试。忠实性用行级比对校验：把 HEAD 原文件与拆分后的全部
新文件的规范化行（去 `use`/`mod`/单字符大括号行、去 `pub(...)` 前缀、折叠空白）做多重集差，
差异只出现在可见性限定符与 rustfmt 重新折行处，注释行零丢失。

| 原文件（行数） | 拆分去向 | 拆后最大文件 |
|---|---|---|
| `runtime/interp.rs`（1652） | `runtime/interp/{binds,cursors,dynamic,flow,routine,sqlstmts,trace}.rs` + `interp/{tests,m5,kernel_tests}.rs`；主文件只留主循环与 trap 展开 | 471（`interp/kernel_tests.rs`，测试文件） |
| `ir/mod.rs`（1216） | `ir/{types,entry,control,stmt,sql,expr}.rs`；`mod.rs` 只做装配与再导出（29 行） | 483（`ir/stmt.rs`） |
| `sql/rewrite.rs`（841） | `sql/rewrite/{tokens,scan,split,ast}.rs` + `rewrite/{tests,transform_tests}.rs` | 227（`rewrite/ast.rs`） |
| `sql/sqlfn.rs`（813） | `sql/sqlfn/{plan,region}.rs` + `sqlfn/tests.rs`；类型集中在 `sqlfn/mod.rs` | 417（`sqlfn/region.rs`） |
| `runtime/frame.rs`（677） | 求值整体移到 `runtime/eval.rs`（含 `eval/numeric.rs` 数值定标），`frame.rs` 只留帧本体 | 368（`runtime/eval.rs`） |
| `project.rs`（553） | `project/{case,fs}.rs` + `project/tests.rs`，为 V20 的入口声明腾出空间 | 179（`project/mod.rs`） |

`docs/architecture.md` §2 模块树同步为真实布局（含 `sql/exec_native.rs` 与 `io/gauss.rs`
的既有事实），并修正 §3.4 / §8.3 中的旧路径引用。

**遗留（登记，非静默跳过）**：
- `engine.rs` 664 行（非测试部分 429 行）依 §3「含 `#[cfg(test)]` 总行 ≤700」保留，不拆。
- §3 单函数 ≤60 行：`ir/stmt.rs::stmt_inner`（约 490 行）与 `runtime/interp.rs::run_inner`
  仍是巨型 match 分发，属计划债（按语句类别下沉到 `interp/` 子模块是既定方向），本轮不动，
  不在文档里声称已达标。
- §4.4 变异测试范围路径随本次拆分更新为 `src/ir`、`src/sql/rewrite`、`src/sql/sqlfn`、
  `src/runtime`、`src/value.rs`（`cargo mutants -f` 目录写法不变，无需改 AGENTS §8 命令）。

## V20. 入口声明与链路核验（`[[entry]]` + `spinduck verify`）

**动机**：接入验收的口径是"从对外可调用面出发，链路上每个对象都能落地"，而不是
"文件都编译得过"（设计 §12.1）。本轮把可调用面写进项目声明，并让 `verify` 从它出发
逐对象核验；同时回答"项目配置文件也要出现在运行态目录下"这一设计问题。

**门禁**：`cargo fmt --all -- --check` 干净；`cargo clippy --all-features --tests -- -D warnings`
干净；`cargo test --features native` **235 passed / 0 failed**（lib 181 + 集成/守护/CLI 54），
较 V19 的 207 净增 28 条测试，全部为本轮新增行为断言。

**测试分层（§4.1）**：

| 层 | 测试 | 条数 |
|---|---|---|
| 单元（不触内核） | `verify::chain`（SQL/IR 链路抽取 7）、`verify::run`（`ChainFacts` 手工快照判 8）、`verify::deploy::sql_units`（1）、`verify::manifest`（2）、`project::*`（入口声明 2 + 运行态快照定位 1） | 21 |
| 内核集成（真实 DuckDB） | `spinduck/tests/verify_chain.rs`：入口命中/断点/表缺席/场景可执行/显式 Skip/退出码/运行态快照 | 6 |
| CLI 契约 | `cli::args::should_parse_verify_with_optional_path_and_reject_extra_args`、`cli::run::should_fail_verify_when_project_file_is_missing` | 2 |

**运行态快照的三条实测结论**（`verify_chain::should_snapshot_manifest_in_home_conf_and_verify_runtime_dir_alone`）：

1. 研发态 `verify` 落 `<home>/conf/spinduck.toml`，快照带 `source`/`generated_by`，
   资产路径全部绝对化，报告行 `manifest: …/conf/spinduck.toml (updated)`；同内容重复
   同步报 `unchanged`。
2. 把 `.spinduck/` 单独作为项目目录再跑 `verify`，得到**同一结论**（`entries=3 failed=3`、
   退出码 1），且快照一字不变——`ProjectConfig` 认 `<dir>/spinduck.toml` 与
   `<dir>/conf/spinduck.toml` 两处（前者优先），运行态因此自足。
3. 从运行态目录执行时**不回写**快照（`(未改写)`），来源标注始终指向手写真源。

**本轮抓到的两个自身缺陷（都有守护测试，非事后补写）**：

- **链路文本重复计数**：`verify::chain::walk_code` 原先在 `PushTrap` 处按
  `TrapEntry.body_start..body_end` 递归展开，而处理器体与主语句本就在同一 `code`
  区，线性遍历已覆盖 → 每个处理器体的 SQL 被计两次（`sql_texts` 实测
  `[T_BODY, T_HANDLER, T_HANDLER]`）。守护测试
  `should_walk_exception_handler_bodies` 锁定"按文档序各计一次"，`PushTrap`/`PopTrap`
  降级为纯标记，`MAX_TRAP_DEPTH` 随之删除。
- **核验失败仍落运行态快照**：`verify_cmd` 原先先同步清单再核验，导致一个没有
  `[[entry]]`、直接 `ENV-0030` fail-fast 的项目也会得到一个"声称可调用面"的快照文件。
  守护测试 `should_not_write_manifest_when_verify_fails_before_reporting` 断言快照不得存在，
  顺序改为"核验通过 → 落快照 → 渲染"。

**静态扫描的近似口径（登记，不声称已完备）**：SQL 侧对象名取自改写前的词法扫描
（`FROM`/`JOIN`/`UPDATE`/`INSERT|MERGE INTO` 后首个标识符、`<seq>.nextval|currval`），
字符串字面量与表函数不计，`EXTRACT(unit FROM ts)` 按括号属主排除；子查询位
（`FROM (`）不计。已知假阴面：动态拼接文本里的表名、以及写成函数调用形态的关系名——
由 `Scenario` 的真实执行兜住（调用即报错）。`docs/architecture.md` §12.1 已把该口径写成正文。

**对既有 golden 项目的影响**：`golden/case_*` 五个项目尚未声明 `[[entry]]`，对其执行
`spinduck verify` 得到 `[ENV-0030] spinduck.toml: 至少一条 [[entry]]（verify 的核验起点）`
并退出 1——黄金层（`cargo test --features gauss`）不消费 `verify`，故无任何回归。
入口声明随 V21 的语料级案例一并补齐。

## V21. 语料形态覆盖门禁 + 用例原生断言层（零外部依赖）

**口径（2026-11-06 决策）**：考题业务函数**不在 golden 层执行**。可验证性改为三条
在 Qoder 环境内直接可跑的通道，全部跑在进程内 DuckDB 1.5.5 上：

1. **形态覆盖门禁**——把语料里出现过的每一种 SQL/PL 语句形态内联复刻并逐形态断言
   （`spinduck/tests/corpus_sql_forms.rs` + `config/corpus_forms.toml`）；
2. **用例断言集**——非简单的函数/过程/包在自己的 `*.case.toml` 里声明显式断言
   （`[[case.assert]]`，四维多粒度，`spinduck/tests/cases_native.rs` 消费）；
3. **链路核验**——`spinduck verify` 从 `[[entry]]` 出发逐对象判定，已入库项目
   `golden/case_corpus_purchase` 现在无容器即可跑通。

原用例 5（`golden/case_corpus_hz_byfund`，热点过程容器逐行比对）随本轮**移出仓库**；
其引擎能力与守护测试全部保留（V15 表格仍是机制说明），语料资产不再入库（考题目录含
远端库凭据，只本地存在）。V20 段末"五个项目尚未声明入口"以本节为准：四个在库用例中
purchase 已声明并跑通，其余三个的主角是无包限定的独立例程，按"入口必须全名定位"不声明。

**门禁**：`cargo fmt --all -- --check` 干净；`cargo clippy --all-features --tests -- -D warnings`
干净；`cargo test --features native` **292 passed / 0 failed**（lib 219 + 集成/守护/CLI 73），
较 V20 的 235 净增 57 条；`cargo audit` 按 CI 的构建期 `zip` 豁免清单
（§9，四条 RUSTSEC 公告）跑为干净，裸跑仍只报该既有清单、无新增公告。

**环境侧一次性观察（登记，非缺陷）**：在同一 `target/` 里先后跑过
`--all-features`（clippy）与 `--features gauss` 之后，`cargo test --features native` 的
**doctest 阶段**报 `E0463 can't find crate for ogsql_parser/duckdb`——rustdoc 拿到的是
`-C metadata` 不同的依赖产物路径（三套 feature 的产物在 `target/debug/deps` 并存）。
处置口径实测为两步：`cargo clean -p spinduck -p ogsql-parser -p duckdb` 后**首跑**仍可能在
doctest 阶段红（测试二进制 17 组 292 passed / 0 failed 全绿，只有 doctest 红），
再跑一次 `cargo test --features native`（或单跑 `--doc`）即全绿——本机实跑
292 passed + doctest 0 例通过。CI 的 clippy/test/golden 是**独立 job**，不同享该目录，
不受此竞态影响。

### V21.1 语料形态清单与覆盖门禁（86 构造 / 135 条形态断言）

**清单是数据，不是散文**：`scripts/extract_corpus_forms.py` 扫描语料 `sql/` 全部
`.sql`（`files = 147`，其中本项目自实现桩 `files_stub_authored = 14`），按构造正则统计
命中次数，输出脱敏清单 `config/corpus_forms.toml`——**只含构造名、命中次数、文件代号**
（代号↔路径映射不入库）。有命中构造 86 项，零命中构造 31 项（`[zero_hits]`，其中
`reject_*` 5 项对应 `[rejects]` 面）。

消费方 `spinduck/tests/corpus_sql_forms.rs`（9 条测试）按六个维度表登记形态：

| 维度 | 表 | 条数 | 断言面 |
|---|---|---|---|
| 标量求值 | `expr::SCALARS` | 52 | `SELECT <形态>` 在进程内内核逐值比对 |
| 改写产物 | `expr::REWRITES` | 9 | 改写后 SQL 文本含目标方言片段 |
| 查询形态 | `query::QUERIES` | 16 | 行集（聚合/连接/集合/`rownum`/`WITH`…） |
| DML/DDL | `dml::STATEMENTS` | 15 | 建表+灌数+语句+读回状态 |
| PL 函数 | `pl_fn::FNS` | 22 | 注册 → 调用 → 返回值 / `ERR:` 前缀错误契约 |
| PL 过程·包 | `pl_proc::ROUTINES` | 21 | 表状态 + OUT 参 / 返回值 |

共 135 条形态断言，按 `key` 归并后恰好覆盖 86 个有命中构造（同一构造可由多条形态
证据支撑）。

两个**门禁**测试把清单和测试表绑成双向闭环：

- `every_corpus_construct_has_an_assertion`：清单有、测试无（未覆盖）或测试有、清单无
  （凭空造形态）任一成立即变红——新增语料形态必须先重跑抽取脚本，否则红。
- `reject_faces_are_absent_from_corpus`：`reject_*` 必须在零命中清单里；一旦语料出现
  `DBMS_*`/`PARTITION BY`/`DISTRIBUTE BY`/`%ROWTYPE 进 SQL`/`autonomous_transaction`，
  即要求重新评审 `[rejects]`（AGENTS §6：收缩兼容范围需业务确认）。

取实测值探针（不参与门禁语义）：
`FORMS_PROBE=1 cargo test --features native --test corpus_sql_forms -- --nocapture`。

### V21.2 `[[case.assert]]`：用例自带的多维断言集

声明面（`spinduck/src/project/case.rs`，四维表见 `architecture.md` §12.1）。校验在
**解析期**：`kind`/`target`/`want` 缺项、`out` 缺 `param` 一律 `ENV-0031`——空 `want`
等于永真断言，不接受静默通过。守护测试：
`project::tests::should_parse_case_assert_blocks_by_kind`（四维各一条，含
`want = "BIZ-0100|no data found"` 的码+片段拆分）、
`should_reject_case_assert_with_unknown_kind_or_missing_fields`（四条残缺体，逐条比对
错误码与消息片段）。

消费方 `spinduck/tests/cases_native.rs`（`#![cfg(feature = "native")]`，4 条测试）用与
`verify` **同源**的执行口径（`deploy_project`：表 DDL → 视图 → 桩 → 例程 → 种子）在进程
内内核上跑完场景再逐条断言；`assert!(n_asserts > 0)` 让"项目里一条断言都没有"显式变红。

**实测值（`CASES_PROBE=1` 取数、原样填回 `want`；共 20 条，四个项目全绿）**：

| 项目 | 条数 | 关键实测 |
|---|---|---|
| `case_split_agg` | 4 | 产物行集 `[20240105, F001, 600000, 300, 3250, 2.6] [20240105, F002, 000001, 50, 1012.5, 0.81]`；`call fnc_fee(12500)` = `10`（SQL 上下文下沉）；`WHERE fund_code='ERR'` = `[0]`（正常路径未误入处理器） |
| `case_corpus_stock_kind` | 5 | `01` / `02`（BETWEEN 字符区间）、`AAAA` × 2（区间未覆盖 / 市场码无配置 → `EXCEPTION WHEN OTHERS` 兜底）；区间表行集 3 行 |
| `case_corpus_min_tax` | 7 | `1.5`（精确档，`ORDER BY stock_kind DESC` + `rownum = 1`）、`2.5`（组内 `0100` 退档）、`4`（全局 `0000`）、`0`（三档全缺 → 内层 `NO_DATA_FOUND` 被 `WHEN OTHERS` 吞掉，`RETURN nvl(RESULT,0)`）；**省略两个 `DEFAULT` 形参**仍得 `1.5`；`rows` 两条互为防假绿（有效期谓词命中 `[0102, 1.5] [0100, 2] [0000, 3]`，过期行确实存在 `[4, 20231231]`） |
| `case_corpus_purchase` | 4 | `out p_o_succeed` = `0`；指令表 8 列 × 2 行（A999 异常行处理口径）；量额合计 `[2, 300, 30000, 6]`（3 行游标输入 → 2 行指令）；`error` = `BIZ-0151\|… 缺少实参 p_o_succeed（形参未声明 DEFAULT）` |

**harness 自身的变异验证**（AGENTS §4.4 精神，一次性手工做）：把 `split_agg` 的一条
`want` 末位 `0.81` 改成 `0.82` → `native_asserts_split_agg` 断言失败并打印差异，改回即绿
——证明该层不是永真比较。

**两条已登记的断言撤回**（不是"改期望值凑绿"）：
`prc_trd_hz_byfund` 的 OUT 断言在原生层报
`KERNEL-0010 Scalar Function with name fnc_com_get_fund_code does not exist`——该过程的
依赖闭包随用例 5 一起退役，缺的不是断言而是源码；`db_log` 日志桩行集实测为空，钉一个
空集没有信息量，改用量额合计断言替代。

### V21.3 为跑通语料形态实现/修复的引擎能力（每项挂测试）

| # | 能力 / 缺陷 | 处理 | 测试 |
|---|---|---|---|
| 1 | **A 模式 `\|\|` 传播 NULL**：基线 `'A' \|\| NULL = 'A'`，DuckDB 得 NULL，整串被判空 | `[patterns] concat_null_as_empty`（新模块 `sql/rewrite/concat.rs`）：`a \|\| b \|\| c` 折叠为 `concat(a, b, c)`；全 NULL 仍得 NULL（与基线一致，形态实测 `NULL`）；不动字符串字面量内的 `\|\|`；只做局部文本替换，其余字节逐字保留。数值拼接口径顺带由此承担（`[implicit] concat_numeric_to_varchar` 注释同步） | `eval.rs::should_treat_null_as_empty_string_in_concat_per_a_mode`（PL 求值层）、`concat.rs` 5 条（折叠/字面量/嵌套实参/数值）、`expr.rs` 形态 `string_concat` 4 条（`ABC` / `N=2` / `A` / `NULL`） |
| 2 | **Oracle `(+)` 外连接标记**（语料 8 处）DuckDB 解析期即报错 | `[patterns] oracle_outer_join`：标记所在表转 `LEFT JOIN … ON`，标记前的逗号表折成 `CROSS JOIN`；标记不在最后一位 / 两侧都有标记**不改写**（宁可报错也不给错语义） | `patterns.rs` 5 条（正例 2 + 负例 2 + 开关关闭 1） |
| 3 | **`seq.NEXTVAL`**（语料 12 处）被当表名解析 | `[patterns] sequence_nextval` → `nextval('seq')`；`xnextval`、字符串字面量不误改 | `patterns.rs` 3 条 |
| 4 | SQL 上下文里的例程调用**嵌套实参**（`f(g(x))`）此前 `BIZ-0158` 直接拒绝 | 新增 `sql/sqlfn/lower.rs` 逐层下沉：内层先物化，外层元组查询引用内层缓存表；**BIZ-0158 下线**，原拒绝测试 `should_reject_nested_routine_call_in_arguments` 替换为下沉断言（行为变更，非删除覆盖） | `sqlfn/tests.rs`：`should_lower_routine_call_in_another_call_arguments`、`…_nested_in_another_call_arguments`、包限定调用在 SQL 上下文命中、目录未登记的限定调用不下沉 |
| 5 | 目录只登记 `package.procedure` 限定名时，SQL 里的**裸名调用**不下沉 | 下沉期按"限定名优先、裸名唯一命中"匹配 | `should_match_bare_name_when_only_qualified_key_is_registered` |
| 6 | **下沉缓存列与 PL 变量同名**（`v`/`a0`）→ 变量绑定抢走缓存列，取值为空 | 缓存列一律走保留命名空间 `sd_`（`sd_fnN`/`sd_v`/`sd_a0…`；语料 `sd_` 零命中，见 §8.3） | 守护 `should_not_bind_cache_columns_when_pl_variable_shares_their_names` |
| 7 | `INSERT … SELECT` 多分支时参数元组取了**单一上下文** | 按分支各自取上下文；`UNION ALL` 每分支独立物化 | `should_take_insert_select_context_per_branch_for_parameter_tuples` |
| 8 | 下沉产生的 `sd_fnN` 临时表被链路核验当成**缺失关系** | 链路对象抽取排除下沉计划表 | `verify/chain.rs::should_exclude_lowering_plan_tables_from_object_refs` |
| 9 | **形参 `DEFAULT`**（语料 22 处）此前省略实参即崩 | 登记期把字面量 DEFAULT 编进目录；调用侧省略的必须是**尾部且声明了 DEFAULT** 的形参，否则 `BIZ-0151`（文案改为点出形参名）；非字面量默认值登记期 `BIZ-0203` 拒绝 | `ir_expr_forms.rs` 4 条（补默认值 ×2、缺参拒绝、非字面量拒绝） |
| 10 | **例程级异常处理器**缺失；处理器末尾**裸 `RAISE;`** 不重抛 | `interp/m5.rs`：处理器体内的裸 RAISE 重抛**当前**异常（保码、保域、保消息），处理器外裸 RAISE 报 `BIZ-0201`（"无当前异常"），处理器结束后不再执行主语句尾部；`RAISE <具名条件>` 语料零命中 → 编译期 `BIZ-0202` 显式拒绝（旧实现编造常量消息，把未覆盖面伪装成已支持） | `m5.rs` 5 条（处理器运行、处理器后不续跑、重抛保码、处理器外拒绝、具名条件拒绝） |
| 11 | `decode` 参数里的**多字节字面量**被顶层逗号切分**按字节**当字符处理，中文实参被截断 | 切分按 `char_indices` 推进，字面量逐字保留 | `rewrite/transform_tests.rs::should_preserve_multibyte_literals_in_decode_arguments` |
| 12 | 内核语句失败会**作废整个显式事务**（DuckDB 无 `SAVEPOINT`）→ `WHEN OTHERS` 处理器体内的 SQL 连锁报 `Current transaction is aborted`（语料 156 处记日志形态的前提） | `interp/flow.rs` 新增"处理器进入前的会话复位"（不经 `engine::call` 直接 run 时无活动事务，回滚失败可忽略）。**已知口径差异**：复位把上次 COMMIT 之后的本段 DML 一并撤销，不等价于 Oracle 语句级回滚 | `exec_native.rs::should_keep_session_usable_after_failing_statement`、`engine.rs::should_run_handler_after_kernel_error_in_same_session`（`o = 'caught'` 且日志表 1 行） |
| 13 | **`SQLERRM`/`SQLCODE`**（语料 32 处）消息文本取自内核 | 编译期映射为帧变量；口径：内核给文本，随内核版本复核，**只钉前 8 字符**（实测 `SUBSTR(SQLERRM,1,8)` = `Conversi`） | `pl_proc.rs` 形态 `pl_sqlerrm_sqlcode`（OUT 参逐值比对） |

### V21.4 部署路径归一（`verify::deploy`，四处实测结论）

链路核验的"能不能落地"取决于部署文本，而语料 DDL 里有内核不接受的子句。归一序列
固定为 `strip_defaults → sql_units 切分 → clamp_sequence_bounds → drop_cache_option →
规则改名 → AST 变换`：

- `drop_cache_option` 只作用于 `CREATE SEQUENCE`（`should_drop_cache_option_only_from_sequence_ddl`
  断言 `CREATE TABLE cache_t` 一字不变）。
- `clamp_sequence_bounds` 剥离超 BIGINT 的 `MAXVALUE`、保留 `START WITH` /
  `INCREMENT BY` / `MINVALUE` / `NO CYCLE`；对语料逐字序列
  `seq_dispatch_job_id`（`MAXVALUE 999999999999999999999999999` + `CACHE 1`）实测剥离后
  首次取号仍为 `20001`（`corpus_sql_forms::deploy_path_strips_corpus_sequence_options`）。
- 视图优先用**真实视图体**建到内核，内核解析不动才回落占位视图并显式打印
  （`verify_chain.rs` 两条：真体成功 / 占位回落）——异常分支与参照侧同源，不再静默降级。
- 表 DDL 的 `DEFAULT to_char(sysdate(),…)` 一类非字面量默认值在落内核前剔除
  （本侧数据由物化/种子提供），与 §3.4.1 的例程 DEFAULT 口径同源但**不同判定点**。

### V21.5 本地考题核验（不入库，仅记录结论）

`cargo run --quiet --features cli -- verify ../golden/exam_split_trade`：
`entries=10 checks=215 failed=2 skipped=9`，退出码 1。

- 2 处 FAIL 均为 `Relation`：`v_dat_inst_base_info` / `v_dat_inst_base_info_s` 被
  `PKG_SPLIT_TRADE_STEP9.sql` 引用，而语料 `sql/ddl/views/`（11 个视图）里没有它们的
  DDL——**语料缺口，不是引擎缺陷**；这正是 `verify` 要暴露的那一类"链路引用了项目里没有
  的对象"。补齐方式是取得视图 DDL 或写显式桩，不在引擎侧伪造。
- 9 处 SKIP 是"无 `*.case.toml` 声明该入口"（可执行性未验证，按 §12.1 不计失败）；
  唯一声明场景的 `exam_bjgsyh#1` PASS（`outs=1`）。
  **V22 起该理由分两档**：有 `[[case.assert]]` 指向该入口时不再写"未验证"。这里的计数是
  当时实测状态，不追溯改写；考题项目不入库，故该命令不可在 clone 里复现。
- 在库项目的同一命令：`verify ../golden/case_corpus_purchase` →
  `entries=1 checks=11 failed=0`，并把运行态快照落到 `.spinduck/conf/spinduck.toml`。
  V22 在同一项目复跑为 `entries=1 checks=11 failed=0 skipped=1`，那 1 条 SKIP 的理由已改为
  "2 条 `[[case.assert]]` 已由用例 `corpus_purchase` 覆盖该例程，本命令不执行断言"。
- "不入库"的范围已核实到文件级：`golden/exam_split_trade/` 共 163 个文件，其中只有
  `docs/4-数据库与连接信息.md` 含远端库凭据；`sql/` 147 个逐字件（36 例程 + 107 表/视图
  DDL + 4 外部依赖桩，与 `config/corpus_forms.toml` 的 `files = 147` 同口径）、`cases/`（4）、
  `rules/`（1）、`spinduck.toml` 经 IPv4 / 连接串 / 口令关键字复核均无凭据。
  **2026-11-07 定案**：整目录（含那 162 个无凭据件）一律不入库——"只排除那个 docs 件、其余
  入库"的备选方案已被明确否决，落点问题不再重开。因此入库面就是 4 个语料用例
  （`case_corpus_min_tax` / `case_corpus_purchase` / `case_corpus_stock_kind` / `case_split_agg`）+
  `config/corpus_forms.toml` 的脱敏形态清单；八个核心过程的逐字源码只作为**本地验证件**存在，
  `golden/exam_split_trade/` 由 `.git/info/exclude` 挡在库外。日后要扩大入库面，需重新走一轮
  口径确认，而不是沿用本节读作"随时可入库"。

### V21.6 未覆盖面（登记，不声称已支持）

语料零命中、本轮也**没有**顺手实现的形态。两类证据要分清：`RAISE_APPLICATION_ERROR`
与 `EXIT <label>` 是本轮对语料**直接 grep 实测 0 处**（抽取脚本没有为它们定义构造，
故不在清单里）；其余形态以清单 `[zero_hits]` 为证据——`DBMS_*` 包、`BULK COLLECT`/
`FORALL`、`SAVEPOINT`、`CONNECT BY`、`MERGE`、解析函数 `MOD`/`NUMTOINTERVAL`/`REPLACE`/
`UPPER`/`LOWER`，以及 `[rejects]` 面对应的 5 项 `reject_*`。另有三处**已知口径差异**
写在形态 `why`/模块文档里而非改代码掩盖：A 模式 `'' ≡ NULL` 本引擎不采纳
（`literal_empty_string_null` 实测 `notnull`，黄金比对层按归一化同值处理）；
`SQLERRM` 文本由内核给出，形态断言只钉前 8 字符，内核升级（V6 门禁）须复核；
处理器进入前的会话复位撤销的是"上次 COMMIT 之后的本段 DML"，不是 Oracle 的语句级回滚。

### V21.7 golden job 的两轮 CI 实录（口令四类字符 → 参照侧 connection closed）

**第一轮（run 42181608285）**：golden job 第一次真正走到容器引导即红在
`Error: The supplied GS_PASSWORD is not meet requirements`——镜像 initdb 要求
**四类字符齐全**（大写/小写/数字/特殊字符），而一次性口令生成器
`A$(openssl rand -hex 12)9z` 只有三类。补尾部 `@`（官方示例 `openGauss@123` 同类，在
double-quote 与 compose 插值中无需转义）。同轮顺带修 `.gitignore` 的 `.env.*` 误吞
`golden/gaussdb-docker/.env.example`（compose 与 README 都引用该模板，新克隆拿不到）。

**第二轮（run 42186167205）**：format / clippy / test / audit 四 job 全绿；golden
容器 healthy 后跑完全部套件，**只有 `golden_corpus_purchase` 红**，且红在参照侧：
`CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase('20250715', null):`
`[ENV-0020] ENV: gaussdb: connection closed`（该用例 0.29s 即失败，早于本地 0.42–1.53s，
形态是"后端进程已不在"，不是比对不符）。同 job 内 `golden_gauss` 1 + `golden_corpus_gauss`
2 + `gauss_e2e` 2 与全部 native 套件（219 lib + 73）均绿。

**本地复现矩阵**（全部用隔离容器与新卷，未动开发机上在用的 `spinduck-gaussdb`）：

| 组合 | 结果 |
|---|---|
| arm64 镜像 + 新卷 + 2G 限额，purchase 单跑 | 绿（0.40s） |
| arm64 镜像 + 新卷 + 2G 限额，`--features gauss` 全量 | 绿（17 组 0 failed） |
| amd64 镜像（Rosetta）+ 新卷 + 2G 限额，purchase 单跑 | 绿（0.53s） |
| amd64 镜像 + 新卷 + 2G 限额，全量 gauss 套件 | 绿（17 组 0 failed） |
| amd64 镜像 + 新卷 + 固定 GUC（`max_process_memory=2GB`/`shared_buffers=256MB`）+ 3G 限额，全量 | 绿 |

purchase 用例期间容器 RSS 峰值实测：2G 限额下 51.75%（≈1.06 GiB），固定 GUC + 3G
限额下 1.44 GiB——**计算量本身不逼近限额**。

**定位到的环境差**：镜像默认按**宿主**内存推导 `max_process_memory`（本机容器内实测
12GB）与 `shared_buffers`（1GB），这两个值不随仓库固定；CI runner 的宿主内存不同，推导
结果也不同，而实例总内存越过容器限额时是内核直接 OOM kill 后端，客户端只剩一句
`connection closed`。本地三轮未能复现 runner 侧失败，根因证据尚未拿到——不猜测、不掩盖：
golden job 现在红时自动落三段证据（容器 stdout 尾部 200 行、实例 `pg_log` 最新两文件
尾部 120 行、runner 侧 `free -m` + `dmesg` 的 OOM 行），下一轮无论绿红都有可判读的事实。

**本轮 compose 侧的确定性改动**（已用隔离容器验证不影响任何套件结果）：`OTHER_PG_CONF`
固定 `max_process_memory = 2GB` + `shared_buffers = 256MB`，`deploy` 限额 2G→3G、
预留 512M→1G——限额高于实例口径，越限先由 openGauss 看护报错而非被内核杀掉。镜像每次
启动把这几行**追加**进 `postgresql.conf` 末尾（后写覆盖先写），旧卷无需 `down -v`。
一次性口令另加 `::add-mask::`，此前步骤 env 转储会把口令原文打进公开 runner 日志。

### V21.8 失败窗口取证改造（含 V21.7 两处结论的更正）

**runner 第四轮实录**（run 42215030234，head = `5d9b419`，即"固定 2GB 口径 + 红时落证据"
两轮改动都在生效的那一轮）：

- 同一 job 内 purchase **之前**的参照侧真调用全部成功：`case_corpus_stock_kind` 4 场景、
  `case_corpus_min_tax` 4 场景（含 EXCEPTION OTHERS 路径）、`golden_gauss`、`gauss_e2e`
  皆绿，219 条 lib 测试绿。所以 runner 上网络/认证/DDL/PL 部署/包调用链路是通的，
  失败只在 `prc_trd_deal_purchase` 的 CALL 本身。
- 失败时刻线：`test result: FAILED. 0 passed; 1 failed ... finished in 1.48s`，
  panic 于 12:17:25.355。主实例 `pg_log` 里能取到的最后一条是 12:17:23.979 的后台
  UNDO 记录，其后没有任何 FATAL/PANIC/kill 行。
- 证据为什么是空的（三条，都已修正）：① 采集距 panic 只有 1.2 秒，syslogger 还没落盘；
  ② 口径 `ls -t | head -2 | xargs tail -n 120` 取到的"最新两文件"里一份是 initdb 临时
  实例的收尾（`received fast shutdown request` / `Gaussdb exit(0)`），另一份主实例日志
  在这 120 行窗口里全是每 60 秒一次的 checkpoint 噪声；③ `dmesg -T 2>/dev/null` 在
  runner 上把"非 root 读不到内核环形缓冲区"伪装成了"没有 OOM 记录"。
- 同时读到的旁证：失败后 1.2 秒 `docker stats` = 452MiB/3GiB、CPU 75%、38 个 PID，
  容器活着（不是 PID 1 被杀）；`free -m` available 14466MB，宿主无压力。

**对 V21.7 的更正**（都是实测，不是措辞问题）：

1. 「旧卷无需 `down -v`，镜像每次启动把这几行追加进 `postgresql.conf`」——**错**。
   镜像 entrypoint 的 `opengauss_setup_postgresql_conf`（`echo -e "$OTHER_PG_CONF"` 在
   其中）位于 `if [ -z "$DATABASE_ALREADY_EXISTS" ]` 分支内，只在**空数据目录首次
   initdb** 时执行。证据：开发机上卷早于该改动的在用容器，改 compose 后 `SHOW
   max_process_memory` 仍是 12GB / `shared_buffers` 1GB。旧卷改口径的两条可用路径：
   `down -v` 重建，或把三行手工追加到 `$PGDATA/postgresql.conf` 末尾再
   `docker compose up -d` 重建容器——实测后者生效（SHOW 变为 4GB / 512MB）。两个参数
   都是 `postmaster` 上下文（`pg_settings.context` 实测），`reload` 不够。
2. 「限额高于实例口径后，越限先由 openGauss 看护报可读错误」——**不成立**。写死口径
   （2GB 与 4GB 都一样）启动日志必现
   `WARNING: Failed to initialize the memory protect for ... cstore_buffers (512 Mbytes)
   or shared memory (9xx Mbytes) is larger`，跟随宿主推导（12GB）时不出现。含义是
   实例级内存看护处于关闭状态，超限仍然是内核静默杀后端。因此正确处置是
   **限额 ≫ 预算**，而不是指望看护报错。

**新口径 4GB/512MB + 限额 6G 的实测依据**（隔离容器 + 新卷，全量 `--features gauss`，
298 条 ok 为绿；峰值取容器 cgroup `memory.peak`）：

| 架构 | `max_process_memory`/`shared_buffers` | 限额 | 结果 | 容器内存峰值 |
|---|---|---|---|---|
| arm64 | 宿主推导 12GB / 1GB（旧卷在用的开发容器） | 2G | 298 ok | 1.39GB |
| arm64 | 写死 2GB / 256MB（新卷） | 3G | 298 ok | 1.099GB |
| amd64 | 写死 2GB / 256MB（新卷） | 3G | 298 ok | 1.707GB |
| amd64 | 写死 4GB / 512MB（新卷） | 6G | 298 ok | 1.744GB |
| arm64 | 写死 4GB / 512MB（旧卷手工追加 = shipped compose） | 6G | 298 ok | 1.034GB |

amd64 的峰值比 arm64 高约 55%，而 2GB 预算 + 3G 限额只剩 ~1.7 倍余量；GitHub runner
实测 16GB（available 14466MB）没有理由把参照实例卡这么紧，故抬到 4GB/6G。
本地仍未能复现 runner 侧的 purchase 失败——**本轮的目标不是猜中根因，而是让下一轮
无论绿红都能判读**。

**golden job 的取证改造**（`.github/workflows/ci.yml`，红绿都落日志）：

- 常态 fingerprint 步骤：打印 `SHOW max_process_memory / shared_buffers /
  server_encoding` + 容器内 cgroup `limit/peak` + 内存看护初始化失败行数。因为
  `OTHER_PG_CONF` 的生效条件在"卷"上，runner 上的真实预算必须是打印出来的事实。
- 测试开始前在**容器内**登记每个 `pg_log` 文件的字节偏移；红时先 `sleep 5` 等落盘，
  再只输出偏移之后的增量（关键行 grep + 尾部 80 行）。偏移法与容器时区无关，也不会
  把历史 ERROR 混进失败窗口。
- 红时另落：`docker inspect` 的 `OOMKilled/RestartCount/Health/ExitCode`、
  `docker events --filter event=die/oom/kill/restart`（必须过滤——不过滤时我自己的
  `exec_create/exec_die` 会淹没输出）、`sudo dmesg` 的 OOM 行（stderr 不再吞掉）、
  容器 stdout 尾部 120 行。
- 全程每 3 秒后台采样 `memory.peak`，绿时也报最大值（"峰值离限额还有多远"是绿
  run 里唯一有判读价值的内存数字）——本节写的是 V21.8 当时的设计，**该采样器已在
  V21.9 被换掉**：它每 3 秒 `docker exec` 进容器，而它采样出来的答案是否掉内存
  假说之后，exec 本身升格成了头号嫌疑（PID 1 就是 postmaster）。
- 演练方式：从 workflow 原文抽出该步骤的 `run` 文本，只把 `cargo test` 换成
  `CHECKPOINT + exit 101`，对本地容器实跑——增量段恰好只输出 CHECKPOINT 那 1745
  字节、无历史行；全部 run 步骤 `bash -n` 通过，workflow YAML 解析通过。

**登记未定论项**：runner 上 purchase 参照侧后端消失的直接成因仍未取到服务端证据。若
下一轮仍红，按此顺序判读：die/oom 事件 → `dmesg` → 增量 pg_log 里的 FATAL/backtrace →
峰值与限额之比。本轮无任何 Rust 代码变更（fmt/clippy/native 门禁不受影响）。

### V21.9 runner 侧内存假设被实测否掉：参照实例是被"静默终止"的

**run 42229183033（head = `8737a68`，golden job 127930349734）**——同一句 panic
（`CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase('20250715', null): [ENV-0020]
connection closed`），但 V21.8 的取证第一次给出了可判读的服务端事实，四条都是实测：

1. **口径在 runner 上真的生效**（fingerprint 步骤打印）：`max_process_memory = 4GB`、
   `shared_buffers = 512MB`、`server_encoding = SQL_ASCII`、cgroup `limit =
   6442450944`。runner 与新口径的开发机完全同参，"runner 推导值漂移"这条排除。
   memory-protect 初始化失败行数 = 2，与本地同（V21.8 第 2 条更正成立：看护是关着的）。
2. **内存假设排除**：测试全程每 3s 采样的容器 cgroup `memory.peak` 最大值
   **670003200 字节（限额的 10%）**；`State.OOMKilled = false`；`docker events` 抓到的是
   `container die … exitCode=0`。OOM 需要"超限额 + 137/被杀"，两者都不是。
3. **失败形态与上一轮不同**：本轮 `RestartCount = 1`、失败后 `health=starting`，
   重启产生的新 `pg_log` 里 `redo record is at 0/2348360; shutdown FALSE` +
   `database system was not properly shut down; automatic recovery in progress`
   ——**整个 PID 1（postmaster）在 CALL 开始后约 0.19 秒退出**，实例走崩溃恢复。
   上一轮（`5d9b419`，无 exec 采样器）容器 stdout 尾部没有第二次引导、没有
   `Skipping initialization`，即容器没重启过，只是后端进程消失。同一句客户端报错，
   两轮是两种服务端形态。
4. **终止是静默的**：失败窗口增量 15994 字节的 `pg_log` 里 FATAL/PANIC/ERROR/
   terminat/signal/kill/crash **一条都没有**，最后一行是 22:05:37.417 的 `[UNDO] LOG`。
   容器内 `ulimit -c = 0`（本轮实测打印）→ 崩溃不可能落 core，也没有 backtrace 可捞。
   因此"下一轮再 grep 一次日志"不会给出答案，必须换问题设定。

**唯一新增的容器侵入项 = 我自己加的 exec 采样器**（本轮把 `max_process_memory` 抬到
4GB 之外，还在测试窗口内每 3 秒 `docker exec` 进容器读一次 cgroup；PID 1 是
postmaster，exec 产生的子进程退出要由它回收）。本地做了直接实验：
`docker exec … cat /sys/fs/cgroup/memory.peak` 每 0.3s 一次（密度是 CI 的 10 倍）叠加
purchase 单跑 → **通过，`RestartCount` 不变**。假说在本地不成立，但本地与 runner 的
内核/docker/调度不同，必须在真环境排除，因为它同时解释"上一轮只死后端、这一轮死整个
实例"这个差异。

**本轮改造**（`.github/workflows/ci.yml` golden job；无 Rust 代码变更）：

- 测试窗口内**零容器侵入**：内存采样改到宿主侧，路径不靠猜目录名——读容器 PID 1 的
  `/proc/<pid>/cgroup`（cgroup v2 唯一一行 `0::<相对路径>`），拼出
  `/sys/fs/cgroup<相对>/memory.peak`；读不到才退到 `docker stats --no-stream`。
  两条路径都不进容器命名空间。目录归属还要过一道交叉校验：该目录的 `memory.max`
  必须等于 Engine API 报的容器限额（`HostConfig.Memory`；`max`/未设限时跳过比对），
  否则说明相对路径解析错位（例如退化成 `/`），读到的会是**宿主全局峰值**——比"没测到"
  更糟，因为它会被当证据判读。`memory.peak` 本身是高位水位，一次读取即全程最大值；
  回退路径读到的是**当前值不是峰值**，两条输出行因此标注不同口径——V21.8 那轮
  "452MiB" 就是把回落后当前值当成峰值判读，标签必须防止同样的误读。
- fingerprint 步骤登记 **PID 1 基线**（`RestartCount` + `StartedAt` + 容器 ID）并打印
  `ulimit -c` 与宿主 `core_pattern`：`restart: unless-stopped` 下实例死后容器原地复活，
  没有基线就判不出"死过"。
- 新增 `Standalone purchase repro` 判定步骤：在全量套件**之前**单独跑同一个
  purchase 用例，比较前后 PID 1 基线，直接区分两种解释——判定B（单跑就死 = 这条存过
  独立可为）／判定A（单跑不死 = 依赖套件先前 11 分钟的建库删库状态）。用例自身的断言
  失败不在这一步门禁（同一判定权留给全量套件），但判定B 必须红——否则套件会在"重启后的
  干净实例"上转绿，把一次可独立复现的参照侧崩溃掩成一次通过的合并；绿的含义必须是"没有
  任何一处观察到崩溃"。若单跑真的打死了实例，会等 `healthy` 再交接，避免诊断步骤本身
  变成新的失败源。
- 红时新增 `crash-recovery 判定` 组：`database system was not properly shut down`
  出现文件数（>0 = 异常终止后恢复）与 `received …shutdown request` 行数
  （>0 = 收到过关停信号，属正常路径）。这两个计数把"被信号停掉"与"猝死"分开。
- `docker events/logs` 的 `--since` 一律改 RFC3339：纯数字在不同 docker 版本里按秒
  或按纳秒解释，窗口口径不能靠猜。

**演练**：workflow YAML 解析通过，13 个 `run` 步骤逐个 `bash -n` 通过；新构造对本地容器
逐条实跑——`pid1_baseline` 打印正常、`ulimit -c` = 0（**更正**：这条读的是 `docker exec`
新起的 shell，不是 PID 1 的限额，判"能否落 core"必须看宿主 `/proc/<pid>/limits`，见
V21.10）、崩溃恢复文件数 = 0（本地无崩溃，
符合预期）、`received …shutdown request` 行数 = 3（initdb 临时实例的两次停服 + 最终
引导，属正常噪声，判读时看的是"失败之后有没有新增"而不是绝对值）、`docker logs
--since <RFC3339>` 与 `docker events --since/--until <RFC3339>` 均无参数错误。

**下一轮判读顺序**：① `Standalone` 步骤给判定 A 还是 B；② 全量套件里还有没有
`container die`（若本轮绿 = exec 采样器即因，永久去掉它）；③ 有 die 但 `pg_log` 无
FATAL = 静默终止，`ulimit -c = 0` 已排除 core，下一步只能改成"在 runner 上给容器放开
core 上限 + 指定可落盘路径"或把 purchase 参照侧语料二分；④ 峰值/限额之比作为内存的
最后确认。仍未定论项：runner 上 purchase 参照侧实例终止的直接成因。
→ ①②已由 V21.10 回答（判定B；exec 采样器嫌疑排除），③ 的"`ulimit -c = 0` 已排除
core"是误测，core 在 runner 上可落且进 systemd-coredump。

### V21.10 判定B：干净实例单跑 purchase 即致死，exec 采样器嫌疑排除

runner 两轮（run 42236691856 = `34bbd48`、run 42237764112 = `b719696`，golden job）
在**测试窗口内零容器侵入**（内存改宿主侧读 `memory.peak`）+ 单跑判定的条件下，结果一致：

| 事实 | 实测值 | 判读 |
|---|---|---|
| `SHOW max_process_memory` / `shared_buffers` | 4GB / 512MB | compose 的 `OTHER_PG_CONF` 在 runner 空目录首次 initdb 确实生效（不是"假设生效"） |
| 容器 cgroup `memory.max` | 6442450944 字节 | 限额 6G 原样落地，未被 runner 抹平 |
| `server_encoding` / `lc_collate` / `lc_ctype` | SQL_ASCII / C / C | 与本地同口径 → **编码漂移不是**本地绿/runner 红的差异来源 |
| memory-protect 初始化失败行数 | 2 | 实例级内存看护关闭（与 compose 注释一致），超限不会被拦成可读 SQL 错误 |
| `Standalone purchase repro` | rc=101，`[ENV-0020] gaussdb: connection closed`，用例自身 finished in 2.46s / 1.85s | 崩溃点就是那一句 `CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase(...)` |
| PID1 `RestartCount` | 0 → 1（两轮一致） | **判定B**：不依赖套件先前 11 分钟的建库/删库状态，单跑即致死 |
| `'not properly shut down'` 文件数 | 0 | 与 V21.8/21.9 记录的"重启后报崩溃恢复"不同形态：本轮窗口内没走崩溃恢复 → 更像 postmaster **自行正常退出**（exit 0）而非段错误后恢复 |
| 重启到 `healthy` | < 7s（`交接给全量套件时 health=healthy`） | `unless-stopped` 把死亡完全掩掉，只有 `RestartCount` 能证明死过 |
| `StartedAt` | 0→1 之后**未变** | 不能当"死过"的判据；取证脚本的 State 组同时打印 started/finished/restart_count/exit_code，把口径固化下来。**V21.11 更正**：这条是 inspect 时序造成的误读——同一个窗口里 baseline `started=15:22:46` 与事后 `started=15:33:27` 明确不同，`StartedAt` 会变。固化下来的口径（同时打印四个字段）不变，但"未变"这个观察本身作废 |

**误测更正（重要）**：V21.9 里"`ulimit -c` = 0 → 崩溃不可能落 core"这条读的是
`docker exec` 新起 shell 的限额。runner 上实测 `ulimit -c = unlimited`，宿主
`core_pattern = |/usr/lib/systemd/systemd-coredump %P %u %g %s %t %c %h %d %F`。
也就是说：**崩溃是能拿到 core 和栈的**，只是 core 不落目录而进 `coredumpctl`；
判 PID 1 的落 core 能力一律读宿主 `/proc/<pid>/limits`（内核维护，进程改不了）。

**本轮改造**（`scripts/ci/gauss_incident_evidence.sh` 新增 + `.github/workflows/ci.yml`；
无 Rust 变更）：

- 取证收进一个脚本、九组：容器/PID1 状态、PID1 权威 rlimit + `core_pattern`、
  生命周期事件（die 的 exitCode/信号）、宿主 `dmesg`（segfault / OOM / traps）、
  `coredumpctl`（含 `Stack trace of thread` 栈顶）、崩溃恢复判定、pg_log 增量、
  内存口径（peak/current/max + 容器内 postgresql.conf 生效值 + `free -m`）、
  容器 stdout。`Standalone` 与全量套件红时都调同一个脚本——上一轮两处各写一份，
  套件那份只 grep 了 oom、压根没查 coredumpctl，这就是口径漂移的代价。
- 脚本全程 `set +e`、末尾 `exit 0`：取证绝不反过来成为新的红因。
- 宿主 cgroup 目录解析加**归属校验**（`memory.max` 必须等于 Engine API 的
  `HostConfig.Memory`；`max`/未设限跳过），不一致即回退 `docker stats`——错位目录
  读到的宿主全局峰值会被当成容器证据，比读不到更危险。
- 全量套件删掉后台内存采样器：`memory.peak` 单调不减，测试前后各读一次即覆盖全程，
  采样器每 5 秒读的是同一个数，还平白多开一个进程。
- `Standalone` 在 `rc ≠ 0` **或** PID1 变化时取证；判定B 仍然门禁（绿的含义必须是
  "没有任何一处观察到崩溃"）。

**演练**：YAML 解析通过，13 个 `run` 步骤逐个 `bash -n` 通过；本地用 `cargo` 桩
（固定 `exit 101`）端到端演练两个步骤骨架——套件步骤按被测 rc 返回 101（门禁行为
正确），`Standalone` 在 `pre == post` 时返回 0（判定A 不门禁断言失败），且 rc≠0 分支
确实调用了取证脚本、九组全部打印；脚本对本地健康容器实跑返回 0，Engine API 限额
6442450944、`received …shutdown request` = 3、崩溃恢复 = 0 与上一轮一致。

**下一轮判读顺序**（脚本输出即按此排列）：① `dmesg` 有 `gaussdb[…]: segfault` →
信号型崩溃实锤，配 `coredumpctl` 栈顶可直接定位 openGauss 内部函数；② `coredumpctl`
有 core → 栈顶即崩位；无 core 但 die 的 exitCode ≠ 0 → 看信号；③ exitCode = 0 且
`received …shutdown request` 无新增 → postmaster 是**主动正常退出**，方向转为"什么让
它自己退出"而不是段错误；④ 内存仍只作最后排除项（V21.9 已否掉一次）。

**仍未定论**：runner（linux/amd64）上 purchase 参照侧实例终止的直接成因。本地
（linux/arm64，同镜像 tag、同 4GB/512MB/6G 口径、同 SQL_ASCII）全量 gauss 套件为绿，
架构是当前最大的未排除变量；本机 registry 只返回 arm64 摘要（`docker pull --platform
linux/amd64` 报"up to date"并解析到 arm64 镜像），amd64 复现只能在 runner 做——这正是
取证脚本必须自带 dmesg/coredumpctl 的原因。**V21.12 更正**：这一段的"同镜像 tag ⇒
同口径"前提被 CI 否掉——arm64 runner 上同一个 tag 拿到的是 amd64 用户态、容器根本
起不来，而本机那份 arm64 来自 registry 镜像站，与 Hub 那份不是同一个构建。本节其余
实测值（4GB/512MB/6G、SQL_ASCII、单跑即致死、RestartCount 0→1）不受影响。

### V21.11 取证脚本自己有一处缺陷被实测抓到；判读走到"postmaster 以 0 退出"

run 42242415215（head = `66f148b`，golden job）是 V21.10 改造后的第一轮实跑，九组证据
全部落进日志。以下每条都是那一轮的服务端事实，不是推断。

| 证据组 | 实测值 | 判读 |
|---|---|---|
| 单跑结果 | rc=101；`[ENV-0020] gaussdb: connection closed`；用例自身 `finished in 0.43s` | 死亡就在 `CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase('20250715', null)` 这一次调用里 |
| PID1 基线 → 事后 | `restart_count` 0→1；容器 id 不变（`537870f7…`）；`started` 15:22:46.315→15:33:27.799；`finished=15:33:25.134`；`exit_code=0`；`OOMKilled=false` | 15:33:25.134 死亡，2.66s 后被 `unless-stopped` 拉起（同容器新进程）。**V21.10 那条"StartedAt 未变"作废**，已就地更正 |
| PID1 rlimit（宿主 `/proc/<pid>/limits`） | `Max core file size unlimited`；`Max address space unlimited`；`Max locked memory 8388608` | 崩溃**有条件**落 core；`core_pattern` 是 systemd-coredump 管道，所以 core 只可能出现在 `coredumpctl` |
| 宿主 `dmesg` | 空 | 没有 `gaussdb[…]: segfault`、没有 `Killed process`、没有 traps |
| `coredumpctl list --since` | `No coredumps found.` | 没有信号型崩溃；**不是**"没落盘"，是根本没产生 core |
| 崩溃恢复判定 | `'not properly shut down'` 文件数 0→1（本轮新增）；`'received … shutdown request'` = 1 | 那条 shutdown request 是 initdb 期 entrypoint `gs_ctl -m fast -w stop` 模板服务器留下的**历史记录**；死亡窗口的 pg_log 增量里 `terminat/signal/kill` 零命中 ⇒ 死亡时**没有**收到关停信号 |
| pg_log 增量 | 崩溃前文件 14118 字节；FATAL/PANIC/ERROR/out of memory/backtrace **一条没有**；最后一行 23:33:24.710（=15:33:24.710Z，死亡前 0.42s）`[BACKEND] LOG: Delete object 2615/16387/0` | 见下面第 2 点 |
| 重启后新日志 | `database system was interrupted; last known up at 23:33:05`；`redo record is at 0/2349188; shutdown FALSE`；`automatic recovery in progress` | 整个实例非正常终止后走 WAL 恢复，与"后端进程消失"不是一回事 |
| 内存口径 | cgroup `peak=current=495935488`（新实例，限额的 7.7%）、`max=6442450944`、Engine API 同值；容器内 `postgresql.conf` 生效 `max_process_memory=4GB` + `shared_buffers=512MB` | 内存第四次被同一份数据排除（V21.8/21.9/21.10/本轮） |

**1. 判读顺序走到 ③**（V21.10 立的那四条）。exitCode=0 + 无 segfault + 无 core +
服务端不记任何错误 + 无新增关停请求 ⇒ 不是 OOM、不是信号型崩溃、不是收到 SIGTERM 的
正常关停。剩下的形态是 **postmaster 自己走了一条不记账号的退出路径**，问题从"谁杀了它"
换成"什么让它自己退出去"。

**2. `Delete object 2615/16387/0` 是最后一条被记录的服务端动作，不是罪名**：`2615` =
`pg_namespace` 的目录 OID（本地 V21.9 已登记），`16387` 是用户范围 OID，对应 harness
`reset_reference` 自己的 `DROP SCHEMA gaussdb CASCADE`。语料里没有 DDL、没有动态 SQL，
这条 DROP 之后到死亡之间只剩那一句存过执行；把它当嫌疑犯是日志截断的假象。

**3. 本轮最该记住的是取证工具自己的缺陷**：生命周期事件那一组**从来没输出过**。

- 本地（docker CLI 29.9.2 / API 1.56）复现同一命令得到：
  `Error parsing format: template: :1:34: executing "" at <.Attributes>: can't evaluate
  field Attributes in type *events.Message` —— 整条 `docker events` 失败。旧写法把
  stderr 丢进 `/dev/null`，于是打印为空，而"窗口内确实死过容器"（`restart_count` 0→1、
  `FinishedAt` 就是死亡时刻）与"没有事件"长得一模一样。**空证据被当成证据判读**，
  比读不到更坏。
- 另一条实测：Docker Desktop 上 `--since/--until` 的历史回放把我给的无时区 UTC 串按
  本机 +08:00 解释，同一窗口事后查询返回 0 行、而实时订阅能拿到 264 行 ⇒
  **事后查询不能作为生命周期事件的唯一来源**。

处置（`scripts/ci/gauss_incident_evidence.sh` + `.github/workflows/ci.yml`，无 Rust 变更）：

- CI 步骤在测试**之前**起一路 `docker events` 实时订阅写进文件，取证脚本读它。这是宿主侧
  对 Engine API 的只读订阅，不进容器命名空间、不 `docker exec`，与 V21.9 排除的 exec
  采样器不是一类侵入。`Standalone` 与全量套件各一路（`/tmp/ci_events_standalone.txt`、
  `/tmp/ci_events_suite.txt`），作为脚本第 5 个参数传入。
- 事件命令一律**不带 `--format`**：默认输出自带完整 Actor attributes（含 `exitCode`），
  没有模板键的版本兼容问题。事后查询降级为兜底，且必须打印返回行数与 stderr。
- 每个"空"都要有**分母**：实时流总行数、按 action 的计数、`dmesg` 总行数、事后查询返回
  行数。没有分母的空，判读无从下手——这就是本轮的教训本身。
- 健康检查每 5s 产生 `exec_create/exec_start/exec_die` 三类事件，数量远超生命周期事件
  （本机实测：4 分钟窗口 264 行里 252 行是这三类）。直接 `head -30` 会把真正的 `die`
  挤出画面。现在先剔 `exec_*`（注意 `exec_die` 是**子进程**退出，不是容器 `die`），
  再看生命周期，最后给"剔掉 exec 后的全量"兜底。
- 事件输出经 sed 脱敏（`*PASSWORD*/*SECRET*/*TOKEN*=` 的取值替换）。实测本机 compose
  健康检查进事件流的是 `-W $GS_PASSWORD` 字面量、未展开，且 CI 口令另有 `::add-mask::`
  按值兜底；但取证脚本不该把安全性建立在"恰好没展开"这条观察上。

**4. 平台处置：golden job 换 `ubuntu-24.04-arm`**（公开仓库免费的 arm64 hosted runner）。
理由就是上面那组数据的合取：同一镜像 tag、同一 4GB/512MB/6G 口径、同一 SQL_ASCII，
amd64（runner）3/3 轮单跑即致死，arm64（开发机）全量 gauss 套件为绿——架构是当前唯一被
实测分离出来的变量。

> **V21.12 作废这一条的前提**：arm64 hosted runner 实测起不了这个镜像（Hub 的 tag
> 在该平台解析到 amd64 用户态，`gosu: cannot execute binary file: Exec format error`），
> 而本机那份 arm64 是经 registry 镜像站拿到的**另一个构建**。"同一镜像 tag ⇒ 同口径"
> 不成立，因此"架构是唯一分离出来的变量"这条推断被撤回。本节其余取证改造（实时事件流、
> 分母、`exec_*` 过滤、sed 脱敏）全部有效、未被撤回。

**改的是参照产品的运行平台，不是断言**：用例、场景、期望基线、`spin==native`/
`spin==baseline` 三重比对、判定 B 的门禁逻辑一条未减；换平台后若 arm64 也观察到 PID1
更替，同一份取证脚本会直接把证据落全。边界写进 workflow 注释，防止被读成"换了平台就
没事了"——**amd64 参照实例的猝死仍是未决问题**，登记在本节，不因换 runner 关闭。
（本节 §4 的平台方案由 V21.12 取代；"改的是运行平台/被测面划分，不是断言"这条边界仍然
有效，V21.12 沿用。）

**仍未定论**：① amd64 上 postmaster 以 0 退出的内部成因（本机 registry 只返回 arm64
摘要，见 V21.10 末段，只能等平台侧证据或 openGauss 源码级复现）；② 语料里哪一段 SQL
触发。②若要继续推进，可行路径是在 amd64 上把这条存过的过程体按段二分（游标循环 /
`<<error_row>>` + `GOTO` / `DELETE…; COMMIT;` 各自单跑）做**参照侧实验**——它不改被测
断言，也不改 golden 期望。

### V21.12 换 arm64 这条路被 CI 实测堵死；V21.10/21.11 的"同 tag ⇒ 同口径"前提作废

V21.11 §4 立的平台处置（golden job → `ubuntu-24.04-arm`）在同一天的两轮 CI 里被直接
否掉。以下每条都是 runner 日志的服务端事实。

| 事实（run 42249019674 = `fb85813`、run 42249235572 = `9002338`，两轮同结论） | 实测值 | 判读 |
|---|---|---|
| 平台自检（本轮新加的三行 echo） | `runner 架构: aarch64`、`Docker version 28.0.4`、`Docker Compose v2.38.2` | 标签与内核一致：runner **确实**是 arm64，问题不在标签 |
| 镜像拉取 | 9 层全部 `Pull complete`、`gaussdb Pulled`，无 `does not match the platform` 警告 | 拉取"成功"——成功的是 amd64 那份构建 |
| 容器启动 | `Container spinduck-gaussdb Started`（16:07:01） | 容器起了，但 PID 1 立刻失败 |
| 容器 stdout（16:07→16:11:57，5 分钟轮询窗口内） | `/var/lib/opengauss/entrypoint.sh: line 356: /usr/local/bin/gosu: cannot execute binary file: Exec format error` + 同行 `Success`，**14 次**（`unless-stopped` 每次重拉都再失败一次） | entrypoint.sh 第 356 行就是 `exec gosu opengauss …`（V21.9 已定位）。`gosu` 是 amd64 ELF，在 arm64 内核上 exec 失败 ⇒ **参照实例不存在**，不是慢、不是配置、不是口令 |
| 步骤结果 | 60×5s 轮询从未 `healthy`，兜底打印 `docker logs \| tail -50` 后 `exit 1` | 红在"引导参照实例"这一步，测试一条没跑 |
| 同 head 其余四个 job | format / clippy / test / audit **全绿** | 受影响面只有需要参照实例的 golden job |

**1. 本机 arm64 那份不是 Hub 的同一件东西**。本机 `docker info` 的 Registry Mirrors 是
`docker.m.daocloud.io` 等三个镜像站；本机镜像 `arch=arm64 os=linux
id=sha256:f8679ef3…`、容器内 `gaussdb` 是 `ELF 64-bit LSB pie executable, ARM aarch64`
（V21.10 记下），而 `docker pull --platform linux/amd64` 报"up to date"并解析到同一份
arm64 镜像——本机拿不到 Hub 的 amd64 构建做对照。合起来：**同一个 tag 在 CI 与开发机上
对应两个不同构建**（不同层集、不同 CPU、极可能不同编译参数）。

于是 V21.10 末段与 V21.11 §4 的推理链断在第一环。仍然成立的是事实本身：Hub amd64 构建
3/3 轮在过程 CALL 中致死；镜像站 arm64 构建全量 gauss 套件为绿。但这**是两份不同
openGauss 构建的行为差**，不是"同一份在两种 CPU 上的行为差"，"架构是唯一被实测分离出来的
变量"这句话作废。

**2. 处置：CI 的参照侧可执行面收敛回 amd64，黄金层的被测面按 2026-11-06 口径收缩**。

- `runs-on` 回 `ubuntu-latest`（amd64）。注释里写明这条路已被实测堵死，防止下一轮再试。
- `case_corpus_purchase` 的黄金层被比对象从 `pkg_split_trade_step3_sh.prc_trd_deal_purchase`
  改为调用链上的语料函数 `fnc_get_purchase_js_days`（`kind = "function"`，5 个场景）。
  它同源覆盖那条过程里最硬的几种形态：`SELECT INTO` + `EXCEPTION WHEN OTHERS` 归零分支、
  `to_date(...) + varchar2` 隐式转换、PL 内建 `substr`、日期算术与 `INTERVALTONUM`。
- 过程本体走**进程内内核**断言（`cargo test --features native --test cases_native`，
  `native_asserts_corpus_purchase`）：4 条 `[[case.assert]]`（`out` = `p_o_succeed`、
  `rows` = `dat_trd_repurchase` 产物行集、`rows` = 合计 `[2, 300, 30000, 6]`、
  `error` = `BIZ-0151` 缺实参契约）**原文保留**，加上 `tests/corpus_sql_forms.rs` 按
  `config/corpus_forms.toml` 的逐形态断言（147 个语料文件的构造清单，脱敏入库）。零外部
  环境，符合"测试必须能在 Qoder 环境里执行"的口径。

| 层 | 命令 | 参照实例 | 现在测什么 |
|---|---|---|---|
| 黄金（`--features gauss`） | `cargo test --features gauss` | 需要（缺容器必红） | 语料级**函数**逐场景 `spin==native` + `spin==baseline` |
| 原生断言（`--features native`） | `cargo test --features native --test cases_native` | 不需要 | 考题**过程**的多维/多粒度断言集（4 维 × 逐值） |
| 形态覆盖（`--features native`） | `cargo test --features native --test corpus_sql_forms` | 不需要 | 语料里每种 SQL/PL 语句形态的改写产物 + 执行结果 |

**3. 黄金层新基线的采集口径**：`expected.json` 的 5 个 `value`（`90 / 180 / 0 / 0 / 90`）
全部来自参照实例实跑。采集用的是一次性本地脚本（不入库），它逐轮跑
`cargo test --features gauss --test golden_corpus_purchase`，**只在同一轮里
`spin == native` 与 `spin == baseline` 双双通过时才把该轮实测值写进基线**；一旦失败信息
指向 `tests/common/mod.rs:179`（即 spin≠native 的真实分歧）立即停机不写，所以不存在
"基线跟着被测实现走"。`expected.json` 的 `why` 串与 `purchase.case.toml` 逐字相同（便于
对照，非门禁）；场景数与基线数不一致会在 `baseline[i]` 越界时当场红。本用例是只读函数、
不声明 `[case.output]`，所以 `check_row_counts`（防"输出表未物化导致漏比"）对它不适用，
比对完全由 5 个场景值承担——这一点也在 `purchase.case.toml` 的注释里写明。

**边界（防止被读成"躲过去了"）**：没有一条断言被删、没有一条期望值被放宽，
`spin==native` 与 `spin==baseline` 双重比对、以及"缺容器必红"（`run_case` 子线程里
`run_case_inner` panic 并打印起容器的命令，不打印 SKIP）三条门禁原样未动。顺手修掉
三处假文档：`tests/golden_*.rs` 的头注释仍写着"未设 `SPINDUCK_OG_HOST` 时打印 SKIP
并返回"，那句话自 `1e43fd7`（审核 Blocker 修复里的"黄金 fail-fast"）起就是假的。

**改的是参照侧被测面的划分**：如实说，参照实例上少执行了一条过程（`prc_trd_deal_purchase`
不再被 `CALL`），被测面本身没有缩小——过程的可观察结果由进程内内核逐值断言接管，SQL
形态由 `corpus_sql_forms` 接管，两者都在常驻门禁里跑。这套分工同时满足两条既有约束：
2026-11-06 的语料口径指示（语料用例不执行业务函数，SQL 原样放置 + 最全面的形态断言）
与 AGENTS §4.1（黄金层禁止"跳过即绿"）。

**仍未定论**（登记为参照产品/平台问题，不再作为门禁阻塞项）：① Hub amd64 构建的
postmaster 在过程 CALL 中以 0 自行退出的内部成因（V21.9–V21.11 四组排除：非 OOM、非
信号崩溃、非套件状态、非取证侵入）；② 语料里哪一段 SQL 触发；③ `opengauss/opengauss:5.0.0`
是否存在官方 arm64 构建（本机拿到的是镜像站产物，Hub 在 arm64 runner 上给的是 amd64
用户态，两种解释——tag 无 arm64 manifest，或 runner 的解析策略——都无法从本机区分）。
若将来拿到官方修复或源码级复现，恢复过程级黄金比对的入口只有 `purchase.case.toml` 的
`kind`/`routine` 与 `cases/baselines/expected.json`，不需要动引擎代码。

**处置首轮 CI 实测（run 42254187059，head `138aa7f`，五个 job 全绿）**——收缩后的黄金层
在 Hub amd64 构建上就是能跑通的，以下都是那一轮的日志事实：

| 项 | 实测值 |
|---|---|
| 平台 | `runner 架构: x86_64`、`Docker version 29.4.2`、`Docker Compose v5.1.3` |
| 容器引导 | 16:38:04 起 → 16:38:48 `A-mode ready`（44s，一次成功，无 Exec format error） |
| 口径指纹 | `max_process_memory=4GB`、`shared_buffers=512MB`、`server_encoding=SQL_ASCII`、cgroup `memory.max=6442450944`、memory-protect 失败行数 2（与 V21.10 同形态） |
| PID1 基线 | `restart_count=0 started=16:38:22.575…`；套件结束后仍无更替 ⇒ 全程零崩溃，取证脚本未被调用 |
| 套件 | `cargo test --features gauss` 298 passed / 0 failed / **0 ignored**；`golden_corpus_purchase … ok`（0.80s，5 场景）与另三条黄金用例同绿 |
| 内存 | `memory.peak` 668438528 → 672178176 字节（限额的 10.4%） |
| 常驻四 job | format / clippy / `--features native`（292 passed，0 ignored）/ audit 全绿 |

同一份 amd64 构建、同一份 compose 口径、同一个 runner 镜像（红绿两轮 golden job 都是
`ubuntu26/20261101.186`，故 docker/compose 版本不是混淆变量）：此前 3/3 轮红在过程
`CALL`，本轮绿——这组对照把"崩溃主语就是那条过程"钉得更死了：被测面上唯一被拿掉的
变量，也是唯一改变结果的变量。


## V22. `verify` 的 SKIP 理由分两档；README 的 verify 例子换成 clone 可复现命令

PR#1 合并后自审扫出两处"输出/文档比事实宽或错"的地方，都在 `verify` 这一面：

1. **README §5 的例子指向不入库的路径。** 那条 `spinduck verify ../golden/exam_split_trade`
   在 clone 里必失败——该目录被 `.git/info/exclude` 挡在库外，clone 后不存在；`verify` 对缺失
   项目目录的实测行为是 `[ENV-0030] 未在 … 及其上级目录找到项目声明（spinduck.toml 或
   conf/spinduck.toml；verify 以项目声明为起点）`（本机用错相对路径时撞到同一条）。README 的
   命令例子必须能在拿到的仓库里跑通。
2. **SKIP 的理由与断言层的存在直接矛盾。** 对 `golden/case_corpus_purchase` 实测：
   `SKIP Scenario - — 无 *.case.toml 声明该入口，可执行性未验证`。而同一份
   `purchase.case.toml` 里 `kind = "function"` 的 `[case]` 只声明函数场景，指向该入口过程的
   恰是同文件的两条 `[[case.assert]]`（`out p_o_succeed` / `error BIZ-0151`），它们由
   `cargo test --features native --test cases_native` 真跑并逐值断言——V21.12 第 2 点刚立的
   分工。留着"未验证"这句话，等于让报告否认自己刚登记的执行面。

### TDD 循环

| 步 | 内容 | 实测 |
|---|---|---|
| Red | `tests/verify_chain.rs::should_credit_assert_layer_when_no_scenario_declares_the_entry`：一个入口 + 一份场景不指向它、断言指向它的用例；断言 Skip 理由含条数、含用例名、不含"可执行性未验证" | 首跑红于 `verify_chain.rs:224`，detail 实为 `无 *.case.toml 声明该入口，可执行性未验证`（断言失败，非编译失败） |
| Green | `src/project/case.rs`：`CaseAssert::targets_routine()`；`src/verify/run.rs`：`scenario_skip_check()` 出两档理由 | `cargo test --features native --test verify_chain` → 9 passed / 0 failed / 0 ignored |
| 口径钉住 | 同一 fixture 再加 1 条 `kind = "rows"` 干扰断言（`target` 是 SQL） | 理由仍写"2 条"，证明计数只认以例程为 target 的 `out`/`call`/`error`，`rows` 不计入——SQL 断言不能证明例程可执行 |
| 在库复跑 | `./target/debug/spinduck verify ../golden/case_corpus_purchase` | `entries=1 checks=11 failed=0 skipped=1`，SKIP 行改为 `2 条 [[case.assert]] 已由用例 corpus_purchase 覆盖该例程，本命令不执行断言（断言层见 cases_native）` |

**判定没动，只动了理由**：状态仍是 `Skip`、`object` 仍是 `-`、`failed` 计数与退出码口径不变
（只有 `FAIL` 计非零）。把 `verify` 变成执行断言的是另一件事（要拖进数据集物化与部署面），
本轮不做。

### 门禁

`cargo fmt --all` / `cargo clippy --all-features -- -D warnings` 干净；
`cargo test --features native` = **293 passed / 0 failed / 0 ignored**（292 + 本轮新增 1 条）。
合并后 main 实测（run `42317165064`，head `741d26c`，五 job 全绿）：`test` job 293 passed、
`golden` job **299 passed / 0 failed / 0 ignored**。

> **更正本来自审写错的一句**：原记"黄金层 `--features gauss` 不受影响"。这句在**计数**上是错的
> ——新增的 `verify_chain` 测试同样计入 `--features gauss` 的套件总数，实测 298 → **299**。成立
> 的部分只是"黄金**比对项**没有新增，`verify` 不被黄金层消费"。已按实测口径改写，避免下一轮
> 把"不受影响"读成"总数没变"。

### 边界（不声称做到的）

- 匹配是归一小写全名比较。`[[case.assert]]` 若以短名指向限定名入口则**不计入**，宁可少认一条
  覆盖，也不把别的包的同名过程算进来；代价是这种写法下理由仍显示"未验证"。在库 4 个用例的
  入口级断言实测均为限定名，计数与用例文本一致。
- 一文件多 `[[case.assert]]` 的用例数与断言数分别计数：理由里的用例名去重，条数不去重。
- 本轮自审的二手结论必须自己复核：另一路评审另报两处待改（`golden/README.md` 里"3 参过程 +
  `outs=2`"的例子、`case_corpus_purchase/reports/PROVENANCE.md` 的陈旧路径说明），经
  `git grep` + `git ls-files` 复核**在跟踪内容里都不存在**（前者无匹配、后者文件不在库内），
  故未采纳，也不写进改动清单。

## V23. 「已知限制」两条口径订正（README / V8 / V9 / architecture §8.3）

**结论先说**：本轮复核 README「已知限制」的两条声明，一条**部分成立且有一条静默
缝隙**，一条**已被后续实现推翻**；顺带发现 V9 语料体检数漂移。只改文档，未动代码。

### 探针口径

编译期不触碰内核，V9 同口径。探针不落库（临时目录），命令与期望输出直接写在这里，
clone 后可逐条复现——本节六条只依赖仓库内可见内容；§3 的语料体检另需 `lib/fastaas`
子模块，见边界：

```bash
cd spinduck
cargo build --bin spinduck --features cli

mkdir -p /tmp/sd_v23 && cd /tmp/sd_v23
# ① 集合元素读取（本轮唯一"不静默"缺口）
cat > read.sql <<'EOF'
CREATE OR REPLACE PROCEDURE prc_read IS
  DECLARE
    TYPE t_tab IS TABLE OF NUMBER INDEX BY BINARY_INTEGER;
    t_res t_tab;
    v_n NUMBER;
  BEGIN
    v_n := 1;
    v_n := t_res(1) + 1;
  END;
EOF
# ② 集合元素赋值 ③ RETURN NEXT ④ PIPE ROW ⑤ 带参游标 OPEN ⑥ 仅声明带参游标
cat > assign.sql <<'EOF'
CREATE OR REPLACE PROCEDURE prc_coll IS
  DECLARE
    TYPE t_tab IS TABLE OF NUMBER INDEX BY BINARY_INTEGER;
    t_res t_tab;
  BEGIN
    t_res(1) := 100;
  END;
EOF
cat > return_next.sql <<'EOF'
CREATE OR REPLACE FUNCTION fnc_set(p IN NUMBER) RETURN TABLE OF NUMBER IS
  BEGIN
    RETURN NEXT p;
  END;
EOF
cat > pipe_row.sql <<'EOF'
CREATE OR REPLACE FUNCTION fnc_pipe(p IN NUMBER) RETURN TABLE OF NUMBER IS
  BEGIN
    PIPE ROW(p);
  END;
EOF
cat > open_args.sql <<'EOF'
CREATE OR REPLACE PROCEDURE prc_open(p IN NUMBER) IS
  DECLARE
    CURSOR c1 FOR SELECT 1;
  BEGIN
    OPEN c1(p);
    CLOSE c1;
  END;
EOF
cat > decl_only.sql <<'EOF'
CREATE OR REPLACE PROCEDURE prc_declonly IS
  DECLARE
    CURSOR c1 (a NUMBER) FOR SELECT 1;
  BEGIN
    NULL;
  END;
EOF

cd -
./target/debug/spinduck compile /tmp/sd_v23      # 期望见下表"实测"列
./target/debug/spinduck explain --file /tmp/sd_v23/read.sql prc_read
./target/debug/spinduck call --file /tmp/sd_v23/read.sql prc_read
```

实测输出（2026-11-07，DuckDB 1.5.5 / `=1.10505.0`）：`compile` 六文件
`ok=2 failed=4`——`read.sql` 与 `decl_only.sql` 为 **OK**（即缺口），其余四条分别是
`assign.sql`/`pipe_row.sql`/`return_next.sql` 的 `ENG-0022`（后两条
`…: ReturnNext{…}` / `…: PipeRow{…}`，赋值条 `…: Sql(":= 100")`）与
`open_args.sql` 的 `ENG-0048 unsupported cursor reference: FunctionCall{ name: c1, … }`；
`explain read.sql prc_read` 出 4 条 IR，末条 `assign v_n := (t_res(1) + 1)`；
`call` 得 `[KERNEL-0010] Catalog Error: Scalar Function with name t_res does not exist!`
（下发文本 `SELECT t_res($p0)`），退出码 1。`FOR r IN c1(p)` 形态另得
`ENG-0059 FOR rec IN cursor(args) is not supported in v1`。

### 1. 四类构造的「编译期显式拒绝」覆盖面

| 构造（探针） | 实测 | 判定 |
|---|---|---|
| `RETURN NEXT i;` | `ENG-0022 statement not yet supported in M2 compiler: ReturnNext{…}` | 与声明一致（兜底支 `ir/stmt.rs`，编译器只列 25 个 `PlStatement` 变体，`ReturnNext`/`PipeRow`/`ReturnQuery`/`ForAll` 落兜底） |
| `PIPE ROW(i);` | 同上，`…: PipeRow{…}` | 一致 |
| 集合元素**赋值** `t_res(1) := 100;` | 同上，`…: Sql(":= 100")` | 一致（解析器把整条赋值降级成 `Sql` 片段，编译器兜底拒绝，非静默） |
| 带参游标 `OPEN c1(p)` | `ENG-0048 unsupported cursor reference: FunctionCall{…}` | 一致：拒绝发生在 `OPEN` **调用点**（`ir/stmt.rs` 把游标引用限定为 `PlVariable`/`ColumnRef`，`OPEN c(args)` 解析成 `FunctionCall` 落错误支） |
| `FOR r IN c1(p)` | `ENG-0059 FOR rec IN cursor(args) is not supported in v1` | 一致 |
| **集合元素读取** `v_n := t_res(1) + 1;` | `compile` **OK**（routines=1）；`explain` 出 `assign v_n := (t_res(1) + 1)`；`call` 运行期 `[KERNEL-0010] Catalog Error: Scalar Function with name t_res does not exist!` | **不一致：静默错译** —— `TYPE … IS TABLE OF` 声明被 `ir/entry.rs` 的"不影响帧布局"支跳过，`t(i)` 在表达式侧按标量函数调用编译 |
| 带参游标**仅声明不使用** `CURSOR c1 (a NUMBER) FOR …` + `NULL;` | `compile` **OK** | 拒绝只发生在调用点；声明本身不报错（README 原句未区分，已在能力矩阵游标行补注） |

### 2. SQL 上下文过程级函数调用

- `should_not_register_macro_for_function_with_local_logic`（`src/engine.rs`）：
  含局部变量的函数不进 `duckdb_functions()`（计数 0），但 `eng.call("fnc_local", …)`
  仍返回 2 —— 宏路径③ 只覆盖纯表达式函数这一半成立。
- `should_keep_procedure_out_of_sql_macro_namespace`（同文件）：过程不进内核函数命名空间。
- `src/runtime/interp/kernel_tests.rs`（注释即"集合形态：`UPDATE ... SET v = length(fnc(k)) * 10`"）：
  `fnc_tag` 含局部变量 + `SELECT INTO` + `RETURN`（过程级），在 `UPDATE SET` 与
  `INSERT … VALUES` 两处 **SQL 上下文**被成功调用，断言行集
  `[a,10] [a,100] [b,2]` 通过 —— **推翻**"过程级函数只能 PL 上下文调用、SQL 内调用
  一律得 `KERNEL-0010`"。机制是路径②（`sql/sqlfn/`，V15/V21.3 已落地）。

### 3. 语料体检复跑

仓库根逐字命令（本轮实跑，非引用 V9 旧数）：

```bash
./spinduck/target/debug/spinduck compile "lib/fastaas/exam/清算拆分优化考题/基线代码"
# summary: files=136 ok=136 failed=0 routines=54      （与 V9 一致）
./spinduck/target/debug/spinduck compile lib/fastaas/exam
# summary: files=187 ok=182 failed=5 routines=72
# reason ENG-0022 x3 / ENG-0048 x1 / ENG-0064 x1      （V9 原登记 183/187）
```

5 例失败逐条（`grep "^FAIL"` 原文，非转述）：

- 3× `ENG-0022 statement not yet supported in M2 compiler: Sql(":= i_str")`，命中文件
  `0907sql/aaspubs1_20260907_修改版.sql`、`all_sq/collected_sql/collected_sql/PKG_IMPORT_EXCEL.sql`、
  `final2/aaspubs1_20260828-final.sql`——同一处**集合变量整体赋值** `v_info_tab := split_string_into_table(…)`
  在三份收集件里各计一次。上一版把这三条写成"集合元素赋值"不准：元素赋值
  `t_res(1) := 100` 是探针里的另一条（`Sql(":= 100")`），两者同落 `ir/stmt.rs` 的
  ENG-0022 兜底支，但语料命中的是整体赋值。
- 1× `ENG-0048 unsupported cursor reference: FunctionCall { name: [c_new_stock], … }`
  （`all_sq/…/PKG_SPLIT_TRADE_STEP3_SH.sql`，带参游标 `OPEN`；基线树同名文件不含带参游标，编译通过）。
- 1× `ENG-0064 声明了例程却未登记任何例程…`（`all_sq/…/SPLIT_LOG_INFO.sql`，登记数为零的
  静默丢弃守卫）。

V9 表已补记。

### 判定与待办

- 集合类型**读取**不属拒绝面：待办是把 `t(i)` 识别为集合元素访问并在编译期显式拒绝
  （改编译器，AGENTS §2 走 Red→Green），并补探针口径的单元/守护测试。
  上一版说"这些拒绝路径**没有任何测试断言**"说宽了，按码逐个核对（仓库根
  `grep -rnE "ENG-00(22|48|59|64)" spinduck/tests/` = 5 命中）：
  `ENG-0064` 已有两条守护（`tests/guard/guard_register_silent_drop.rs:48`、
  `tests/guard/guard_ir_in_list_empty.rs:38`），`ENG-0022` 兜底支已由 `FORALL` 断言一条
  （`tests/compiler_contracts.rs:287`）；**真正零断言**的是 `RETURN NEXT`、`PIPE ROW`、
  集合赋值、`OPEN c(args)`（`ENG-0048`）、`FOR r IN c(args)`（`ENG-0059`）这五类具体构造，
  它们只有本节的手工 `compile` 证据，V9 门禁无法机械化。
- 语料里该形态真实存在（`all_sq/collected_sql/collected_sql/PKG_IMPORT_EXCEL.sql`：2429 行
  `v_info_tab := split_string_into_table(p_i_extendStr,'$|$')` 后接 2430 行
  `v_FUND_CODE1 := v_info_tab(1)`），只是该文件在 2429 行就停在 `ENG-0022`（实测
  `Sql(":= i_str")`），读侧未暴露。

### 边界（不声称做到的）

- 本轮只改文档口径，未改代码、未改任何测试；测试数与门禁不变。
- `KERNEL-0010` 在"目录未登记名 + SQL 上下文"下的实跑只经由集合读取探针得到，
  未穷举其它未登记名场景（如下沉形态不支持时的 `BIZ-0160` 分支只引用既有测试，
  本轮没重跑）。
- §3 的两个数**不是 clone 本仓库就能复现的**：`lib/fastaas` 是子模块（`.gitmodules`
  指向 `git@github.com:NO3623/fastaas.git`，私有），考题语料不在本仓库跟踪面内
  （V21.5 定案整目录不入库）。只 clone SpinDuck 的人能复现的是上面六条 `/tmp` 探针，
  以及仓库内的 `./spinduck/target/debug/spinduck compile golden/case_corpus_purchase/sql`
  → `files=7 ok=7 failed=0 routines=6`（本轮实跑）。


> **后续事实（2026-11-08，V26.5 回写）**：V21.9–V21.12 的「参照实例不可作为
> 过程执行基线」结论**只在 5.0.0 amd64 成立**。开发机复现确认（干净实例裸
> CALL → postmaster exit 0，RestartCount 0→1→2，服务端零错误日志）后，
> 性能基准改用 openGauss 7.0.0-RC3 作替代参照：同夹具 3 行种子冒烟产物与
> 黄金断言逐值一致后才采用；裸 CALL 同语句在 7.0-RC3 上正常完成并落行。
> 口径详见 V26.5。

## V24. 数据通道认证面：sha256 实测可连，驱动由 `postgres` 换为 `gaussdb::sync`

**结论**：README「已知限制」里"GaussDB 侧若为 sha256 口令认证，纯 Rust `postgres`
驱动不支持（v1 需 md5 或自建 TLS 通道）"这条**已被驱动替换解除**——不需要自建 TLS、
不需要服务端降级 md5。

### 一手证据（本机 amd64 镜像、stable 1.98）

对照矩阵，同一实例、同一口令、同一 `SELECT`/DDL/DML 序列：

| 被测实例 | 认证形态 | `postgres`（旧驱动） | `gaussdb::sync`（新驱动） |
|---|---|---|---|
| 新建默认实例（5434，`SHOW password_encryption_type` = **2**，即 sha256only） | sha256 | `ENV-0020 gaussdb: error parsing response from server`（握手即失败） | 连接成功；`SELECT` 取回 `[1, sd]`；`CREATE/INSERT/SELECT/DROP` 全通 |
| 现有 golden 容器（5433，`password_encryption_type = 1`，md5） | md5 | 通 | **同样通**——换驱动不打断参照侧 |

驱动侧依据：`crates/tokio-opengauss/src/connect_raw.rs` 有
`AuthenticationSha256Password` / `AuthenticationMd5Sha256Password` / SM3 三条握手支路，
算法在 `crates/opengauss-protocol/src/authentication/sha256.rs`。上游仓库
`c2j/rust-opengauss`，`license = MIT OR Apache-2.0`，`rust-version = 1.85`，
`gaussdb` crate 版本 0.1.0（未发 crates.io，故只能是 git 依赖）。

### TDD 循环（测试名 + 改动 + 命令结果）

- **Red**：新增 `spinduck/tests/gauss_sha256.rs` 三条断言
  （`should_connect_and_fetch_rows_on_sha256_auth_instance`、
  `should_refuse_to_claim_sha256_when_instance_is_md5_only`、
  `should_execute_ddl_and_dml_on_sha256_auth_instance`），在旧驱动下
  `3 failed`，理由全是 `connect_parts → ENV-0020 error parsing response from server`。
  中途一次编译失败（`Option<&Option<String>>` 不是迭代器），先修测试自身再取红。
- **Green**：`spinduck/Cargo.toml` 移除 `postgres`，加
  `gaussdb = { git = …, rev = "b95ed0b9…", default-features = false, features = ["sync"], optional = true }`；
  `gauss = ["dep:gaussdb", "native"]`；`src/io/gauss.rs` 换
  `gaussdb::sync::{Client, NoTls}` 与 `gaussdb::sync::driver::SimpleQueryMessage`。
  `SPINDUCK_OG_SHA256_HOST=… SPINDUCK_OG_SHA256_PW_FILE=… cargo test --features gauss-sha256 --test gauss_sha256`
  → `3 passed / 0 failed`。
- **第三处门禁修正（Refactor）**：门控初版写"缺环境打印 SKIP 后返回"，实测报
  `3 passed`——正是 §4.1 禁止的假绿。改为两级：`gauss-sha256` feature 不开时
  `running 0 tests`（如实"未纳入"），开了却缺 env 直接 `panic!` 变红，与
  `gauss_e2e.rs` 的"缺容器必红"同规格。三条命令分别实测到 `3 passed` /
  `FAILED` / `0 passed`。

### 门禁复跑

- `cargo fmt --all` + `cargo clippy --all-features -- -D warnings` 干净。
- `cargo test --features native` = **293 passed / 0 failed / 0 ignored**（与 V22/V23 持平，本轮未增减 native 层测试）。
- md5 回归面：`cargo test --features gauss --test gauss_e2e` → `2 passed`；
  `--test golden_gauss --test golden_corpus_gauss` → `1 passed` + `2 passed`。
- sha256 复现口径改由 compose 服务承担并实测过一次端到端：
  `GS_PASSWORD=… docker compose up -d gaussdb-sha256`（全新卷 `gauss_sha256_data`）
  → healthcheck 转 healthy，`SHOW password_encryption_type` = **2**
  → 同一条探针 `3 passed / 0 failed`。探针用的临时容器与卷、`/tmp` 下的口令文件
  （0400，均自运行容器 env 派生）已在取证后删除，仓库内不驻留口令。
- `cargo audit`：一次成功运行报 `4 vulnerabilities`，四条码
  `RUSTSEC-2026-0367/0368/0369/0429` 与 CI 现有豁免清单**逐条相同**（即未见新增条目），
  明细里抓到 `zip` 6.0.0 → `RUSTSEC-2026-0369`（CVSS 9.1，需 ≥9.0.0），同属既有豁免；
  其后多次运行取公告库网络失败（`error sending request for …/advisory-db.git`），
  故"四条确属构建期 zip"这一条本轮未能重取明细，需 CI 侧或网络恢复后复验。

### 未做的事（明确不声称）

- **未撤 golden 容器的 md5 后门**：撤掉即同时改参照实例与认证面，黄金基线需重跑，
  属独立决策；本轮只把 sha256 能力加进来并钉住探针。
- **未启用 TLS**：`tls-native-tls` 会把 native-tls 栈（Linux→动态 OpenSSL、
  Windows→schannel）拉进 §10 平台矩阵，Win7 目标是 `-Zbuild-std + crt-static`，
  未经探针；v1 继续 `NoTls`。
- **未开类型扩展 feature**（`with-chrono-0_4`/`with-rust_decimal-1`/`with-serde_json-1`/
  `with-uuid-1`）：物化/回写仍是文本承载（§13.1），依赖面收窄优先。
- **`Cargo.lock` 净增 153 行 / 删 57 行**：来源是 `postgres`/`tokio-postgres` 出、
  `tokio-opengauss`/`opengauss*`/`gaussdb` 入与 tokio feature 面变化，逐条对应上表；
  未做无关漂浮升级。

---

## V25. 发版产物：三目标 CI 两轮（tag `v0.1.1` 首跑 / `v0.1.2` 复跑）全绿，Linux 侧另有本机 glibc 2.28 一手证据

**结论**：AGENTS §9 的 Platform Build 从"文档里声称有"变成实物
（`.github/workflows/release.yml`，tag `vX.Y.Z` 触发发布、`workflow_dispatch` 只构建
不发布），并已在 GitHub 上连跑两轮：三个目标（Linux x86_64 / Linux aarch64 /
Windows x86_64）构建 + 冒烟 + 发布全绿，`v0.1.1` 与 `v0.1.2` 各四个资产已发布。
Linux 两目标除 CI 证据外另有本机一手证据（下表）。

### 一手证据（本机 apple silicon，stable，zig 0.15.2）

| 目标 | 命令 | 构建 | 产物 GLIBC 上限（rockylinux:8 内 `readelf -V`） | 冒烟（glibc 2.28 实例内跑解包后的字节） |
|---|---|---|---|---|
| x86_64 | `cargo zigbuild --release --locked --features cli --target x86_64-unknown-linux-gnu.2.28` | `Finished in 3m 51s` | `GLIBC_2.18 / 2.23 / 2.25 / 2.28`（最大 2.28） | `spinduck version` → `spinduck 0.1.0`；`call prc_smoke 14 null` → `OUT p_o = 42`，`exit=0` |
| aarch64 | 同上，`--target aarch64-unknown-linux-gnu.2.28`（cargo-zigbuild **0.23.4**） | `Finished in 4m 27s` | 最大 `GLIBC_2.28` | 同上，两条断言均通过（arm64 容器原生执行，非模拟） |

参照侧环境自证：`rockylinux:8` 容器内 `ldd (GNU libc) 2.28`——即 AGENTS §10 的最低支持面
本身，不是"差不多的发行版"。

冒烟用的是打包后的字节：`tar -czf` → 解到 `/tmp/sd-smoke` → 从解出来的目录里执行，
顺带验证 Package/Smoke 两步 shell 逻辑本身（workflow 里这两步与本次逐字同构）。
压缩包实测 11,750,719 字节（aarch64）。

### 被排除与踩到的坑（原文留证）

1. **cargo-zigbuild 0.22.1 在 aarch64 链接期失败**（x86_64 同版本成功）：
   ```text
   error: linking with `.../zigcc-aarch64-unknown-linux-gnu.2.28-7810.sh' failed: exit status: 1
   = note: error: unsupported linker arg: --fix-cortex-a53-843419
   ```
   根因是 rustc 对 aarch64-unknown-linux-gnu 传 LLD 专用参数，zig 的 cc 包装不转发。
   0.23.4 通过 → workflow 顶部把 `ZIGBUILD_VERSION: "0.23.4"` 与 `ZIG_VERSION: "0.15.2"`
   钉成 env，并写明"这是下限不是偏好"。
2. **"在 glibc≤2.28 的容器里原生构建"这条备选路当前不可用**：`rockylinux:8` aarch64
   内 `dnf install gcc-toolset-12-*` 失败于镜像源，非包名问题（包名问题已修，第二次
   跑到源仍全镜像 404）：
   ```text
   Error: Failed to download metadata for repo 'appstream': ... Cannot download,
   all mirrors were already tried without success
   （多镜像 404 for .../rocky/8.10/AppStream/aarch64/os/repodata/*-primary.xml.gz）
   ```
   另注：rockylinux:8 基础镜像内**没有** `python3`，容器内跑门禁脚本需要显式装。
3. **release 构建期有一条 lint 级链接器提示**（两个 Linux 目标都有，非本轮引入）：
   `warning: linker stderr: ignoring deprecated linker optimization setting '1'`
   （`#[warn(linker_messages)]`，来自 zig 的 lld）。常驻门禁的 `clippy -D warnings`
   不覆盖 release 交叉构建，故不会红；若日后把 `-D warnings` 扩到发版 job，需要显式处理。

### 门禁脚本（两条断言面 + 各自自检）

- `scripts/ci/check_max_glibc.py`：`--self-test` 通过；对真产物报
  `需要 GLIBC_2.28（策略上限 GLIBC_2.28）` 且退出 0；负例（PATH 前置伪造 `readelf`
  输出 `GLIBC_2.39`）报 `要求 GLIBC_2.39，超出 GLIBC_2.28` 且退出非零——正负两面都取过证。
- `scripts/ci/check_release_tag.py`：`--self-test` 通过（含"非 `vX.Y.Z` 形态 tag 必须被拒"
  四条负例）；`--tag v0.1.0` → stdout 裸版本 `0.1.0`；`--tag v9.9.9` → 退出 1 并指向
  "先改 Cargo.toml 再打 tag"。`--print spinduck/Cargo.toml` 供干跑取版本号。

### 首次发版与版本号

上表的本机证据取于 crate 版本 `0.1.0`（`spinduck version` 输出即 `spinduck 0.1.0`）；
首个 tag 走 `v0.1.1`，同一提交把 `spinduck/Cargo.toml` 的 `version` 由 0.1.0 bump 到
0.1.1，锁文件随之只多这一行版本号变化。版本号只影响 `meta` job 的 tag 守卫与资产命名，
不影响构建路径，故 Linux 两目标的构建/glibc/冒烟结论对 0.1.1 同样成立。tag 命名必须
`vX.Y.Z`：不带 v 的 `0.1.1` 由 `check_release_tag.py` 直接拒（其 `--self-test` 里的负例之一）。

### CI 首跑（run 42336602157，tag `v0.1.1`，五个 job 全 success）

| job | 关键断言原文（取自各 job 日志） |
|---|---|
| 版本守卫与脚本自检 | 两条 `self-test ok`；`产物版本号 = 0.1.1` |
| Build linux-x86_64 | `spinduck/target/x86_64-unknown-linux-gnu/release/spinduck: 需要 GLIBC_2.28（策略上限 GLIBC_2.28）`；解包后的字节 `OUT p_o = 42` + `spinduck 0.1.1` |
| Build linux-aarch64 | 同上两条，路径为 `target/aarch64-unknown-linux-gnu/release/spinduck` |
| Build windows-x86_64 | `dumpbin /dependents` 静态导入清单（见下）；解包后的字节 `OUT p_o = 42` + `spinduck 0.1.1` |
| 发布 GitHub Release | `资产数 3（期望 3）` + `SHA256SUMS.txt` 三条哈希 |

已发布资产（<https://github.com/c2j/SpinDuck/releases/tag/v0.1.1>，非 draft、非
prerelease）：

| 资产 | 字节数 | SHA-256 |
|---|---|---|
| `SHA256SUMS.txt` | 309 | — |
| `spinduck-0.1.1-linux-aarch64.tar.gz` | 11,730,589 | `3e11d13da768b671ac68d26ff9332c4db2946b89bd7d338307d09f94738146ff` |
| `spinduck-0.1.1-linux-x86_64.tar.gz` | 13,047,389 | `6b8b51f97d012c45d1f70ac1afa91362c5b1f3bdd8c7848825e930e2edf752d2` |
| `spinduck-0.1.1-windows-x86_64.zip` | 11,507,084 | `fcf8ac03ff7cbb7f8af9193de54f1558ce1fe639fb9b5c5f599eecd3108ac001` |

Windows 那条"目标机需 VC++ 2015-2022 x64 运行库"的前提，这次由产物侧取证而不只是
README 里推断：`dumpbin /dependents` 打印出 `MSVCP140.dll`、`VCRUNTIME140.dll`、
`VCRUNTIME140_1.dll`（三者即 redist 实体）与一组 `api-ms-win-crt-*.dll`（UCRT 走系统
转发桩，Win10+ 自带），另有 `KERNEL32.dll / WS2_32.dll / ntdll.dll /
bcryptprimitives.dll`。边界仍是「未做的事」里记的那条：该面只覆盖静态导入。

### 已知缺陷：三个构建 job 的 `rust-cache` 都没命中 `target` 缓存

run 42336602157 的三个构建 job，`Swatinem/rust-cache@v2` 步骤各自打印同一组：

```text
Error: The process '/home/runner/.cargo/bin/cargo' failed with exit code 101
    command: 'cargo metadata --all-features --format-version 1 --no-deps'
    stderr: 'error: could not find `Cargo.toml` in `/home/runner/work/SpinDuck/SpinDuck`
             or any parent directory'
Workspaces: /home/runner/work/SpinDuck/SpinDuck      ← 仓库根，本仓库根没有 Cargo.toml
Cache Paths: ... /home/runner/work/SpinDuck/SpinDuck/target
No cache found.
```

根因是 `uses:` 步骤**不跟随** `defaults.run.working-directory: spinduck`（该 defaults 只
作用于 `run:` 步骤），rust-cache 因此在仓库根找 manifest。两条后果：⑴ 步骤非致命——该
action 把 `cargo metadata` 失败降级成日志，job 依旧 success，所以红绿面上看不出问题；
⑵ 它登记的缓存路径 `<根>/target` 永远不会被写入（真正的产物目录是 `<根>/spinduck/target`），
只有 `~/.cargo/registry`、`~/.cargo/bin` 那部分缓存生效。

对本次结论的影响：上表与本机证据里的 `Finished in 3m 51s / 4m 27s` 是**冷构建**时长，
不能当"缓存命中后的稳态时长"引用。

已按同一修法补齐 `release.yml` 两处与 `ci.yml` 四处（`with: workspaces: spinduck`），并在
v0.1.2 这一轮取到修复后的 run 级证据（下节）：**manifest 定位缺陷已消失，缓存命中与否
仍未取证**。

### v0.1.2 复跑（PR #5 → merge commit → tag，两条 run 全绿）

流程按"PR 先绿 → 合并 → bump 随 PR 落地 → tag 打在 CI 已验证的 SHA"走：

| 环节 | 对象 | 结果 |
|---|---|---|
| 常驻五门禁（PR） | run 42338938955 @ head SHA `c336222` | format / clippy / test / audit / golden 全 success |
| 合并 | merge commit `a42c2d7`（PR #5） | `git diff --stat c336222 origin/main` 为空——main head 与已验证的 PR head **同内容** |
| 常驻五门禁（push main） | run 42339792560 @ `a42c2d7` | 五 job 全 success（v0.1.1 缺的那份"tag 的 head SHA 上常驻门禁留痕"，这一轮补上了） |
| 发版 | run 42339823731 @ tag `v0.1.2` → Release 已发布 | 五 job 全 success |

发版矩阵复跑断言原文：

- 版本守卫：两条 `self-test ok`；`产物版本号 = 0.1.2`
- linux-x86_64：`spinduck/target/x86_64-unknown-linux-gnu/release/spinduck: 需要 GLIBC_2.28
  （策略上限 GLIBC_2.28）`；解包后 `OUT p_o = 42` + `spinduck 0.1.2`
- linux-aarch64：同两条，路径 `target/aarch64-unknown-linux-gnu/release/spinduck`
- windows-x86_64：`dumpbin /dependents` 静态导入清单与 v0.1.1 **逐条同集合**，共 18 项：
  `MSVCP140.dll`、`VCRUNTIME140.dll`、`VCRUNTIME140_1.dll`（redist 实体）、10 个
  `api-ms-win-crt-*.dll`（UCRT 转发桩）、`api-ms-win-core-synch-l1-2-0.dll`、
  `KERNEL32 / WS2_32 / ntdll / bcryptprimitives`；解包后 `OUT p_o = 42` + `spinduck 0.1.2`
- Release job：`资产数 3（期望 3）`

| 资产 | 字节数 | SHA-256 |
|---|---|---|
| `SHA256SUMS.txt` | 309 | — |
| `spinduck-0.1.2-linux-aarch64.tar.gz` | 11,730,723 | `9be11223d1a26986fe3ad8491bd34b832f41d95fb1f9cc39a868069a6e8a03e8` |
| `spinduck-0.1.2-linux-x86_64.tar.gz` | 13,047,431 | `3586d976f26726f6503c0a355f06f14bf540136bbc54e0aeef9adfeb51b651d9` |
| `spinduck-0.1.2-windows-x86_64.zip` | 11,507,085 | `8a8aa329dc49fbd16b9951f1d56b9e5e7010d5e4e2aa3f304113e952ca67df91` |

rust-cache 修复的取证方式与结论（对 `gh run view --log` 的计数，不是看红绿）：

- 修复前（v0.1.1 三个构建 job）：各有一条 `could not find Cargo.toml` + `exit code 101`，
  Cache Paths 为 `<根>/target`。
- 修复后：windows 发版 job、push-main 的 clippy 与 test job 三个抽样里
  `could not find` 命中数均为 **0**，且 `spinduck/target`（Windows 为
  `spinduck\target`）各出现 3 次——分别在 Cache Paths 列表、Workspaces 列表与 Post 步骤的
  `Cleaning …` 里，即真正产物目录已进入缓存面。
- **仍未取证的一条**：这几个 job 的 Restoring 全是 `No cache found`。原因是缓存 key 随
  工作区路径/锁文件集合变化而失效（首跑必冷），命中需要"同 workflow、同分支口径、
  key 不变"的第二次 run；在此之前，构建时长只能按冷构建口径引用——本轮发版 job 里
  实测到的区间是 `rust-cache` 恢复到 glibc 反查之间 aarch64 ≈ 9m56s、x86_64 ≈ 9m34s
  （与本机 `Finished in 4m 27s / 3m 51s` 的差值即 runner 上的依赖编译与安装步骤，
  两个口径都还没享受到 target 缓存）。

### 未做的事（明确不声称）

- **Windows x86_64 本机证据仍为零**：本机无 MSVC 工具链，该目标的构建/打包/冒烟证据全部
  来自 CI 发版 run（`v0.1.1`、`v0.1.2` 两轮），两轮是同一条路径的重复，不构成独立交叉印证。
  该目标不加 `+crt-static`，运行库前提由发版 job 每次打印的 `dumpbin /dependents` 静态导入
  清单取证（`ilammy/msvc-dev-cmd` 提供环境）——注意该面**只覆盖静态导入**，delay-load 与
  运行时 `LoadLibrary` 的依赖看不到，且 windows-latest 自带运行库，冒烟通过不能反推裸机
  可直接跑。
- **macOS 双架构、Win7（nightly `-Zbuild-std` + `crt-static`）没有发版 job**：本轮范围按
  用户点名的 3 目标；AGENTS §10 表相应两行已标注"尚无发版 job"。
- **只跑过 tag push（发布）路径**：`workflow_dispatch` 的"只构建不发布"分支未实跑过，
  它靠 `if: github.event_name == 'push'` 挡发布，这条判断目前只有 YAML 级依据。
- **`target` 缓存命中仍未取证**：修复只证明了"缓存面里出现了正确的产物目录"，`Restoring`
  两轮都是 `No cache found`（见上节）。
- **发版 SHA 上的常驻门禁证据：`v0.1.2` 起才补齐**：tag push 本身不触发 `ci.yml`，
  `v0.1.1` 的 head SHA 上五门禁只有本机证据。`v0.1.2` 改走 PR，两条 run 都留了痕——
  PR run 42338938955（head `c336222`）与 push-main run 42339792560（merge `a42c2d7`，
  与已验证 head 同内容）各五 job 全绿，tag 指在该 merge commit 上。后续发版沿用
  "bump 随 PR 落地、tag 打在 CI 已验证的 main head"这条顺序（AGENTS §9）。

## V26. 三段引擎优化与数据搬运批量通道的实证（记忆化 / prepared / 合并 COMMIT 开关 / COPY）

> **与 V24 的关系（rebase 后口径）**：本节初稿基于 `postgres` 驱动的旧树，V24
> （sha256 认证面、驱动换 `gaussdb::sync`）落在同一窗口。V26.4 的回写通道已按
> 新驱动重做——`gaussdb::sync` 自带 `copy_in`/`CopyInWriter`（openGauss 官方
> 驱动 fork，协议层含 CopyInResponse），手写线协议会话随之退役；sha256 口令
> 实例不再需要任何降级（V24 结论原样成立）。V26.4 的重做实测见本节内更新值。

**结论先说**：100k 行语料过程 `prc_trd_deal_purchase`（openGauss 参照 vs SpinDuck，
`golden/case_corpus_purchase` 全套夹具）三段逐项迭代、逐项基准对比：引擎执行
698.9s→264.3s（首跑）/ 938.2s→316.2s（稳态含 DELETE），物化 244.7s→26.0s，回写
204.0s→1.18s；全链路 1,147.6s→291.5s（−75%）。每段默认语义不变或显式 opt-in
开关（`Engine::batch_commit` 默认 OFF）。产物（`dat_trd_repurchase` 逐值）在
三段全部运行中与基线一致。本节只做实证记录与口径钉死；机制描述见
architecture §3.5 补注与 `writebuf.rs` 头注释。

### 基准环境（一次声明，全节适用）

- 参照：openGauss **7.0.0-RC3**（`opengauss/opengauss:latest`，amd64，127.0.0.1:5435）。
  采用依据见 V26.5（5.0.0 amd64 的过程调用自杀在本机复现，V21.9–V21.12 补充证据）。
  部署夹具与 golden 同源（tables/views/stubs/routines/seed 逐字），3 行种子冒烟
  `CALL pkg_split_trade_step3_sh.prc_trd_deal_purchase('20250715', NULL)` 产物
  `[2, 300, 30000, 6]` 与黄金断言逐值一致后才做基准。
- 被测：SpinDuck release 构建，`golden/case_corpus_purchase` 部署口径
  （`verify::deploy_schema/deploy_routines` + 项目规则），数据集与参照同一份
  SQL 文件装入（37 列 ×100k，含 2% A999 异常行=partner 未维护走行级跳过路径）。
- 计时口径：参照为 gsql 客户端墙钟（含 ~0.3s 启动）；SpinDuck 为进程内
  `Engine::call` 墙钟（部署/编译/装数在计时圈外）。语句计数来自
  `MeterExecutor`（按 SQL 文本聚合下发的装饰器，随 bench 存证）。
- 两侧写路径对称：过程内逐行 INSERT+COMMIT（fsync 同价）。

### V26.1 迭代①：纯标量语句执行期记忆化（MemoizingExecutor）

- 依据：Meter 计量显示**解释器只占 wall 2.7%**，97% 在内核下发；每行 16 条语句
  中 5 条是纯标量表达式宏（`substr`/`to_char(to_date()+n)`/date_diff），参数组合
  仅 4 种。白名单（substring/strftime/strptime/to_days/date_diff）按
  （SQL 文本, 绑定值）缓存，`Engine::call` 入口清空。
- 实测（10k 稳态）：68.12s→46.60s（**−31.6%**）；语句 157,006→78,617（−50%）。
  **口径注记（V26.7 修后）**：该数字采集于「memo 对任意 DDL 失效」的版本——
  失效口径收窄到 MACRO 后（V26.7），带 sqlfn 下沉的游标循环不再每行清缓存，
  该类过程的实际收益应高于本数字；重跑见 V26.7 口径标注。
- 测试 6 个：合并命中、按绑定值区分、表查询/非确定性函数/execute 不缓存、
  clear 后重执行（`exec::tests::should_memoize_*` 等）。

### V26.2 迭代②：内核 prepared 语句缓存 + 一个内核缺陷被实测钉死

- `prepare`→`prepare_cached`（容量 512，内核默认 16）：微查询均摊 212→91µs、
  BEGIN 70→46µs；但 wall 持平——大项（COMMIT 1.05ms / 单行 INSERT 0.99ms /
  点查 0.8ms）是执行硬成本（WAL fsync / MVCC / 管线固定），parse 只占小头。
- **实测钉死的内核缺陷（1.5.5）**：缓存的 `CREATE OR REPLACE TEMP TABLE` plan
  二次执行留下 **0 列残表**（`Binder Error: table sd_fn0 has 0 columns but 2
  values were supplied`），sqlfn 物化路径（`materialize_fn_sites`）必踩——
  V24 前该路径在开关 OFF 下由既有测试 `should_execute_pl_function_calls_inside_
  sql_statements` 抓出。修复（复核决断 1 后的现行口径）：**DDL 本体绕过语句
  缓存** + execute 期 rebind 兜底——不做全缓存 flush（sqlfn 物化路径每行一次
  CREATE TEMP TABLE，全 flush 等于把 512 容量抵消掉）。守护测试
  `guard_should_recreate_temp_table_across_prepared_cache` /
  `guard_should_rebind_prepared_shape_after_table_recreate` 集中登记于
  `tests/guard/guard_exec_native_prepared_cache.rs`，在**无 flush 前提**下钉住。
- 接缝纪律：DuckDB API 触碰只在 `sql/exec_native.rs`（AGENTS §1）。

### V26.3 迭代③：合并 COMMIT 开关（`Engine::batch_commit`，默认 OFF）

- 依据：COMMIT 1.05ms + 单行 INSERT 0.99ms 占内核时间 44%，只能靠事务边界
  合批消化——属对 §3.5「过程内 COMMIT 即时生效」的显式偏离，故为 opt-in 开关。
- 语义（四条，`writebuf.rs` 头注释为准）：① 过程内 COMMIT 吞并，INSERT 攒批
  合并（同前缀连续攒入为一组，每组刷盘为一条多值 INSERT；缓冲行数上限
  10,000），出口统一刷盘由 `engine.call` 最终 COMMIT 持久化；② 触及
  缓冲表的语句先刷盘（读自己写）；③ 合并失败回退逐行重放，失败行**数据级
  跳过**——与 OFF 的差异是该行不进 PL 异常处理器（inst_log 缺失），组内首行
  失败视为表级错误上抛；④ 嵌套例程边界即刷盘点（各自独立缓冲；SQL 上下文
  函数调用不批——语料函数体无 INSERT）。
- 实测（10k 稳态）：46.6s→23.6s（**−49%**）；100k 首跑 439.3→264.3s、稳态
  911.7→316.2s；语句 786,017→494,015。产物逐值一致（三组运行）。
- 评审修复（V24 PR 审查实测抓出，各带回归覆盖）：① `INSERT … SELECT`
  不可缓冲形态回落即时下发（原 ENG-0033 上抛，语料合法形态）；② flush
  失败时逐组出账（原多组部分成功后重复下发）；③ `INSERT INTO sch.t`
  限定名的读自己写命中（原 token 切分漏判）；④ 字符串字面量内的 `$`
  不再误判为占位符；⑤ push 改收已验证前缀（原不可达错误分支）。
- 测试 10 个（V26.10 修后实数；interp 层 FakeExecutor 计数），按面分组：
  语义四条（合并+吞并 ON、逐行 COMMIT OFF 默认守护、触及缓冲表先刷盘、
  无空格表名形态触表刷盘）、回落两条（INSERT…SELECT、带 fn_sites 的
  INSERT）、回放一条（合并失败回退跳行）、事件面两条（刷盘点 SqlExec、
  回放跳行 BatchSkip）、上限一条（WRITE_BUF_CAP 满则先刷盘）。
- 实现中由测试/基准抓出的自身缺陷（各一条，均修复）：
  ① md5 外层摘要误用 16 字节 raw digest——正确输入是 inner 的 **32 字符
  hex ASCII 串**（服务端 28P01）；
  ② 行级业务异常（NO_DATA_FOUND，语料空表 commission 查询每行合法抛出）的
  trap 重入路径误清缓冲 → 全部行丢失（实测 count=0）——discard 收敛到
  「handler 不可重入的最终异常」；
  ③ 字面量行（`VALUES ('x')`，无占位符）需原样采用；
  ④ VALUES 表达式是复合形态（`round(($a-$b)/$c, 2)`）——字面量化必须
  占位符**就地文本替换**，按值提取会拆散表达式导致列/值错位（22→26 值实测）。

### V26.4 数据搬运批量通道：postgres crate 的 copy_in 静默无效 → `gaussdb::sync` 驱动通道

- **旧驱动缺陷存证**：`postgres` crate 0.19 的 `copy_in`（扩展协议
  Parse/Bind/Execute）对 openGauss 5.0.0/7.0.0-RC3 均**不报错且 0 行落库**
  （CSV 与文本两种格式变体实测）；同表 gsql `\copy` 成功——服务端 COPY
  能力正常，缺口在扩展协议交互。历史对照：手写简单协议会话（TcpStream
  Startup → md5 → `Q`/`G`/`d`/`c`）曾在此驱动上打通（10 万行 104–143ms），
  探针存证于 V24 初稿（未入库，结论保留在此）。
- **落地（V26 重做口径）**：驱动换 `gaussdb::sync`（V24）后，其 `copy_in` /
  `CopyInWriter`（`opengauss-protocol` 独立协议实现，**非** tokio-postgres
  fork）对 openGauss 实测可用——`sync_append` 切换为驱动 `copy_in`
  （COPY FROM STDIN，CSV 分块写入，`finish()` 行数与本地计数交叉校验，
  单事务原子追加）；`materialize` 为 500 行/语句批量多值 INSERT。双内核
  e2e（`gauss_e2e`）与 100k/98k 全量实测通过。
- **实测（V26 重做后）**：回写 98k 204.0s→**0.89s**（480→110,236 行/s）；
  物化 100k 244.7s→**25.6s**（409→3,903 行/s）。行数核对：回写前后服务端
  行数精确 +98,000。
- **退役说明**：初稿的手写线协议会话（含 md5-only 限制与四条协议契约
  ——md5 长度含 `\0` / `G` 即转数据流 / COPY 期禁发 `Q` / `G` body[0]
  为格式码）随驱动通道退役，不再构成回写路径的认证面限制（sha256 实例
  由驱动原生支持，V24 结论原样成立）。

### V26.5 参照实例 V21.9 的本机复现；7.0.0-RC3 替代口径的验证依据

- **V21.9–V21.11 在开发机复现**：5.0.0 amd64（`187da08af341`，与 CI 同 artifact）
  干净实例裸 `CALL` → postmaster 自杀：`RestartCount` 0→1→2、`ExitCode=0`、
  服务端日志零错误（V21.10/21.11 的判定协议逐条执行，本机 100% 复现）。
  首次 CALL 幸存一次但 0 行插入（未做工作）——与 CI"3/3 轮"口径一致：
  参照实例不可作为过程执行基线，证据链闭合。
- **7.0.0-RC3 替代口径**：Lite 镜像入口在 `/entrypoint.sh`（非 5.0.0 的
  `/var/lib/opengauss/entrypoint.sh`），`GS_HOST_AUTH_METHOD=md5` 原生支持、
  `OTHER_PG_CONF` 不支持（无需——实测 `password_encryption_type=0` md5 为
  默认）；`datcompatibility=A` 由 initdb 默认给出（实测查询确认）。镜像缺陷：
  缺 `libopenblas.so.0`/`libgfortran.so.5`（openEuler RPM + 宿主 so 注入后
  正常，探针脚本存证）。部署口径与 golden 逐字一致；冒烟产物与黄金断言
  逐值一致后才采用。**黄金 pin 的 5.0.0 参照地位不变**——7.0-RC3 仅作为
  性能比对替代参照，绝对耗时不可与黄金基线历史混比。
- 端口适配：宿主 5433 被既有 dota-gaussdb（同 5.0.0 镜像，同缺陷）占用——
  按golden/README 处置方式改绑端口，未触碰他人实例。

### V26.6 门禁与测试清单

- fmt / clippy（gauss，`-D warnings`）✓；`cargo audit` 无新增告警
  （既有 zip 维持 CI 豁免口径）。
- `cargo test --features cli` 全量 **322/0**（V26.10 修后口径，cli 蕴含
  native；V24 批次时 native 口径为 307/0）：新增 memo 8（含 FUNCTION/短形态
  MACRO 判定 1）、prepared 守护 2、batch_commit 10（见 V26.3 分组）、
  回放跳行事件 1、CLI 解析 1、cases_native 6
  （OFF 4 + **ON 2**，见 V26.8 应修 1）、CLI 契约 3（bin 面，V26.10）。
- golden 回归（5.0.0 容器，默认 OFF 路径）：cases_native 4/4、
  golden_corpus_purchase 1/1、gauss_e2e 2/2（**新 COPY/批量通道在 5.0.0 实测**）
  ——7/0。ON 模式的内核级 A/B（cases_native ON 2 条）走 native 门禁（进程内
  内核、无容器依赖），断言集与 OFF 完全一致。
- 基准与探针工具存证口径（与 V26.4 一致，复核修）：`spinduck_bench`、
  `tests/wire_probe.rs`、`tests/io_probe.rs` 三者**均未入库**——本节全部
  基准数字为开发机一次性测量，结论保留在此，不可从仓库复现。

### 边界（不声称做到的）

- 合并 COMMIT 开关 ON 的两处偏差（失败行不进 inst_log、嵌套边界即刷盘）是
  **文档化偏离**而非等效实现；默认 OFF 的行为由 OFF 守护测试锁定。
- ON 模式刷盘点补发的 `SqlExec` 事件（V26.7）`elapsed_us` 记 0——与
  `sqlstmts::exec` 现行口径一致，合并语句未单独计时。
- 攒批多行共享一张按元组缓存的下沉表（sd_fnN）是独立设计题，本节不覆盖
  （带下沉的 INSERT 一律回落即时下发，见 V26.7 准入条件）。
- 7.0-RC3 参照的绝对耗时不可与黄金基线历史混比（不同内核版本）；本节所有
  "对比"都是同机同数据同时点的 A/B。
- 参照侧计时含 gsql 客户端（~0.3s 启动），SpinDuck 为进程内 call——两口径
  差异相对 200s/264s 的量级可忽略，未做归一。
- 动态 SQL（`EXECUTE IMMEDIATE`）在开关 ON 下走保守整缓冲刷盘，未单独基准。
- `lib/fastaas` 考题语料不在本仓库跟踪面内（V21.5 口径），本节数据全部来自
  `golden/case_corpus_purchase` 在库夹具。

### V26.7 第二轮复核修复：fn_sites 攒批漏物化、memo 失效收窄、刷盘事件、CLI 预算入口

第二轮复核（head `a267748`，2026-11-09）抓出的数据级阻塞项与后续修：

- **阻塞项（数据级）**：ON 模式带 sqlfn 下沉（`fn_sites` 非空）的 INSERT 被
  `take_insert` 攒批——`materialize_fn_sites` 是 `sqlstmts::exec` 的执行期
  副作用，攒批跳过它 → `sd_fnN` 未建（KERNEL-0010 硬失败）或滞留旧元组
  （静默写错值）。语料形态 `PKG_SPLIT_TRADE_STEP3_SH.prc_trd_hz_byfund` 的
  `INSERT … VALUES (… fnc_get_rate_info2 …)` 即是此形态；V26 基准恰好未覆盖
  带下沉的 INSERT 故未暴露。修复：`take_insert` 增加显式准入条件——fn_sites
  非空一律按表命中刷盘后回落即时下发；回填为「缓冲 = 只能替换 `ex.execute`，
  原路径其余副作用一律回落」的原则。回归：
  `should_fall_back_to_immediate_exec_for_insert_with_fn_sites_when_batch_commit_on`
  （修前实测红：sd_fn0 未物化）。
- **memo 失效收窄（复核应修 A）**：DDL 即清 memo 与 sqlfn 每行一次
  CREATE TEMP TABLE 相乘，带下沉的游标循环每行清空缓存。收窄到只认
  `CREATE OR REPLACE MACRO|FUNCTION` / `DROP MACRO`（`is_macro_ddl`；
  TEMP TABLE 不改变白名单纯表达式语义）。回归：`should_keep_memo_across_
  temp_table_ddl`（修前红）+ `should_clear_memo_after_macro_ddl`。
  V26.1 的 −31.6% 为收窄前采集，已加口径注记；重跑需 7.0-RC3 参照实例，
  未在本轮执行。
- **刷盘点补发 SqlExec 事件（复核应修 B）**：被合并的 INSERT 原在 JSONL
  面完全消失。`flush` 现对每条实际下发的语句（合并语句 + 回放行）发
  `TraceEvent::SqlExec`（`elapsed_us` 记 0，与 `sqlstmts::exec` 同口径）。
  回归：`should_emit_sql_exec_event_for_merged_insert_at_flush_point`。
- **CLI 步数预算入口（复核应修 C）**：`Engine::step_budget` 原有字段无入口
  ——按步数模型推断，`call --batch-commit` 跑 100k 级游标循环会撞 ENG-0032
  （基准工具未入库，当时的实际步数/预算未留档，此为推断口径而非实测）。
  补 `spinduck call --step-budget <n>`（正整数，0 拒绝），与 `--batch-commit`
  同处接线。**注意：该入口只放大顶层 `RunLimits.budget`，嵌套例程仍沿各自
  默认预算**。回归：`should_parse_call_with_step_budget`（解析层；CLI 接线的
  契约测试面 `tests/cli.rs` 仓库尚不存在，属既有缺口、非本 PR 引入）。
- **小项**：`is_insert` 字节切片改 `chars().take(12)`（与 `is_ddl` 同口径）；
  不可达的 `last_mut()` None 分支改按索引取尾组；`writebuf.rs` 头注释补
  `borrow_mut` 跨 `ex.execute` 的重入约束一行。空组分支在第三轮整支删除
  （V26.8：本节初稿写「随逐组出账重写消除」不成立——它只是改成了
  `-= 0` 的不可达形态）。
- **文档一致性（复核文档 6 条）**：V26.6 探针口径与 V26.4 统一为「未入库」；
  V26.2 改述为决断 1 后的现行修复（DDL 绕缓存 + rebind 兜底，无全 flush）
  并指向 `tests/guard/` 现行守护；「采用依据见 V24.5」改 V26.5；
  architecture §3.5 删除代码中不存在的「组随例程内业务异常边界切分」、
  「协议级细节」措辞随 WireCopy 退役改为「机制细节」；README 能力矩阵数字
  回传 V26.4 更新值、删除线/杂行清理。

### V26.8 第三轮复核修复：ON 模式内核级 A/B、回放跳行留痕、表名解析、is_macro_ddl 口径

第三轮复核（head `66cebd3`，2026-11-09）无阻塞项；三条应修 + 两条建议落地：

- **应修 1（ON 模式内核级 A/B）**：`cases_native.rs` 的 `run_project` 增加
  `batch_commit` 形参，`case_split_agg`（INSERT 带 `fnc_fee` 下沉，钉住准入
  回落路径）与 `case_corpus_purchase`（大 INSERT 循环，真攒批）各加 ON 变体
  ——**同一部署、同一断言集**在 ON 下逐值复跑，合并 INSERT 产物由真实
  DuckDB 解析，「开关不改变数据」从文档承诺变成门禁。两条一次通过。
- **应修 2（回放跳行留痕）**：`flush` 回放期被跳过的坏行补发
  `TraceEvent::BatchSkip { rows, first_error }`（枚举新变体）。第四轮复审
  ② 的决策为**接受并入契约**：事件枚举与 JSONL 形态已登记
  architecture §11.2（`first_error` 自第四轮起携带错误码）。口径更正
  （第四轮③）：当前全仓无 JSONL writer、CLI 无 `--trace`——事件面是
  「可订阅」，**JSONL 落盘属 §11.2 未完成项**，本节不断言其已可见；
  `first_error` 携带错误码使引擎缺陷与业务跳行在事件层可区分。
  回归：`should_emit_batch_skip_event_when_replay_drops_rows`（修前编译红：
  变体不存在；第四轮②断言收紧为必须含 `KERNEL-` 错误码）。
- **应修 3（表名解析）**：`tables()` 对 `INSERT INTO t(a, …)`（表名与列清单
  无空格，golden `case_split_agg` 的 routine.sql 即此写法）曾取到 `"t(a,"`
  → 读自己写永不命中。按 `'('` 截掉列清单。回归：
  `should_flush_buffer_before_touching_table_when_insert_has_no_space_before_cols`
  （修前红：DELETE 先于刷盘下发）。
- **建议 4（is_macro_ddl 口径）**：`chars().take(24)` 字符窗口使
  FUNCTION 分支永不匹配（26 字符被截），`CREATE MACRO` 短形态 /
  `DROP FUNCTION` / TEMP 修饰同漏。改前 5 个空白分隔 token 判定，覆盖
  `CREATE [OR REPLACE] [TEMP] MACRO|FUNCTION`、`CREATE MACRO` 与
  `DROP MACRO|FUNCTION`。回归：
  `should_clear_memo_after_function_and_short_form_macro_ddl`（修前红：
  FUNCTION 不清缓存）。
- **建议 5（回落分支断言保护 + 代码收敛）**：`INSERT … SELECT` 与 fn_sites
  两条回落测试改为**顺序断言**（预置同表缓冲行 → 缓冲合并 INSERT 必须先于
  回落语句下发；删掉 flush 分支会红）；`take_insert` 三处内联的
  「取 tables → touches → flush」收敛为 `flush_if_touches` 单一调用。
- **文档口径**：V26.6 测试计数回传（307→318，分类明细见上）；V26.3
  batch_commit 测试数 5→7；V26.7 的「必撞 ENG-0032」改推断口径并注明
  `--step-budget` 只作用顶层预算；「空组分支已消除」的不实表述更正（本节
  整支删除，V26.7 已附注）。
- **规模注记（§3）**：`exec.rs` 含测试 543 行（第四轮修后 `wc -l` 实测；该
  数字随 memo 口径微调会小幅漂移，判定以当场 wc 为准，两轴 500/700 内）。
  **书面豁免**：本 PR 对 exec.rs 的后续改动（is_macro_ddl）即 memo 口径
  本身，拆 memo 子模块属独立重构、另开 PR，不在本 PR 范围内。`interp/tests.rs` 的
  682 行注记在写下后被第四轮修复推过 700 上限（793 行）——已在第四轮**拆分
  为 `tests/mod.rs`（共享 helper，401 行）+ `tests/writebuf.rs`（batch/writebuf 块，452 行）**，
  两个文件均在限内；本条历史数字保留作口径教训。
- ~~**非本 PR 缺口（存档）**：AGENTS §4.1 的 CLI 契约层~~ **已在 V26.10 落地**：`tests/cli.rs`
  （bin 契约 3 条，`required-features = ["cli"]`），CI Test 口径改
  `--features cli`（蕴含 native）——`cli/run.rs` 两个开关的接线自此有测试面。

### V26.9 第四轮复核：契约决策落地、tests 拆分、缓冲上限入测

第四轮复审（head `65f3bad`）确认第三轮五条应修全部真落地、无数据正确性
阻塞项；本轮把被挂起的契约决策定案并收口小尾巴：

- **契约决策（第四轮②①）**：`TraceEvent::BatchSkip` **接受并入契约**——
  事件枚举与 JSONL 形态登记 architecture §11.2（`BATCH_SKIP` detail 携带
  `rows` 与 `first_error`）；`first_error` 从只含 message 改为
  `"CODE message"` 形态（错误码在产生点携带，§5），回归断言收紧为必须含
  `KERNEL-` 前缀（修前红）；「引擎缺陷与业务异常行自此可在 JSONL 面区分」
  的超出口径更正为「事件接缝可订阅，JSONL 落盘属 §11.2 未完成项」。
- **应修 ④（tests 拆分）**：`interp/tests.rs` 793 行破 700 上限 → 拆为
  `tests/mod.rs`（共享 helper 与 interp 层测试）+ `tests/writebuf.rs`
  （batch/writebuf 全部测试与 fixture），仅移动、断言不变，全量复绿。
- **应修 ⑤（缓冲上限入测）**：`WRITE_BUF_CAP`「满则先刷盘」此前无任何测试。
  `WriteBuffer` 增加 `cap` 字段 + 测试专用 `shared_buffer_with_cap`（生产
  入口恒为 10,000）；回归 `should_flush_partial_batch_when_write_buf_cap_
  reached`（cap=2 注入：第 3 行攒入前刷出两行合并 INSERT，出口再刷 1 行，
  两条下发各为独立语句；编译红起步）。
- **小项**：`is_macro_ddl` 的 TEMP 修饰放宽为前缀匹配（`TEMPORARY` 同义，
  潜在面收口）并加回归用例；`VecSink` 消歧 match 的 `_ => {}` 兜底改为
  显式分支；V26.3 测试清单补齐 2 个漏列名字；V26.8 规模注记更正
  （682→793→拆分后实际行数）。
- **计数（V26.9 修后）**：`cargo test --features native` 全量 **319/0**
  （V26.8 的 318 + cap 回归 1）。

### V26.10 第五轮复核：CLI 契约层落地、CI Test 口径升 cli、文档精度收口

第五轮复审（head `c1adc5a`）确认第四轮四条全部真落地、无阻塞项；本轮处理
三处文档精度与那个需要显式定案的缺口：

- **CLI 契约层落地（复审「合并前唯一需要显式定的事」，取本 PR 内补）**：
  新建 `tests/cli.rs`（`required-features = ["cli"]`，AGENTS §4.1 口径），
  驱动**打包后的二进制**走完整 parse → run → Engine::call 路径：开关同开
  成功调用（`--batch-commit` + `--step-budget 1000000`，语料同款
  LOOP/COMMIT/EXIT 例程）、`--step-budget 0` 拒绝（usage 退出码 2）、
  默认 OFF 路径同例程——`cli/run.rs` 对 `eng.batch_commit` /
  `eng.step_budget` 的接线自此有测试面。3/3 一次通过。
- **CI Test 口径升 `--features cli`**：cli 蕴含 native（既有覆盖一条不减）
  并使 cli 契约层在常驻 Test job 真实运行；AGENTS §8/§9 与 workflow 注释
  同步。**计数（V26.10 修后）**：`cargo test --features cli` 全量
  **322/0**（V26.9 的 319 + cli 契约 3）。
- **文档精度三处**：V26.8 规模注记改为不随行号漂移的表述（543 行 +
  「is_macro_ddl 即 memo 口径本身、拆 memo 另开 PR」的书面豁免）；V26.3
  batch_commit 测试 7→**10**（按语义/回落/回放/事件面/上限分组）；V26.9
  拆分注记补实际行数 401/452。
- **口径注记**：`KERNEL-0010` 是 exec_native 对所有 DuckDB 错误的统一映射，
  **不承担 BatchSkip 的判别维度**——判别信号是事件本身 + message 文本
  （architecture §11.2 已注）。
## V27. 生产 GaussDB 接入的前置取证（A0a，本机半程）：三处"看着绿、其实没跑"取到一手证据，并订正本节自记的两个数

**范围口径**：`docs/design-production-gaussdb.md` 把取证阶段拆成 A0a（本机，零功能改动）与
A0b（等真实例）。**V27 只覆盖 A0a**——所有需要连实例的数字一律进"仍是推算"（V27.6），
不在此处出现。A0b 完成后从 **V27.8** 起追加子节（V27.6/V27.7 已占用）。

**取证环境**：`spinduck 0.1.2` 的 release 产物（`cargo build --release --features cli --locked`，
rustc 1.98.0，main `cbaaae5` + 本轮未提交改动，改动只有注释与新测试，不影响被测行为）。
交付件分析对象 = 真实交付的存过与表结构目录（**目录名与本地路径从略**，本节与 design §4
提到的"交付件"都指它：**仅本地分析**，不外传、不入库、不复制进本仓；只记数量与形态，不抄内容）。

**门禁**（本轮改动后全跑）：`cargo fmt --all -- --check` 干净（先 `cargo fmt --all` 收拢了新
测试的排版）；`cargo clippy --all-features -- -D warnings` 干净；
`cargo clippy --all-features --tests -- -D warnings` 干净；`cargo test --features native`
**296 passed / 0 failed**（lib 219 + 集成/守护 77，其中新增
`tests/guard/guard_patterns_outer_join_residual.rs` 3 条）；`cargo test --features cli`
同口径全绿；`cargo audit` 报 4 条**全部是 `zip 6.0.0`**，与 CI 豁免清单四条 ID 一一对应
（`RUSTSEC-2026-0367/0368/0369/0429`，`.github/workflows/ci.yml:84-87`），未新增豁免。

### V27.1 `call` 的三种命令形态：缺表时到底响不响（design §3.3）

被测 = `golden/case_corpus_purchase` 的 `fnc_get_purchase_js_days('20250715','0130000001')`，
参照基线 `'90'`（`golden/case_corpus_purchase/cases/baselines/expected.json` 场景 1）。
三条命令只差 `--file` 传什么（完整命令见 design 附录 A）：

| 形态 | 传入文件 | 实测 stdout | rc |
|---|---|---|---|
| A | 仅主角函数 | `[KERNEL-0010] Catalog Error: Scalar Function with name fnc_com_if_deal_date does not exist!` + `Did you mean "encode"?` + `LINE 1: SELECT fnc_com_if_deal_date($p0, $p1)` | **1** |
| B | 主角 + `sql/ddl/tables.sql` + `cases/datasets/seed.sql` | **与 A 一字不差** | **1** |
| C | 主角 + `0_SIMPLIFIED_fnc_com_getday.sql` + `0_SIMPLIFIED_fnc_com_if_deal_date.sql`（补齐依赖函数，仍不建表） | `RETURN 0` | **0** |

三点结论（**已实测**）：

1. **B≡A** 证明"把建表脚本经 `--file` 递进去"完全无效：`Engine::register` 只编译
   `CreateProcedure | CreateFunction | CreatePackageBody`（`spinduck/src/engine.rs:124,138`），
   其余语句不执行也不告警。已有守护 `tests/guard/guard_register_silent_drop.rs` 锁同一行为，
   本条是它在**真语料 + 发版二进制**上的一次落地复现。
2. **C 的 0 不是引擎吞异常**，是语料自己的兜底：
   `golden/case_corpus_purchase/sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql:14-23` 写的是
   `BEGIN SELECT … INTO v_purchase_days FROM par_sys_purchase …; EXCEPTION WHEN OTHERS THEN
   v_purchase_days := '0'; END;`。A/B 之所以响，是因为它们死在这段兜底**之外**的 `:29`。
   ⇒ 设计文档 §3.3 原写"被 PL 异常分支吞掉"是**错定性**，本轮改为"CLI 无部署通道 + 语料自带
   `WHEN OTHERS` 兜底"的组合，引擎侧无过错。
3. **`0` 是这份基线里的合法值**（同文件场景 3「品种无配置 → `WHEN OTHERS` 归 `'0'`」、
   场景 4「日期不在生效区间 → 同归 `'0'`」）。⇒ C 的产物连值域都对得上一次正确复现，
   这是它比报错危险的实证理由，也是 DoD #2 要求"不存在 rc=0 但值为 0/空"的依据。

### V27.2 `(+)` 不适配形态的后果：显式红，不是静默错值（design §4 硬事实 #2、§8）

新增守护 `spinduck/tests/guard/guard_patterns_outer_join_residual.rs`（`Cargo.toml` 注册
`[[test]]`），三条 **已实测** 全绿：

| 测试 | 锁定行为 |
|---|---|
| `should_keep_marker_when_marker_side_is_not_last` | 标记侧不在 FROM 末位 → 产物**不含 JOIN** 且 **`(+)` 原样保留**（不静默改连接顺序，也不静默丢标记） |
| `should_keep_marker_when_both_sides_marked` | 两侧各带标记 → 同上保留 |
| `should_fail_loudly_in_kernel_on_residual_marker` | 保留的 `(+)` 交进程内内核 → **显式解析错**。含合法 JOIN 对照组（必须先过，否则证明的是夹具问题） |

第 3 条的实测错误文本（一字抄录，`\n` 为原文换行）：

```text
Parser Error: syntax error at or near ")"\n\nLINE 1: ... a.id, b.id, c.id FROM ta a, tb b, tc c WHERE a.id = b.id(+)\n                                                                      ^
```

⇒ design §8 原"预期是解析器拒绝、**不是静默错值**，这条必须实测确认，若相反则优先级高于 A1"
**按推测方向成立**，优先级不抬高；断言已收紧到"必须落 Parser Error 且回显含 `(+)`"，
内核升级改文案即红（V6 门禁复核点）。

交付件 23 的命中面（同口径 `grep`，命令在 design 附录 A）：**已实测** = 出现处 7,906 /
命中文件 574 / 命中行 7,695（`grep -c` 计行不计次）/ 标记在 `=` 右侧 4,876、左侧 1,921、
不紧邻 `=` 余 1,109；按目录 `012_VIEW` 293 处·100 文件、`数据拆分PROGRAM--GAUSS` 7,613 处·474 文件、
`sql` 与 `sequence` **0**；单文件最高 213 处。**仍是推算** = 这 7,906 处里落进"不改写分支"的
**比例**（本轮只证明了后果不静默，没证明命中量），归 A0b。

> 顺带订正一条对本仓规则数据的误判：`spinduck/src/sql/patterns.rs:67` 原注释"语料 8 处"
> **不是错数**——它对齐 `config/corpus_forms.toml:54`（`oracle_outer_join_marker = 8`，由
> `scripts/extract_corpus_forms.py` 生成，口径是**本地考题语料 147 个文件**里
> 正则 `\(\s*\+\s*\)` 的出现次数，全部来自 F140，且被 V21 的两条门禁测试双向绑定）。
> 问题只在**没写清是哪一份语料**。注释已按此改写（本轮改动之一），并显式禁止把"8 处"读成
> "8 处都改写成功"。

### V27.3 交付件画像重测：本节自记的两个数被推翻（design §4）

| 项 | 旧值（错） | 新值（已实测） | 错因 |
|---|---|---|---|
| `.sql` 总行数 | 147,618 | **5,628,370** | `find -print0 \| xargs -0 wc -l \| tail -1` 只取到**最后一批**的 total；14,611 个文件必然分批调用 `wc`，每批各打一行 total。正确口径是把所有批次求和（design 附录 A 已给 awk 版）。python 逐文件数 `\n` 交叉验证同一结果 |
| 源码编码 | "GBK" | **混合**：UTF-8 可解 **11,462** / 需 GBK **3,149** / 两者都不通 **0** | 旧值取自 `file -I` 对个别文件的判断（GBK 文件若无中文行则为纯 ASCII，`file` 会报 us-ascii）。逐文件 `decode('utf-8')` 失败再试 `decode('gbk')` 的全量口径才有意义。需 GBK 的分布：`数据拆分PROGRAM--GAUSS` 2,710/2,947、`012_VIEW` 395/1,210、`sequence` 44/1,188、`sql` **0**/9,266 |

新增一条与正确性直接相关的画像（**已实测**）：3,149 个需 GBK 的文件里，**2,384 个（75.7%）**
在**单引号字面量内**出现 CJK，合计 **182,723 处**；仅注释含 CJK 的 761 个。这条是 V27.4 的
风险体量来源——有损解码污染的**不只是注释**。

### V27.4 非 UTF-8 源码：三条入口三种行为，只有 `call` 是静默的（design §3.5，R9/D9）

| 入口 | 读源码方式 | 实测行为 | rc |
|---|---|---|---|
| `compile <path>` | `read_source`（`spinduck/src/cli/mod.rs:45`，有损解码）+ **保留**标记 | `OK … routines=1  (non-utf8)` | 0 |
| `call --file <sql>` | 同上，但 `spinduck/src/cli/run.rs:244` 写成 `let (text, _)` **丢弃**标记 | 无任何提示 | 0 |
| `verify`（→ `deploy_*`） | `std::fs::read_to_string`（`spinduck/src/verify/deploy.rs:68,81,97,108`） | `[ENV-0030] 读取 …: stream did not contain valid UTF-8` | 1 |

静默那条的实证（自造最小夹具，**不取交付件内容**：同一份源码写两份，一份 GBK 字节、一份
UTF-8 字节；函数体 `IF p_i_flag = '已成交' THEN 'MATCH' ELSE 'NO_MATCH'`；参数一律 UTF-8
`'已成交'`）：

```text
GBK  源 + UTF-8 参数 → RETURN NO_MATCH   rc=0
UTF-8 源 + UTF-8 参数 → RETURN MATCH      rc=0
```

**同源码、同参数，只差文件编码 → 结论相反且两条都绿。** 有损解码把字面量里的中文换成
U+FFFD，比较条件永不成立。按 V27.3 的体量，交付件里 2,384 个文件在射程内。
处置顺序记在 design D9：**先让 `call` 变响**（零依赖，把 `non_utf8` 透出并判红，新增错误码挂
探针），真 GBK 解码（新依赖 = Ask-first）另批。守护测试随 A2 落，本轮**未**改行为——A0a 只补证据。

### V27.5 能力面改判：`verify` 已具备"建表 + 缺件红"（design §3.1、R3、§6.2）

设计文档初稿把"执行 DDL 的部署"整体判为不存在，与 V20 自己的登记冲突。本轮实跑复核
（**已实测**，用的就是发版二进制）：

```text
$ cd golden/case_corpus_purchase && ../../spinduck/target/release/spinduck verify .
manifest: ./.spinduck/conf/spinduck.toml (updated)
== entry pkg_split_trade_step3_sh.prc_trd_deal_purchase
  PASS Entry … / PASS Routine pkg_split_trade_log.inst_log …
  PASS Relation dat_trd_repurchase / mid_yjqs_detail / par_fund_partner / par_fund_seat /
         par_fund_seat_para / par_sys_purchase / swh_all_kind / sys_dummy
  SKIP Scenario - — 无场景声明该入口；2 条 [[case.assert]] 已由用例 corpus_purchase 覆盖…
verify: entries=1 checks=11 failed=0 skipped=1        rc=0
```

即：`cli` feature 含 `native`，发版产物**能**跑完整部署（表 DDL → 视图 → `sys_dummy` → 桩与
例程 → 种子）与闭包核验，缺件红由 `tests/verify_chain.rs:245`
`should_render_reports_and_exit_1_when_chain_has_gaps` 锁着。差距因此**重述**为四条（design §3.1
表）：库开在内存（`spinduck/src/verify/deploy.rs:28`）→ 交不出可被 `call` 打开的 `--db`；
资产按目录发现且 `sql_files` **忽略递归参数**（`spinduck/src/project/fs.rs:49`）→ 深层碎目录扫不到、
也表达不了闭包子集；`[[entry]]` 要求包限定（`spinduck/src/project/entry.rs:50`，实测裸函数名报
`入口/例程全名必须为 package.procedure 形态`）→ 独立函数（交付件 2,148 个）进不了门禁；
`call` 自己那条路**没有**部署（V27.1）。

### V27.6 仍是推算的（明确不声称）

- **千万级取数/回写的任何时长与内存数字**：本轮一个都没测。`fetch_rows` 收全 `Vec` +
  逐行字面量 `INSERT`（`spinduck/src/io/gauss.rs:88,111`）与逐行 `execute` 无事务（`:142`）
  是**代码事实**，"千万行会 OOM / 千万次往返"是**推算**。
- **真实实例的握手面**：`sslmode`、`datcompatibility`、`server_encoding`/`client_encoding`、
  目标表行数与列宽——全部未连（附录 B 五条待确认项在 design）。
- **`MAX_TUPLES = 100_000` 在真实闭包上的突破命中数**、`(+)` 7,906 处里不改写分支的**占比**、
  闭包例程在 `register` 上的**编译通过率**——均属 A0b。
- **codeweb 图谱类数字**（43,432 节点 / 286,311 边 / 292 条 high 冲突 / 单过程闭包 4 函数 + 14 表）
  取自前一轮同机会话，本轮**未重跑** codeweb；口径与命令在 design 附录 A，复跑属 A0b 的
  计划生成部分。

### V27.7 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `docs/design-production-gaussdb.md` | v3 → **v4**：§1 摘要两行改判、§3.1 重写、§3.3 三形态、新增 §3.5、§4 表 + 硬事实（三→四条）、R3/R7/R9 判定口径、新增 D9、§6.2 deploy/plan 契约重写、§7 拆 A0a/A0b、§8 三行状态更新、§9 DoD #2/#5、附录 A 命令与附录 B 第 5 条 | 仅设计文档 |
| `spinduck/src/sql/patterns.rs` | 两处注释口径改写（`:67` 命中数口径来源；`:141` 保守边界不再声称"语料只出现适配形态"） | 注释，零行为变更 |
| `spinduck/tests/guard/guard_patterns_outer_join_residual.rs` | 新增守护 3 条（V27.2 的实证来源） | 新增测试 |
| `spinduck/Cargo.toml` | 注册上述 `[[test]]` | 测试目标登记 |

未做且**不该由本轮做**的：没碰 golden 夹具与断言、没改任何人类已有测试、没动
`fetch_many(max+1)` 的超限形态（design D7 明确归 A6）、没引入 GBK 解码依赖（D9② = Ask-first）、
没连任何外部实例。

---

## V28. CLI 契约层落地：AGENTS §4.1 登记的 `tests/cli.rs` 从"规范里有、仓库里没有"变成 27 条真子进程用例

范围：只新增测试层与两处文档口径，**零引擎行为变更**（`src/` 一行未动）。

被测与构建口径（两条都跑过，文本一致）：

- 自动化跑的是 `cargo test --features cli --test cli` 由 cargo 现建的 debug 二进制
  （`env!("CARGO_BIN_EXE_spinduck")`），feature 口径 = 发版矩阵那条（`release.yml` 的
  `--features cli`）。
- 手工交叉印证用的是本机 release 0.1.2（`--features cli --locked`）：`version` 打印
  `spinduck 0.1.2`；V28.2 的形态 C 仍 `RETURN 0` + rc=0；`materialize` 仍
  ``[ENV-0012] ... requires feature `gauss` `` + rc=1，且**没有**生成 `--db` 指向的本地库文件。

门禁（本机）：

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features --tests -- -D warnings` | 干净（`--all-features` 含 `cli`，本层被 lint 到） |
| `cargo test --features cli` | **323 passed / 0 failed**（= native 的 296 + 本层 27） |
| `cargo test --features native` | **296 passed / 0 failed**——与 V27 基线逐条相等，本层不参与 |
| `cargo test --features cli,gauss --test cli` | **27 passed / 0 failed**（`gauss_boundary` 两条换成 ENV-0014 形态） |
| `cargo audit` | 4 条，全部 `zip 6.0.0`，与 V27 同一豁免清单，无新增 |

### V28.1 这一层独有的是什么（为什么不是把 `src/cli/run.rs` 单测抄一遍）

`src/cli/run.rs` 的 `run_str` **进程内**调 `run()` 拿返回码，它证明不到两件事：

1. 进程真正观察到的退出码（`src/bin/spinduck.rs` 的 `std::process::exit(code)` 这一步）；
2. "全部输出走 stdout、stderr 恒空"这条渲染纪律（`should_write_contracts_to_stdout_and_leave_stderr_empty`）。

其余 25 条把子命令逐个的 stdout 文本钉成契约（`version` / 无参 usage / 三类用法错误 /
`compile` 的 OK 行 + `FAIL` 行 + summary + `reason` 直方图 + `(non-utf8)` 标记 /
`rules probe` 的 match=true|false 与缺文件 / `explain` 的逐字节 IR 行与 BIZ-0150 /
`call` 的 `OUT p_o = 42`、`CALL p_null OK` 与 `[BIZ-0150]`/`[ENV-0010]` 两条红面，
详见 V28.2）。断言一律给**整行或全文**
（`assert_eq!(out, "OUT p_o = 42\n")` 这一类），不用 `is_ok()`/`contains("error")` 式的宽断言。

### V28.2 把 V27 的三条"取证结论"变成可复现回归

| 用例 | 锁定的契约 | V27 出处 |
|---|---|---|
| `call::should_ignore_ddl_and_seed_files_given_to_call` | 形态 **B ≡ A**：`stdout` 与退出码**逐字节相等**（`[KERNEL-0010] … fnc_com_if_deal_date does not exist!`，rc=1）——建表脚本经 `--file` 递进去完全无效 | V27.1 结论 1 |
| `call::should_exit_0_returning_0_when_deps_registered_but_tables_missing` | 形态 **C**：`RETURN 0` 且 **rc=0**（缺表却绿）。`tests/guard/guard_register_silent_drop.rs` 只锁到 register 层，端到端的静默形态此前无 CLI 级裁决 | V27.1 结论 2/3 |
| `call::should_distort_non_utf8_literals_on_call_without_any_warning` | GBK 源码走 `call` → 每个非法字节一个 U+FFFD、**无任何告警**、rc=0；同一段体的 UTF-8 写法得 `OUT p_o = 中文`——仅编码不同即两值 | V27.4 |
| `compile::should_mark_non_utf8_corpus_in_compile_report` + `verify::should_exit_1_with_env_0030_when_corpus_is_not_utf8` | 编码三入口的另外两种行为：compile 打 `(non-utf8)` 仍 OK，verify 直接红 `[ENV-0030] 读取 … stream did not contain valid UTF-8` | V27.4 |
| `gauss_boundary::*`（两条 feature 门禁 + 两条"本地库文件未被创建"） | release feature 口径下 `materialize`/`sync` 的失败点：ENV-0012「… requires feature `gauss`」（未开）/ ENV-0014「SPINDUCK_OG_HOST is required」（开了但零环境变量） | design §1 第一行、R1 |
| `verify::should_exit_0_and_write_runtime_manifest_for_clean_project` | `verify` 的渲染契约：`manifest: … (updated)` + `verify: entries=1 checks=2 failed=0 skipped=1`，rc=0 | V27.5 |

**两条期望值是"当前缺陷的快照"，改它们必须与行为改动同一个 PR**：
形态 C（A2/D5 缺对象即红时转红）与 GBK 静默变形（D9② 真解码落地时转正确值）。
用例注释里已就地写明，防止日后单点放宽断言。

> **前向注记（V29）**：GBK 那条已在 V29 与行为**同批**改写，但改它的是 **D9① 判红**（rc=1），
> 不是本节当时预期的 D9② 真解码（转正确值）——"同一个 PR"这条纪律兑现了，预期到的形态没有。
> 形态 C 未动，仍锁 `RETURN 0` + rc=0，等 A2-4。
> 另外本节把编码说成"三条入口"是少算了 `explain --file`，V29.2 已订正。

### V28.3 落点、feature 门禁与由此留下的 CI 口径

- `spinduck/tests/cli.rs`：模块根（共享夹具 + 真子进程接缝 `cli()` + `write_gbk_routine()`），
  按职责拆 6 个子模块（AGENTS §3 单文件 ≤500 行；拆分后最大 147 行、总计 613 行）：
  `tests/cli/{usage,compile,rules_explain,call,verify,gauss_boundary}.rs`。
  crate 根的 `mod` 默认在同目录查找，故用 `#[path = "cli/…"]` 显式指向子目录。
- `spinduck/Cargo.toml`：注册 `[[test]] name = "cli"` 带 `required-features = ["cli"]`。
  选 `required-features` 而不是 `#![cfg(feature = "cli")]`：前者在 feature 没开时**不构建该目标**，
  后者会收集 0 条测试——正是 AGENTS §4.1 禁止的"跳过即绿"形态。
- **由此产生的口径，明确不声称**：常驻 CI 的 Test job 跑 `cargo test --features native`，
  **不会**构建 `[[test]] cli`。所以 V28 的 27 条目前只有本机证据；`--features cli` 进入常驻
  门禁属改 `.github/workflows/ci.yml`（Ask-first），留给用户决策。本机复现命令：
  `cargo test --features cli`（323 passed）或 `cargo test --features cli --test cli`（27 passed）。
  > **前向注记（V29）**：用户随后授权改 CI，Test job 已加 `cargo test --features cli` 一步
  > （V29.4），本条"不进常驻门禁"不再成立；但 GitHub 侧尚未跑过，这一步目前也只有本机证据。
  > V29 后本层是 29 条，两个口径变成 325 / 296。
- 黄金资产只读：形态 A/B/C 三条用 `golden/case_corpus_purchase` 的现成文件做输入，
  未新增 golden 用例、未改夹具、未改 `expected.json`（design §6.5 的承诺不变）。

### V28.4 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/tests/cli.rs` | 新增 CLI 契约层模块根（135 行） | 新增测试 |
| `spinduck/tests/cli/*.rs` | 新增 6 个职责子模块，共 27 条用例 | 新增测试 |
| `spinduck/Cargo.toml` | 注册 `[[test]] cli` + `required-features = ["cli"]` | 测试目标登记 |
| `docs/design-production-gaussdb.md` | §6.5 的"落点与规范不一致"注记改为"已补齐 + 留下的 CI 口径一条"；顺带订正原注记把 `run_str` 说成"起子进程"的错定性 | 仅设计文档 |
| `docs/validation.md` | 本节 V28 | 实证登记 |

未做：没改 `src/` 任何行为、没改人类已有测试与断言、没动 `ci.yml`、没连任何外部实例、
没有把 release 二进制纳入自动化（`CARGO_BIN_EXE_*` 指 cargo 现建的构建；release 侧只有
V28 前言里的手工交叉印证三条）。

---

## V29. A2-D9① 落地：执行面见非 UTF-8 判红（`call`/`explain` 两条入口），并订正 V27.4 的"三条入口"少算了一条

范围：只改编码判红的执行面与它的回归面。design §7 A2 的其余三项（`plan`、`deploy`、缺件红扩到
`call`）**未动**，形态 C 仍是 `RETURN 0` + rc=0。改动落在 CLI 壳与真子进程契约层，零新依赖。

### V29.1 TDD 循环（Red 原文 → Green → 判红后实测）

**Red**（先只改测试，`src/` 一行未动）：

```text
test call::should_exit_1_with_env_0030_when_call_source_is_not_utf8 ... FAILED
  assertion `left == right` failed: OUT p_o = <4 个 U+FFFD>            left: 0  right: 1
test call::should_explain_utf8_literal_and_reject_the_gbk_sibling ... FAILED
  assertion `left == right` failed: 0<TAB>assign<TAB>p_o := '<4 个 U+FFFD>'   left: 0  right: 1
test result: FAILED. 26 passed; 2 failed
```

**Green**：`spinduck/src/cli/mod.rs` 新增 `read_source_strict()`（字节读失败仍是 `[ENV-0010] read …`，
UTF-8 校验失败 → `[ENV-0030] 读取 {path}: stream did not contain valid UTF-8`），两个执行面入口改用它：
`explain_cmd`（`spinduck/src/cli/run.rs:188`）与 `call_cmd` 的 `--file` 循环（`spinduck/src/cli/run.rs:244`）。
`call_cmd` 里判红发生在 `eng.register` **之前**，因此红的那次连一次内核调用都没发出，`OUT`/`RETURN`
一行不产出。体检面 `compile` 保留原样（有损解码 + `(non-utf8)` 标记）——语料批量体检要的是
"扫完全部文件并按错误码分类"，不是"停在第一个编码问题"。

**没有新增错误码**：复用 `verify`/`deploy` 已在用的 `ENV-0030`，消息文本逐字符对齐
`std::fs::read_to_string` 的 io 错误。AGENTS §5「新增错误码必须挂探针」因此不触发；入口一致这条由
下面表里的契约用例锁住，而不是靠新增探针条目。

**Refactor**：无。两处调用点替换 + 一个 helper，未触碰其它路径。

### V29.2 订正 V27.4：受影响的入口是**四条**，不是三条

V27.4 的表只列了 `compile` / `call` / `verify`，漏了 `explain --file`。它在改动前和 `call` 走同一处
丢弃标记（`run.rs:188` 的 `let (text, _)`），因此同样**静默**。Red 阶段的失败原文即一手证据：

```text
$ spinduck explain --file gbk.sql prc_gbk      # 改动前
0<TAB>assign<TAB>p_o := '<4 个 U+FFFD>'        rc=0     ← 无任何告警
```

改动前的真实分布是四条入口两种行为：**执行面**（`call`、`explain`）静默有损；**核验面**里
`compile` 有损 + 打标记、`verify` 直接红。V27.4 的结论方向不变（`call` 的静默变形成立），只是射程
多一条。design §3.5 的表已按此订正并标注落地状态。

### V29.3 判红后的四条契约（实测 stdout / 退出码）

| 用例（`spinduck/tests/cli/`） | 实测 |
|---|---|
| `call::should_exit_1_with_env_0030_when_call_source_is_not_utf8` | `[ENV-0030] 读取 <path>: stream did not contain valid UTF-8`，rc=1；另断言不出现 `OUT `、不出现 `中文`；同段体 UTF-8 写法仍 `OUT p_o = 中文` rc=0 |
| `rules_explain::should_exit_1_with_env_0030_when_explain_source_is_not_utf8` | 同上形状 rc=1，并断言不产出任何 IR 行 |
| `rules_explain::should_explain_utf8_literal_verbatim` | `0<TAB>assign<TAB>p_o := '中文'` rc=0——红的是编码，不是中文字面量 |
| `compile::should_mark_non_utf8_corpus_in_compile_report`（本轮未改） | `… routines=1  (non-utf8)` rc=0——体检面口径不变 |

MATCH/NO_MATCH 分叉（V27.4 最警惕的那条）在实测层面消失：同一份源码现在只可能「UTF-8 → 正常返回
值」或「非 UTF-8 → 判红」，不可能因编码不同返回两个不同的值。

真二进制交叉印证（本机 release 重新构建，`cargo build --release --features cli --locked`；
夹具落在 `/tmp/sd-v28/`，GBK/UTF-8 **同内容两文件**四份，**不含任何交付件内容**；命令行为原文）：

```text
$ ./target/release/spinduck call --file /tmp/sd-v28/gbk.sql prc_gbk null
[ENV-0030] 读取 /tmp/sd-v28/gbk.sql: stream did not contain valid UTF-8        rc=1
$ ./target/release/spinduck explain --file /tmp/sd-v28/gbk.sql prc_gbk
[ENV-0030] 读取 /tmp/sd-v28/gbk.sql: stream did not contain valid UTF-8        rc=1
$ ./target/release/spinduck call --file /tmp/sd-v28/utf8.sql prc_gbk null
OUT p_o = 中文                                                                rc=0
$ ./target/release/spinduck call --file /tmp/sd-v28/f_gbk.sql f_chk '已成交'
[ENV-0030] 读取 /tmp/sd-v28/f_gbk.sql: stream did not contain valid UTF-8        rc=1
$ ./target/release/spinduck call --file /tmp/sd-v28/f_utf8.sql f_chk '已成交'
RETURN MATCH                                                                rc=0
$ ./target/release/spinduck compile /tmp/sd-v28
OK   /tmp/sd-v28/f_gbk.sql  routines=1  (non-utf8)
OK   /tmp/sd-v28/f_utf8.sql  routines=1
OK   /tmp/sd-v28/gbk.sql  routines=1  (non-utf8)
OK   /tmp/sd-v28/utf8.sql  routines=1
summary: files=4 ok=4 failed=0 routines=4                             rc=0
```

（`f_chk` 的实参 `'已成交'` 一律 UTF-8 传入，正是 V27.4 那对分叉的输入形态。）

V27.4 那对 `MATCH` / `NO_MATCH` 现在换成 **`MATCH`（rc=0）/ 判红（rc=1）**，体检面逐字未变。

### V29.4 门禁（本机）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features --tests -- -D warnings` | 干净 |
| `cargo test --features cli --test cli` | **29 passed / 0 failed**（V28 是 27：+2 新用例，其中 V28 那条 `should_distort_non_utf8_literals_on_call_without_any_warning` 按 D9① 改写为判红形态） |
| `cargo test --features cli` | **325 passed / 0 failed**（= native 的 296 + 本层 29） |
| `cargo test --features native` | **296 passed / 0 failed**——与 V27/V28 基线逐条相等，执行面改动未波及内核通道 |
| `cargo test --features cli,gauss --test cli` | **29 passed / 0 failed**——`gauss` 只换 `gauss_boundary` 两条的失败点（ENV-0012 → ENV-0014），编码判红与 feature 无关，这条是实测不是推断 |
| `cargo test --features cli,gauss`（全量） | **2 条 FAILED**，都在 `tests/gauss_e2e.rs`（`gauss_e2e_materialize_with_where_clause`、`gauss_e2e_materialize_process_sync_back`），失败原文是"E2E 用例需要 A 模式容器（golden/gaussdb-docker）"。本机未起容器，改动前后同形态，与本轮无关 |
| `cargo audit` | **本轮未复跑成功**：拉取 advisory-db 网络失败（`error sending request for url …/git-upload-pack`）。本轮零新增依赖，命中面与 V28 登记的 4 条 `zip 6.0.0` 不可能变化——这是**推断**，不是实测 |

契约层由此**首次成为一次行为改动的 Red/Green 载体**（V28 只登记现状）。V28.2 声明"两条期望值是
当前缺陷的快照，改它们必须与行为改动同一个 PR"——本轮正是那条 GBK 用例与行为**同批**改写；形态 C
那条未动（等 A2-4）。

### V29.5 仍是推算的 / 明确未做

- **D9② 真 GBK 解码**：未做（新依赖 = Ask-first）。判红后的现实口径是：V27.3 数出的 3,149 个 GBK
  文件在 `call`/`explain` 上**一律红**，跑不起来。这是设计要的"不假装支持"，不是"已支持"。
- **A2 其余三项**：`plan` / `deploy` / 缺件红扩到 `call` 均未动；design §7 A2 的三条退出判据只满足
  第三条（GBK 源经 `call` → 非 0）。形态 C（依赖齐全但缺表 → `RETURN 0` + rc=0）在
  `cargo test --features cli --test cli` 里仍以原期望通过，即**缺件静默绿这条路今天依然可达**。
- **CI 里新增的 `cargo test --features cli` 这步没有 GitHub 证据**：本轮只改了 workflow 定义
  （V29.4 表里的数字全部来自本机）。AGENTS §9 的 Test job 描述（只写 `--features native`）与
  workflow 现状由此不一致，规范正文是否同步属用户决策，本轮**未改 AGENTS.md**。
- **判红按文件、不按行**：`read_source_strict` 对整个文件做 UTF-8 校验，因此只有注释是非 UTF-8
  的 761 个文件（V27.3）同样红。这条是代码事实；"这会不会让本可运行的语料一起出局"属推算，
  要等真实语料跑一遍才有数字。
- **千万级数据、真实实例、回写**：一个都没碰，本轮全部在本机临时目录 + `golden/` 只读夹具上完成。

### V29.6 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/cli/mod.rs` | 新增 `read_source_strict()`（+19 行）；`read_source()` 原样保留给体检面 | 行为改动（执行面入口） |
| `spinduck/src/cli/run.rs` | `explain_cmd`（`:188`）与 `call_cmd` 的 `--file` 循环（`:244`）改调严格读取 | 行为改动（两处调用点） |
| `spinduck/tests/cli/call.rs` | V28 的 GBK 静默变形用例改写为判红契约 `should_exit_1_with_env_0030_when_call_source_is_not_utf8` | 测试期望值与行为**同批**改 |
| `spinduck/tests/cli/rules_explain.rs` | 新增 `should_exit_1_with_env_0030_when_explain_source_is_not_utf8` + `should_explain_utf8_literal_verbatim` | 新增测试 |
| `spinduck/tests/cli.rs` | 头注的实证出处补 V29；`call` 子模块标题"编码丢失"→"编码判红" | 注释 |
| `.github/workflows/ci.yml` | Test job 在 `--features native` 之后加 `cargo test --features cli`（用户本轮授权） | CI 定义 |
| `docs/validation.md` | 本节 V29 + V28.2/V28.3 的两处前向注记 | 实证登记 |
| `docs/design-production-gaussdb.md` | §1 编码行、§3.5（四条入口 + 落地状态）、§5.2 D9（①已落地的两处出入）、§7 A2 行、§8 两行、附录 A 复现块 | 仅设计文档 |

Red 阶段那条 explain 用例起初写在 `tests/cli/call.rs`（V29.1 的失败原文即其当时的名字
`call::should_explain_utf8_literal_and_reject_the_gbk_sibling`），Green 时按职责移到
`tests/cli/rules_explain.rs` 并改名，断言未变——表里 V29.3 记的是移动后的名字。

## V30. A2-`plan` 落地：闭包 → 逐字节可复现的计划文件；真实语料把它逼出的三处抽取口径缺陷一并修掉

范围：design §6.1 的 `run-plan` schema + `plan` 子命令（A2 的第二项，D9① 已在 V29）。
`deploy`（A2-3）与缺件红扩到 `call`（A2-4，形态 C）**本轮未动**。零新增依赖、零新错误码
（目标未登记复用 `BIZ-0150`），schema 正文入 architecture §13.5。

### V30.1 交付面

| 件 | 落点 | 职责 |
|---|---|---|
| 计划 schema + 渲染 | `spinduck/src/plan/mod.rs` | `[plan]` → `[sources]` → `[[missing]]` 手工渲染；排序去重、段序固定、不写日期/绝对路径 |
| 闭包求值 | `spinduck/src/plan/closure.rs` | 目标例程 → BFS（链路对象 + 被调例程）→ 文件清单 + 缺件表；只编译不下内核（`NoopExecutor`） |
| `plan` 子命令 | `spinduck/src/cli/plan.rs`、`args.rs`、`run.rs` | 落盘 `<home>/plan/<target>.toml`，缺件即 rc=1 并照抄 `MISS <object> <- <from>` |
| CLI 契约 | `spinduck/tests/cli/plan.rs`（6 条，本层 29→35） | 真子进程锁退出码 + stdout + 计划文件字节 |
| 抽取口径修复 | `src/verify/chain/`（V31 起为目录）、`src/verify/run.rs`、`src/verify/deploy.rs` | 见 V30.4（三条都是 `verify` 现路径上的缺陷，不在新代码里绕行） |

口径纪律：链路抽取、名字归一、包补全、尾段匹配**逐字复用 `verify::chain` / `verify::run` 的纯函数**，
计划与核验不两套真相。两处刻意保留的"孪生实现"（`closure::resolve_routine` ↔
`run::resolve_routine`、`closure::ddl_file_for` ↔ `run::present`）只因容器类型不同（`Engine` 目录 /
`BTreeSet<ObjectRef>`），规则改动必须同步另一处——注释里已写明。

### V30.2 TDD 循环与 Red 留档口径

本阶段 8 个循环（跨两次会话，前 4 项在上一段会话落地），**只有下表标"一手"的失败原文是本次逐字截留的**；
循环 1–4 的 Red 在各自循环内跑过并转绿，原文未逐条留档，此处只登记用例名与终态计数——按本文件
"每条数字都要能被复查"的口径，这类只到"跑过"级别的项标为**二手**。

| 循环 | 行为 | 用例 | Red |
|---|---|---|---|
| 1 | schema 渲染与字节可复现 | `plan::tests` 前 4 条（`should_render_*`、`should_omit_schema_key_*`） | 二手 |
| 2 | 闭包 BFS：文件清单 + 三类缺件 | `should_collect_routine_and_ddl_files_transitively_into_the_plan` 等 5 条 | 二手 |
| 3 | `plan` 子命令契约（rc / stdout / 落盘） | `tests/cli/plan.rs` 6 条 | 二手 |
| 4 | 改写后形态：`nextval('seq')`、`bigfund . t_a` | `should_collect_sequences_from_the_rewritten_nextval_form`、`should_join_qualified_names_printed_with_spaces_around_the_dot` | 二手（left/right 记录在本轮会话的循环日志里，非本文件留档） |
| 5 | 同包裸名调用不是缺件 | `should_resolve_a_bare_sibling_call_inside_the_same_package` | 由真实语料逼出（V30.3 的三条 `MISS routine:` 原文即一手输入） |
| 6 | 注释不参与词法扫描 | `should_ignore_names_inside_sql_comments`、`should_match_a_view_declared_behind_an_inline_comment` | **一手**（V30.4 ①） |
| 7 | 引擎自建对象不算缺件 | `should_not_report_an_engine_provided_table_as_missing` | **一手**（V30.4 ②） |
| 8 | 内核目录名归一小写 | `should_lowercase_kernel_catalog_names` | **一手**（V30.4 ③） |

终态计数：`--features native` **315 passed**（V29 基线 296 + 本轮 19：`plan::tests` 13 + `verify::chain`
3 + `verify::run` 1 + `cli/args` 2）、`--features cli` **350 passed**（= 315 + 契约层 35）、
CLI 契约层 **35 passed**。

### V30.3 真实语料：计划把三条假缺件逼了出来

考题语料（`golden/exam_split_trade`，136 文件只读夹具）改动前实测（循环 5 之前）：

```text
$ spinduck plan --dir golden/exam_split_trade --schema bigfund pkg_split_trade_step2.prc_pre_deal_shjycf
plan: target=pkg_split_trade_step2.prc_pre_deal_shjycf routines=4 tables=8 missing=3
MISS routine:prc_pre_deal_shgh_jy <- pkg_split_trade_step2.prc_pre_deal_shjycf
MISS routine:prc_pre_deal_shgh_qfii_dealer <- pkg_split_trade_step2.prc_pre_deal_shjycf
MISS routine:prc_pre_deal_shgh_zyk <- pkg_split_trade_step2.prc_pre_deal_shjycf
```

三条都是**假缺件**：`PKG_SPLIT_TRADE_STEP2.sql:357` 起的同包兄弟过程按裸名调用（
`prc_pre_deal_shgh_jy(p_i_date, v_last_date, p_o_succeed);`），而闭包只做了全名查目录。
`verify` 早有 `resolve_routine` 的补全规则，闭包没复用——这正是"两套真相"的代价，本轮按口径统一修掉。

循环 5–8 全部落地后，10 条 `[[entry]]` 逐条重跑（命令同上一条，`--schema bigfund`）：

```text
plan: target=pkg_split_trade_step2.prc_pre_deal_shjycf            routines=4  tables=21  missing=0
plan: target=pkg_split_trade_step2.prc_pre_deal_bjgsyh            routines=7  tables=12  missing=0
plan: target=pkg_split_trade_step3_sh.prc_trd_hz_byfund           routines=7  tables=15  missing=0
plan: target=pkg_split_trade_step3_sh.prc_trd_deal_purchase       routines=2  tables=7   missing=0
plan: target=pkg_split_trade_step5.prc_split_sh_cjqs              routines=7  tables=18  missing=0
plan: target=pkg_split_qq.prc_general_deal_inst_sh_buy            routines=3  tables=14  missing=0
plan: target=pkg_split_qq.prc_general_deal_inst_sh_sell           routines=3  tables=14  missing=0
plan: target=pkg_split_qq.prc_general_deal_inst_sh_other          routines=3  tables=14  missing=0
plan: target=pkg_inst_domestic_purchase.prc_gen_inst              routines=4  tables=18  missing=0
plan: target=pkg_split_trade_step9.prc_deal_bond_repurchase_inst  routines=3  tables=22  missing=0
```

design §7 A2 第一条退出判据（§4 式闭包的 `[[missing]]` 为空）由此在**真实 136 文件语料**上成立。
字节可复现同语料实测（连续两次 `plan`，`cmp` 逐字节相同，stdout 两次同为 `(unchanged)`）：

```text
$ ./target/debug/spinduck plan --dir golden/exam_split_trade --schema bigfund pkg_split_trade_step2.prc_pre_deal_shjycf
plan: …/.spinduck/plan/pkg_split_trade_step2.prc_pre_deal_shjycf.toml (unchanged)
plan: target=pkg_split_trade_step2.prc_pre_deal_shjycf routines=4 tables=21 missing=0
$ cmp plan1.toml plan2.toml && echo IDENTICAL
IDENTICAL     （1,186 字节，sha256 219038d91f11ba4bf8f90dc1eb20fa30e4c81d05aa862760cd476060c023f9ae）
```

### V30.4 三处抽取口径缺陷（一手证据）

三条都不是"新代码没写好"，而是 `verify` 现路径上已经存在、被闭包第一次完整暴露出来的缺陷。
每条先写会失败的测试再改实现；② ③ 会**改变 `verify` 的判定**，逐条列在结果里。

**① 词法扫描不认注释**（`chain::tokens`）——V1 早就记过"视图含 `/*FORCE*/` hint"，但抽取器与
DDL 扫描都不剥注释，于是 `CREATE OR REPLACE /*FORCE*/ VIEW V_PAR_BOND` 整条声明看不见，
`-- from t_comment` 里的表名反倒算一次引用。Red 原文：

```text
test verify::chain::tests::should_ignore_names_inside_sql_comments ... FAILED
  assertion `left == right` failed
    left: ["Relation:t_comment", "Relation:t_real"]   right: ["Relation:t_real"]
test plan::tests::should_match_a_view_declared_behind_an_inline_comment ... FAILED
  注释后的视图声明要能登记: [MissingObject { object: "relation:v_par_bond", from: "prc_main" }]
```

改法：`tokens()` 在字符串分支之后加行注释（`--` 到行尾）与块注释（`/*` 到 `*/`，未闭合即到文末）。
`sequence_calls()` 仍在原文上取（序列名只活在引号内，剥注释会把它一起弄丢），代价是注释里被注释掉的
`nextval('…')` 也算一次引用——本仓库语料实测无此形态：

```text
$ grep -rn "^\s*--.*\(nextval\|currval\)" golden/exam_split_trade/sql/{routines,stubs}   # 无输出
$ grep -rn "/\*.*nextval" golden/exam_split_trade/sql/routines                            # 无输出
```

**② 引擎自建对象没有源文件**——`sys_dummy` 由 `deploy_schema` 建（`src/verify/deploy.rs:91`），项目里
不存在它的 DDL 文件，静态闭包按"无声明即缺件"就会对每个用它做常量取数的过程报假缺件（考题语料 4 条
entry 各报一次）。Red 原文：

```text
test plan::tests::should_not_report_an_engine_provided_table_as_missing ... FAILED
  内核自建对象不该落缺件: [MissingObject { object: "relation:sys_dummy", from: "prc_main" }]
```

改法：`deploy.rs` 新增 `ENGINE_PROVIDED_TABLES`（现为 `["sys_dummy"]`），建表语句按它生成，闭包读
同一份清单——既不进 `ddl_tables`（没有源文件）也不进 `missing`。项目自己声明了同名对象时，DDL 索引
优先，不改变既有行为。

**③ 内核目录名不归一**（`kernel_objects`）——链路名一律小写，内核侧却按书写保留大小写，于是 DDL 用
大写声明的对象**永远**匹配不上。这条只在内核侧存在，静态闭包看不见，故用最小夹具 + 内核探针取证
（夹具落 `/tmp/vprobe`，**不含任何交付件内容**，视图 DDL 取自语料逐字副本，用完即删）：

```text
# 无基表 → 视图体下发失败 → 走占位视图（小写名）→ PASS
$ spinduck verify /tmp/vprobe
  PASS Relation v_dat_inst_base_info — 内核存在（main.v_dat_inst_base_info）

# 补上基表 → 视图体原样下发成功 → 内核里是 main.V_DAT_INST_BASE_INFO → FAIL
$ spinduck verify /tmp/vprobe
  FAIL Relation v_dat_inst_base_info — 链路 SQL 引用，内核目录无此对象（缺 DDL/桩，或部署顺序不符）

# 同一场景的目录原文（临时探针，打印 deploy 下发的 SQL 与 kernel_objects）
--- EXEC ok=true :: CREATE OR REPLACE VIEW V_DAT_INST_BASE_INFO AS SELECT inst_num, …
objects = ["Relation:main.V_DAT_INST_BASE_INFO", "Relation:main.dat_inst_base_info", "Relation:main.sys_dummy"]
```

Red 原文（`FakeExecutor` 脚本化目录行，不触内核）：

```text
test verify::run::tests::should_lowercase_kernel_catalog_names ... FAILED
  内核目录名要归一小写: {ObjectRef { kind: Relation, name: "main.V_X" }}
```

改法：`kernel_objects` 取回的名字 `to_ascii_lowercase()`。DuckDB 的标识符匹配本就不区分大小写，
归一只发生在比较口径上，不改任何下发文本。

**`verify` 判定面的实测变化**（②③ 合起来）：

| 语料 | 改动前 | 改动后 |
|---|---|---|
| `golden/exam_split_trade`（10 entries） | `entries=10 checks=222 failed=2`（两条 `FAIL Relation v_dat_inst_base_info[_s]`） | `entries=10 checks=222 failed=0` |
| `golden/case_corpus_purchase`（1 entry） | `entries=1 checks=11 failed=0 skipped=1` | 逐字相同（该语料无 `/*FORCE*/`、无大写声明、无裸名同包调用，① ② ③ 都不触发） |

考题语料那两条 FAIL 是**误判**（对象确实存在，只是名字大小写不同），修掉后判定面变干净；本轮没有
任何一条 PASS 变成 FAIL。

### V30.5 门禁（本机）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all` + `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features --tests -- -D warnings` | 干净（无 `#[allow]`、无新增 lint 抑制） |
| `cargo test --features native` | **315 passed / 0 failed** |
| `cargo test --features cli` | **350 passed / 0 failed**（含 CLI 契约层 35） |
| `cargo test --features cli,gauss --test cli` | **35 passed / 0 failed**——`plan` 的契约与 `gauss` feature 无关，这条是实测不是推断 |
| `cargo test --features cli,gauss`（全量） | **2 条 FAILED**，仍是 `tests/gauss_e2e.rs` 的两条（失败原文"需要 A 模式容器"），与 V29.4 同形态、本轮无关 |
| `cargo audit` | **本轮复跑成功**（V29 因网络未跑成）：4 条命中 = `RUSTSEC-2026-0367/0368/0369/0429`，与 CI 豁免清单逐条相同；带 `--ignore` 四条后 0 命中（242 crate 依赖）。本轮零新增依赖，命中面不变这条现在有实测 |
| `cargo build --release --features cli --locked` + 真二进制 `plan` | 见 V30.6 |

### V30.6 发版形态二进制复核（A2 判据不要求，本轮顺手取证）

```text
$ cargo build --release --features cli --locked            # exit 0
$ ./target/release/spinduck version
spinduck 0.1.2
$ ./target/release/spinduck plan --dir golden/exam_split_trade --schema bigfund \
      pkg_split_trade_step2.prc_pre_deal_shjycf
plan: …/golden/exam_split_trade/.spinduck/plan/pkg_split_trade_step2.prc_pre_deal_shjycf.toml (unchanged)
plan: target=pkg_split_trade_step2.prc_pre_deal_shjycf routines=4 tables=21 missing=0     rc=0
$ cmp <debug 产物写出的计划> <release 产物写出的计划> && echo IDENTICAL
IDENTICAL
```

发版 feature 组合（`cli`，同 `release.yml` 的构建口径）下闭包与 debug 逐字节一致，
计划里的 `generator = "spinduck 0.1.2 (plan schema 1)"` 与 `spinduck version` 同源（V25 版本守卫那条
口径在 `plan` 侧同样成立）。本轮没有对 release 产物跑 `deploy`——它还不存在（A2-3）。

### V30.7 仍是推算的 / 明确未做

- **`deploy`（A2-3）未动**：计划的消费方还不存在，`ddl_tables` 目前是"给人看的清单"。design §6.2
  要求的三点差异（输入换成计划、库落盘到 `--db`、缺件在 `[[entry]]` 之外也红）一条未落地，
  `src/verify/deploy.rs:34` 的 `open_in_memory` 仍是硬编码。
- **缺件红扩到 `call`（A2-4，D5）未动**：形态 C（依赖齐全但缺表 → `RETURN 0` + rc=0）在
  `cargo test --features cli --test cli` 里仍以原期望通过，静默绿这条路今天依然可达。
- **闭包是静态近似**：不建 AST，故 `EXECUTE IMMEDIATE 'SELECT … FROM t'` 拼出来的动态表名、
  以及只出现在被注释掉的代码里的对象，静态面看不见（后者 ① 修完已明确为"不参与"）。
  `plan` 报 0 缺件不等于内核里必然齐——那是 `deploy`/`verify` 要证明的事，注释与 architecture §13.5 都写了这条边界。
- **孪生实现仍是两处**：`resolve_routine` / `ddl_file_for` 与 `verify::run` 的对应函数只在容器类型上不同，
  本轮没有抽公共接缝（改接缝属 Ask-first）。规则改动必须同步另一处，靠注释与用例锁。
- **`deploy_view` 的视图体路径对 `/*FORCE*/` 同样瞎**（**代码事实**，不是跑出来的数字）：
  `src/verify/deploy.rs:144` 的判定 `up.contains("CREATE VIEW") || up.contains("CREATE OR REPLACE VIEW")`
  在注释插在关键字中间（`CREATE OR REPLACE /*FORCE*/ VIEW`）时两条都不成立，故考题语料 9 个
  `1_V_*.sql` 的循环体一条都不下发，全部落到 `:158` 的**占位视图**（`SELECT 1`）分支。
  本轮只读到这个层面的形态：缺件判定不受影响（对象名仍登记，`verify` 侧 PASS 是实测），
  但"占位视图只有一列 ⇒ 后续读该视图列的语句在内核里取不到列"是推断，要等 A2-3 用计划真跑一次才有实测。
  V30.4 ① 修的是抽取面（`chain::tokens` 与 DDL 扫描），**这条部署面本轮未动**，属 A2-3 的前置项。
  > **V31 已修并实测**：`creates_view()` 改用链路同一把词法刀（V31.1），而"占位视图取不到列"这条
  > 推断也在真题上跑出数字（V31.4 ②：修前 `call` 得 Binder Error，修后 `OUT p_o = 0`）。
  > 同轮还逼出第二条更隐蔽的同类缺陷——逗号连表的第二、三张表对链路隐形（V31.4 ③）。
- **千万级数据、真实实例、回写**：一个都没碰，全部在本机构架 + `golden/` 只读夹具上完成。

### V30.8 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/plan/{mod.rs,closure.rs,tests.rs}` | 新增：schema + 渲染 + 闭包 BFS + 13 条单测 | 新能力（design §6.1） |
| `spinduck/src/cli/plan.rs` | 新增：落盘与渲染，缺件 rc=1 | 新子命令（CLI 仅壳） |
| `spinduck/src/cli/{args.rs,run.rs,mod.rs}` | `Plan` 变体 + 两条 usage + 分发 + `mod plan` | 新子命令接线 |
| `spinduck/src/lib.rs` | `pub mod plan;` | 接线 |
| `spinduck/src/verify/chain.rs`（V31 起拆成 `src/verify/chain/`） | `tokens()` 剥注释；`join_dotted()` 拼回空格拆开的限定名；`sequence_calls()` 取改写后序列形态；3 条新单测；`normalize`/`tokens`/`is_object_name`/`Tok` 放开到 `pub(crate)` | 行为改动（核验面抽取口径） |
| `spinduck/src/verify/run.rs` | `kernel_objects` 目录名归一小写 + 1 条单测 | 行为改动（核验面判定） |
| `spinduck/src/verify/deploy.rs` | `ENGINE_PROVIDED_TABLES` 常量，建表语句按它生成 | 口径收口（同义改写） |
| `spinduck/src/verify.rs` | 导出上述常量与四个 `pub(crate)` 项 | 接线 |
| `spinduck/tests/cli/plan.rs` + `tests/cli.rs` | 6 条契约 + 子模块接线（本层 29→35） | 新增测试 |
| `docs/architecture.md` | §13.5 运行计划 schema（design §6.1 要求的最终口径） | 设计正文 |
| `docs/validation.md` | 本节 V30 + `:722` 表内裸 `\|` 转义 | 实证登记 |
| `docs/design-production-gaussdb.md` | §7 A2 行状态、§8 两条口径订正 | 仅设计文档 |


---

## V31. A2-`deploy` 落地：计划 → 落盘库，换一个进程就能跑真题；同轮把"视图体从不下发"与"逗号连表隐形"两处部署面缺陷修掉

范围：design §6.2 的 `deploy` 子命令（A2 第三项；`plan` 在 V30，缺件红扩到 `call` = A2-4 未动）。
零新增依赖、零新错误码（复用 `ENV-0012`/`ENV-0030`/`ENG-0010`，非幂等那条是内核原样带出的
`KERNEL-0070`）。机制正文入 architecture §13.6。

### V31.1 交付面

| 件 | 落点 | 职责 |
|---|---|---|
| 计划读侧 | `spinduck/src/plan/mod.rs` | `from_toml_str`（只认 `schema_version=1`，不符即 `ENV-0030`）+ `project_root`（`project_dir` 相对**计划文件所在目录**） |
| 计划消费 | `spinduck/src/plan/deploy.rs`（80 行） | `resolve`（清单 → 逐条核对存在的文件，`ddl/views/` 分流到视图组）与 `apply`（逐文件下发）两段分开 |
| CLI 壳 | `spinduck/src/cli/deploy.rs`（73 行） | 读计划 → 缺件即拒 → 定位项目 → 核对文件 → 开库 → 一行汇总。零引擎逻辑（§1） |
| 下发复用 | `spinduck/src/verify/deploy.rs` 的 `deploy_ddl_file`/`deploy_view_file`/`ensure_engine_provided_tables`/`register_routine_files`（`pub(crate)`） | `verify` 与 `deploy` 共用同一套逐文件实现，不建第二套建表口径 |
| 视图检测 | 同上 `creates_view()`（上一段会话，V30.7 那条登记项的修复） | 关键字判定改用链路同一把词法刀 `verify::tokens()`，注释插在 `CREATE OR REPLACE` 与 `VIEW` 中间（`/*FORCE*/`）也认得 |
| 闭包展开视图体 | `spinduck/src/plan/closure.rs` `expand_view_bodies` + `view_sources` | 视图文件进清单后，其视图体引用的表/序列也进清单（视图套视图沿队列展开） |
| 逗号连表抽取 | `spinduck/src/verify/chain/mod.rs` `comma_join_refs`/`subquery_tail_refs`/`matching_paren`/`in_relation_slot` | `FROM a t, b s` 的第二、三位也是关系引用（V31.4 ③） |
| CLI 契约 | `spinduck/tests/cli/deploy.rs`（7 条，本层 35→42） | 真子进程：落盘 + 跨进程可跑 + 三类拒 + 两条视图体形态 |

design §6.2 的三点差异逐条对照（全部为实测，非推算）：

| 差异 | 现状 | 证据 |
|---|---|---|
| 输入是**计划里的文件清单**，非目录发现 | 已落地：`resolve` 只吃 `sources.ddl_routines`/`sources.ddl_tables`，清单外的资产一个都不下发 | `ddl_files=28 routine_files=7`（bjgsyh）与该计划的两条清单条数逐一对应 |
| 库**落盘**到 `--db` | 已落地：CLI 接缝处 `DuckExecutor::open`，`verify` 侧仍 `open_in_memory`，`SqlExecutor` 接缝签名未动 | V31.3 |
| 缺件在 `[[entry]]` 之外也红 | 已落地：`[[missing]]` 非空 → rc=1 且**不开库**；计划列出的文件不存在 → `ENV-0030` 且不开库。两条都不要求项目有 `[[entry]]` | `tests/cli/deploy.rs` 三条 `should_refuse_*`；其中"缺文件"断言 `!db.exists()` |

### V31.2 TDD 循环与 Red 留档口径

沿用 V30.2 的口径：只有下表标"一手"的失败原文是本次逐字截留的。

| 循环 | 行为 | 用例 | Red |
|---|---|---|---|
| 1 | 计划读侧往返（渲染 → 解析 → 逐字段相等）+ 版本/必填字段拒绝 | `plan::tests::should_read_back_*`、`should_reject_a_plan_*`（4 条） | **一手（变异）**：4 条里 2 条首跑即绿，把 `version != SCHEMA_VERSION` 改成 `if false && …` 后 `should_reject_a_plan_from_an_unknown_schema_version` 转红，随即还原 |
| 2 | 落盘库换进程可跑 / 缺件即拒 / 缺文件即拒 / 版本不符即拒 / 缺 `--db` 用法红 | `tests/cli/deploy.rs` 前 5 条 | 上一段会话跑过并转绿，原文未逐字留档 ⇒ **二手** |
| 3 | `/*FORCE*/` 插在关键字中间也要下发视图体 | `should_deploy_the_view_body_when_a_comment_splits_the_create_keywords` | 上一段会话 Red 落在 `call` rc=1 + Binder Error（形态记档，原文未留）⇒ **二手（形态一手）** |
| 4 | 视图体的基表随视图进清单 | `plan::tests::should_include_the_base_table_that_a_planned_view_reads` | **一手**，原文见 V31.4 ② |
| 5 | 逗号连表的每一张表都是关系引用 | `verify::chain::tests::should_take_every_relation_in_a_comma_separated_from_list` | **一手**，原文见 V31.4 ③（这条 Red 出现两次：先断在 `["Relation:t_a"]`，补完第一档后断在子查询形态 `[]`） |
| 6 | 两处修复合起来的进程面 | `should_deploy_a_view_body_whose_comma_joined_base_tables_only_the_plan_knows` | 首跑即绿 ⇒ **补测**，用两次变异证明非永真（原文见 V31.4 ④） |

终态计数：`--features native` **323 passed**（V30 基线 315 + 循环 1 的 4 条 + 循环 4/5 各 1 条 - 其中 4 条在 V30 末已计入，净 +8）、`--features cli` **365 passed**（= 323 + 契约层 42）、CLI 契约层 **42 passed**。

> 说明：V30 记的 315 里已含循环 1 的 4 条读侧用例（同轮落地），所以本轮 native 的净增是
> 循环 4、循环 5 各 1 条 + 上一段会话 `deploy` 侧的 3 条单测。终态以命令输出为准，不靠加减法。

### V31.3 落盘与跨进程（一手）

```text
$ spinduck deploy <bjgsyh 计划> --db /tmp/examView2.duckdb
deploy: target=pkg_split_trade_step2.prc_pre_deal_bjgsyh ddl_files=28 routine_files=7 db=/tmp/examView2.duckdb
real 0m3.368s     rc=0     -rw-r--r-- 2,109,440 bytes
```

上一段会话同口径的 21 文件计划：`ddl_files=21 routine_files=4`、`real 0m2.682s`、库 1,847,296 bytes
（记为**二手**：本轮未重跑该条，数字来自上一段会话的留档）。

"库真的落盘"不靠文件大小证明，靠换一个进程只带 `--db` 就能跑：`tests/cli/deploy::should_leave_a_persisted_database_a_second_process_can_run_against`
先 `deploy`，再两次独立 `call --db`（`prc_seed_count` 每次 `INSERT` 一行再数回来）得 `OUT p_o = 1` →
`OUT p_o = 2`——后一进程既看得见前一进程装的表与序列，也看得见它写进去的行。

一条必须写清的反面事实（**一手**）：

```text
$ spinduck call --db /tmp/viewprobe/pkg_split_trade_step3_sh_prc_trd_deal_purchase.duckdb \
      pkg_split_trade_step3_sh.prc_trd_deal_purchase
[BIZ-0150] routine pkg_split_trade_step3_sh.prc_trd_deal_purchase not registered      rc=1
```

**落盘的是数据对象，不是例程**：`catalog.register` 产出的编译结果是进程内结构，不写进库文件
（`src/catalog.rs:136`）。所以 `deploy` 之后 `call` 仍要 `--file` 逐个传例程源。design §8 R3 的判据
"部署产物是一个可被 `call` 打开的 `--db` 文件"成立（表/视图/序列都在），但"打开即免传源码"不成立——
那条归 A2-4 的 `call --plan`（design §6.2 / D5 尚未落地）。

### V31.4 四处口径缺陷（一手前后对照）

**① `deploy` 之前不存在**：V30.7 登记"计划是给人看的清单"。本轮补上消费面，`ddl_tables` 从此有语义。

**② 视图体的基表不在闭包里**（循环 4）。V30 的闭包只从**例程**出发，视图文件本身进清单、它读的表不进。
Red 原文：

```text
test plan::tests::should_include_the_base_table_that_a_planned_view_reads ... FAILED
  assertion `left == right` failed: 视图体的基表要随视图一起进清单（下发顺序才可能表先于视图）
    left: ["sql/ddl/views/1_V_BOND.sql"]
 right: ["sql/ddl/tables/t_base.sql", "sql/ddl/views/1_V_BOND.sql"]
```

后果不是报错而是**静默降级**：下发顺序是"表 → 视图"，基表没进清单 ⇒ 内核建视图失败 ⇒
`deploy_view` 落到 `CREATE OR REPLACE VIEW <名> AS SELECT 1` 占位分支，而 `deploy` 仍 rc=0。
真题实测（bjgsyh 计划，`ddl_files=12` 时）：

```text
# 修前：读该视图的一列
[KERNEL-0010] Binder Error: Referenced column "security_id" not found in FROM clause!
Candidate bindings: "1"                                                    rc=1
# 修后（同一份计划重跑 plan → ddl_files=28）：
OUT p_o = 0                                                                rc=0
```

`Candidate bindings: "1"` 就是占位视图那一列——V30.7 那条"占位视图取不到列"的推断至此有了一手数字。

**③ 逗号连表只有第一个名字进链路**（循环 5）。② 修完再探测，语料仍有 4 张视图退化成占位视图
（`BODY=3 PLACEHOLDER=4`）。三张的视图体都是 Oracle 逗号连表：

```sql
FROM par_fund_rate_info t, par_drv_config s        -- V_PAR_FUND_RATE_INFO
```

`FROM` 只出现一次，`sql_object_refs` 只取其后首个标识符 ⇒ 后半边的表对链路隐形：verify 不核它、
闭包也不收它。这条同时是 `verify` 现路径上的缺陷（不是新代码引入），Red 原文：

```text
test verify::chain::tests::should_take_every_relation_in_a_comma_separated_from_list ... FAILED
  assertion `left == right` failed
    left: ["Relation:t_a"]
 right: ["Relation:t_a", "Relation:t_b"]
（补完第一档后同一条断言簇又红在子查询形态）
    left: []
 right: ["Relation:t_b"]                            -- `FROM (SELECT 1) x, t_b b`
```

改法：`comma_join_refs` 从第一个关系名之后继续（别名只有紧跟逗号时才算别名、逗号后的子查询整对越过、
拼回的限定名仍过 `is_object_name` 与表函数排除——**宁少不误**）；`in_relation_slot`/`subquery_tail_refs`
是为把 `sql_object_refs` 压回 60 行以内提出的两个谓词（AGENTS §3），不改判定。

修后：`BODY=9 PLACEHOLDER=0`（7 张不同视图 × 各自所在计划共 9 对，全绿）。

**④ 新用例是否永真**（循环 6 的变异取证）：分别关掉两处修复，同一条 `tests/cli/deploy.rs` 用例转红位置不同——

```text
关 `expand_view_bodies`        → panicked at tests/cli/deploy.rs:267  assert!(text.contains("sql/ddl/tables/t_a.sql"))
关 `comma_join_refs` 的 extend → panicked at tests/cli/deploy.rs:268  assert!(text.contains("sql/ddl/tables/t_b.sql"))
```

两次随即还原，还原后同一条用例绿。

**判定面（`verify`）的实测变化：零**。②③ 都动到 `verify` 共用的代码，所以逐条复跑：

| 语料 | V30.4 之后（本轮起点） | V31 之后 |
|---|---|---|
| `golden/exam_split_trade`（10 entries） | `entries=10 checks=222 failed=0 skipped=9` | 逐字相同 |
| `golden/case_corpus_purchase`（1 entry） | `entries=1 checks=11 failed=0 skipped=1` | 逐字相同 |

`checks` 数没涨是因为逗号连表补出的那些关系，在同一入口的链路里已被其它单表引用覆盖过一次（集合去重）。
也就是说：这两处修复**改变的是部署产物**（表与视图体真的进库了），没有改变核验结论——没有一条 PASS 变 FAIL，
也没有一条 FAIL 变 PASS。计划侧 `missing=0` 在 10 条 entry 上仍然成立，`tables` 计数变化：

| entry | V30.3 | ② 修后 | ③ 修后 |
|---|---|---|---|
| `pkg_split_trade_step2.prc_pre_deal_shjycf` | 21 | 21 | 21 |
| `pkg_split_trade_step2.prc_pre_deal_bjgsyh` | 12 | **28** | 28 |
| `pkg_split_trade_step3_sh.prc_trd_hz_byfund` | 15 | **26** | **27** |
| `pkg_split_trade_step3_sh.prc_trd_deal_purchase` | 7 | 7 | 7 |
| `pkg_split_trade_step5.prc_split_sh_cjqs` | 18 | **19** | **20** |
| `pkg_split_qq.prc_general_deal_inst_sh_{buy,sell,other}` | 14/14/14 | 14/14/14 | 14/14/14 |
| `pkg_inst_domestic_purchase.prc_gen_inst` | 18 | **19** | **20** |
| `pkg_split_trade_step9.prc_deal_bond_repurchase_inst` | 22 | **23** | 23 |

### V31.5 顺带取到的一条 §8 未决项：purchase 夹具那"4 张缺表"

§8 的"包限定名扁平化"行挂着一条 codeweb 时代的手数推算：真实闭包 14 表里 4 张不在
`case_corpus_purchase` 夹具 DDL，"是否真缺件要等 `deploy` 用计划实跑一次"。实跑结果：

```text
$ spinduck plan --dir golden/case_corpus_purchase pkg_split_trade_step3_sh.prc_trd_deal_purchase
plan: target=… routines=2 tables=1 missing=0
$ grep -rli "par_oper_close_date\|par_sys_marketrate\|split_log_info" golden/case_corpus_purchase/sql/
（无输出）
```

三个名字在该夹具的 `sql/` 里**根本不出现**（这些例程是 `0_SIMPLIFIED_*` 简化版），故 SpinDuck 的
闭包既看不到它们也不报缺件。结论：这条推算**不适用于当前夹具形态**，因此不需要"对齐 fixture =
动 golden = Ask-first"那一步；`sys_dummy` 的改判（V30.4 ②）继续成立。

### V31.6 门禁（本机）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all` + `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features --tests -- -D warnings` | 干净（无 `#[allow]`、无新增 lint 抑制） |
| `cargo test --features native` | **323 passed / 0 failed** |
| `cargo test --features cli` | **365 passed / 0 failed**（含契约层 42） |
| `cargo audit` | `error: 4 vulnerabilities found!`——全部 `zip 6.0.0`（RUSTSEC-2026-0367/0368/0369/0429，Solution 一律 `>=9.0.0`），与既有豁免清单同集合、零新增（本轮零新依赖）。一手：本机 `cargo audit --no-fetch --stale`，advisory-db 缓存提交 `899077d9`（2026-11-09）；`--no-fetch` 是因为此刻取不到 GitHub（`error: couldn't fetch advisory database … error sending request for url (…/info/refs?service=git-upload-pack)`），不是"跳过审计" |
| `verify` 两语料 | 逐字未变，见 V31.4 末表 |

### V31.7 仍是推算的 / 明确未做

- **`deploy` 非幂等**（一手）：对已装配的库文件再跑一次即撞内核报错，原样带出、不静默跳过：
  `[KERNEL-0070] 建表 …/sql/ddl/tables/dat_trd_repurchase.sql: Catalog Error: Table with name "dat_trd_repurchase" already exists!` rc=1。
  语义是"从零装配一个库"，重来换文件或先删。**未做**：`--force`/逐对象 `IF NOT EXISTS` 化（要先定口径：跳过还是覆盖）。
- **`plan.schema` 不下发**：对象一律落在内核默认目录 `main`，与 `verify` 既有判定口径一致。
  design §6.1 草图给了 `schema` 字段但未约定 `deploy` 用它建 schema，本轮把它**记为未落地**
  （architecture §13.6 已登记为偏差，不假装支持）。
- **不跑种子、不消费两条清单**：`cases/datasets/*`、`materialize`、`writeback` 一律不动——数据面归 §13.1 物化（A3/A4）。
- **`ENV-0030`/`KERNEL-0070` 消息里的路径是 `plan/../../sql/…` 未做词法归一**：可读性问题（`std` 无路径规范化 API，
  `std::path::absolute` 未稳定），未影响判定，留待统一处理。
- **视图套视图的终止条件没在语料上跑到**：语料 11 个视图文件**两两之间无引用**（逐个 grep 视图名 ×`FROM`/`JOIN`/`, ` 三种位置，
  全部"无"），故 `expand_view_bodies` 的队列递归分支只被单测形态覆盖，真题面等价于单层展开。
- **4 张视图不在任何 entry 的闭包内**：`V_DAT_INST_SECU_ACNT_INFO`、`V_DAT_INST_SECU_DEAL_INFO`、
  `V_JK_BDP_DIC_ALL_KIND`、`V_JK_DAT_INST_INDEX`——10 份计划里一份都没有，本轮既不下发也不探测。
- **A2-4（D5）未动**：`call` 侧的形态 C（依赖齐全但缺表 → `RETURN 0` + rc=0）仍以原期望通过；
  `call --plan`（免逐个 `--file`）与"红之前不许产出 `RETURN`"都没做。
- **golden（`--features gauss`）侧未本机实测**：②③ 改的是 `deploy_schema` 下发的内容（视图体从占位变真身），
  黄金层是否受影响只有 CI golden job 有资格回答。本轮**没有**跑 `--features gauss`（容器面见 V21.12/V25）。
- **千万级数据、真实实例、回写**：一个都没碰，全部在本机构架 + `golden/` 只读夹具上完成。

### V31.8 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/plan/mod.rs` | 读侧 `from_toml_str` + `project_root`；`pub mod deploy;` | 新能力（计划消费面） |
| `spinduck/src/plan/deploy.rs` | 新增：`Deployment` + `resolve`/`apply` 两段 | 新能力 |
| `spinduck/src/plan/closure.rs` | `expand_view_bodies`/`view_sources`（视图体基表补全） | 行为改动（闭包面） |
| `spinduck/src/plan/tests.rs` | 新增 5 条（读侧 4 + 视图体基表 1），1 条注释订正 | 新增测试 |
| `spinduck/src/verify/chain.rs` → `chain/{mod.rs,tests.rs}` | 拆分（AGENTS §3：加完逗号连表后总行 768 > 700）；`sql_object_refs` 扩逗号连表 + `comma_join_refs`/`subquery_tail_refs`/`matching_paren`/`in_relation_slot`；`sql_object_refs` 放开 `pub(crate)`；1 条新单测 | 行为改动（核验面抽取口径）+ 纯搬家 |
| `spinduck/src/verify/deploy.rs` | 四个逐文件实现放开 `pub(crate)` 供计划面复用 | 口径收口（不改判定） |
| `spinduck/src/verify.rs` | 导出上述 `pub(crate)` 项 | 接线 |
| `spinduck/src/cli/{deploy.rs,args.rs,mod.rs,run.rs}` | `deploy` 子命令 + 变体 + 解析 + usage + 分发 | 新子命令（CLI 仅壳） |
| `spinduck/tests/cli/deploy.rs` + `tests/cli.rs` | 7 条契约 + 子模块接线（本层 35→42） | 新增测试 |
| `docs/architecture.md` | §13.5 增"视图体的基表"一行；新增 §13.6 `deploy` | 设计正文 |
| `docs/validation.md` | 本节 V31 + V30.7 那条登记项标注已修 + V30.1/29.8 的 `chain.rs` 路径标注 | 实证登记 |
| `docs/design-production-gaussdb.md` | §6.2 `deploy` 行改判已落地、§7 A2 行、§8 三行（视图体两行合并为已修 + 新增 `plan.schema`/非幂等/例程不落盘三行）、§8 purchase 缺表面已实测 | 仅设计文档 |

---

## V32. A2-4 落地：`call` 第一次红在 `eng.call` **之前**（design D5），并新增计划面入口 `call --plan`

本轮把 A2 的最后一条本机项做掉：§3.3 的静默形态 C（补齐依赖函数、仍不建表 ⇒ `RETURN 0` + rc=0）
在 CLI 面上不复存在，且缺件清单在**执行任何一句例程体**之前就打印出来。同时补上 `call --plan`——
`deploy` 那条"例程不落盘"的限制（V31.3）由此有了免传源码的正路。

前提：**三条人类已有测试的期望值随行为同批改写**，用户在 2026-11-10 明确授权（原话"1、可以动"），
改判清单见 V32.2 末表。除此之外本轮**没有**删、跳、放宽任何一条测试。

### V32.1 交付面

| 段 | 落点 | 内容 |
|---|---|---|
| 判定 | `spinduck/src/plan/closure.rs` | `walk_chain(eng, entry, on_object)`——把原先埋在 `build` 里的 BFS 抽成**唯一一份**闭包遍历，"对象在不在"由回调决定；`kernel_gaps`（内核参照面，目标名走 `catalog.lookup`，与 `Engine::call` 同一个键）+ `kernel_has`（全名精确优先、其次尾段，口径同 `ddl_file_for` / `verify::run::present`，不开第二套判定） |
| 接线 | `spinduck/src/plan/mod.rs` | `pub(crate) use closure::kernel_gaps;`（顺带修掉 `src/verify/chain.rs` 的过期路径注释 → `chain/`） |
| 壳 | `spinduck/src/cli/run.rs` | `call_target_and_sources`（计划面：读计划 → 计划缺件即拒 → 定位项目 → `plan::deploy::resolve` 取例程源 → 追加 `--file`）与 `refuse_on_kernel_gaps`（内核门）。两个门函数都是从 60 行上限里拆出来的（AGENTS §3），`call_cmd` 现 54 行 |
| 解析 | `spinduck/src/cli/args.rs` | `Command::Call { target: CallTarget::Routine \| Plan, … }`——目标做成枚举而不是可选字段；`--plan` 下位置参数**全部**归实参；`call` 无目标即用法错，诊断同时点名两条路 |
| 契约层 | `tests/cli/{call,deploy,usage}.rs` | +6 条（42→48） |
| 单测 | `src/cli/args.rs` 内 `#[cfg(test)]` | +1 条（`--plan` 下位置参数全归实参），1 条改形状（`CallTarget::Routine`） |

输出契约（两条摘要行不同措辞，故意不同——一个要你去补件、一个要你重跑 `plan`）：

```text
MISS <identity> <- <引用它的例程全名>          ← 与计划文件 [[missing]] 逐字同形状
call: target=<归一小写名> missing=N 未执行（design D5 缺件即红）      ← 内核门
call: target=<计划目标>     missing=N 拒绝执行（补齐缺件后重跑 plan）  ← 计划门（发生在开库之前）
```

### V32.2 TDD 循环、改动前形态、以及三条改判

**循环 1（执行面门）** — Red：`should_list_the_missing_relation_and_execute_nothing_when_a_chained_table_is_absent`
（合成夹具：一条链、两次引用同一张 `t_gap_a`、一张 `t_gap_b`、一个未登记的被调过程）。断言簇是**逐字全文**：
三条 `MISS` 行（顺序、去重、`routine:` 与 `relation:` 同列）+ 摘要行 + rc=1 + stderr 空 + 不含 `OUT `。

**"改动前"原文（本轮不引用旧日志：把门整块短路后重跑同一批命令实测）**

```text
# 变异：call_cmd 里 `if false && refuse_on_kernel_gaps(…)?`（门不介入，其余不变）
合成夹具 pkg_a.prc_gap null        → [KERNEL-0010] Catalog Error: Table with name t_gap_a does not exist!  rc=1
形态 A（只传主角）                 → [KERNEL-0010] Scalar Function with name fnc_com_if_deal_date does not exist!  rc=1
形态 C（补齐两个依赖函数、仍不建表）→ RETURN 0   rc=0
```

后两条与 V27.1 登记**逐字一致**（等于在当前代码上二次确认了那轮取证），第一条补上了本轮新写的
合成夹具在门前的形态：内核死在哪张表就只报哪张表，**没有清单**。变异还原后同批命令转红为 MISS 形态。

> 这处变异与 V32.3 的变异 A **不是同一改法**：这里是 `false &&` 短路，整块门（含打印）都不介入，用来复现
> "改动前"；变异 A 只把 gaps 清空，MISS 行仍打印但不早退，用来单独证明"早退这一步"非虚。

**循环 2（计划面入口）** — Red：`should_call_the_plan_target_without_passing_any_source_file`（deploy 后
`call --plan … 0` ⇒ rc=0 + `OUT p_o = 1`，零 `--file`）。

**循环 3（计划门的拒）** — Red 原文（当时只有内核门，缺件门晚了一步、且库已经开了）：

```text
left:  "MISS relation:t_a <- prc_seed_count\nMISS sequence:seq_a <- prc_seed_count\n
        call: target=prc_seed_count missing=2 未执行（design D5 缺件即红）\n"
right: "…拒绝执行（补齐缺件后重跑 plan）…"
```

这条 Red 是本轮最有信息量的一次：**期望的拒没发生，实际发生的拒晚了一步并且已经建了库文件**。
Green 之后同一断言簇另加 `!db.exists()`。

**三条人类已有测试的改判**（授权范围内，均只改期望值/测试名，未放宽断言强度）

| 测试 | 旧期望 | 新期望 |
|---|---|---|
| `call.rs::should_exit_0_returning_0_when_deps_registered_but_tables_missing` → 改名 `should_exit_1_with_the_missing_relation_when_deps_are_registered_but_tables_are_absent` | rc=0 + `RETURN 0` | rc=1 + `MISS relation:par_sys_purchase <- fnc_get_purchase_js_days` + 摘要行，且 `!out.contains("RETURN")` |
| `call.rs::should_ignore_ddl_and_seed_files_given_to_call`（形态 B≡A） | 两条都 rc=1 + `[KERNEL-0010] … does not exist`，并断言 B 与 A 输出**逐字相同** | 两条都 rc=1 + 同一条 MISS 摘要，**B≡A 的逐字相等断言原样保留**（本条意图不变：建表脚本对 `call` 彻底无效）；内核面那条证据仍留在 V27.1 与 `tests/guard/guard_register_silent_drop.rs`，未删改 |
| `args.rs` 内 `should_parse_call_with_flags_and_args` | `routine: "prc_x"` | `target: CallTarget::Routine("prc_x")`（纯形状跟随，行为断言不变） |

### V32.3 非虚断言：两处变异各自只红该红的用例

| 变异 | 改法 | 转红 | 红点 |
|---|---|---|---|
| A 内核门短路 | 让 `kernel_gaps` 的返回值里 gaps 恒为空 `Vec`（target 仍保留，故摘要行的名字不变） | **恰好 3 条**（本轮 A2-4a 那 3 条），其余 45 条全绿 | `tests/cli/call.rs:122`/`:152`/`:186` |
| B 计划门短路 | `if !plan.missing.is_empty() && false {` | **恰好 1 条** | `tests/cli/deploy.rs:165` |

两次随即还原，还原后契约层 48/48 绿。判据的另一半（**门不是永红**）同批取证：
`should_leave_a_persisted_database_a_second_process_can_run_against`（deploy 后的库 + `--file` 调用，门静默、rc=0）、
`should_call_routine_from_file_and_print_out_params`（无表夹具照常返回 `OUT p_o = 42`）、
以及 V32.6 那条考题语料正例。

### V32.4 语料侧：闭包重构输出中性、判定面逐字未变

`build` 改成走 `walk_chain` 是**搬家不是改判**，10 条 entry 逐条复跑 `plan`：

| entry | V31.4 末列 | V32 之后 |
|---|---|---|
| `prc_pre_deal_shjycf` / `prc_pre_deal_bjgsyh` / `prc_trd_hz_byfund` / `prc_trd_deal_purchase` / `prc_split_sh_cjqs` | 21 / 28 / 27 / 7 / 20 | 21 / 28 / 27 / 7 / 20 |
| `prc_general_deal_inst_sh_{buy,sell,other}` | 14 / 14 / 14 | 14 / 14 / 14 |
| `prc_gen_inst` / `prc_deal_bond_repurchase_inst` | 20 / 23 | 20 / 23 |

`routines` 计数与 `missing=0` 全部不变。核验面（`verify`，与本轮无共用代码路径，仍复跑确认）：

| 语料 | V31.4 之后 | V32 之后 |
|---|---|---|
| `golden/exam_split_trade`（10 entries） | `entries=10 checks=222 failed=0 skipped=9` | 逐字相同 |
| `golden/case_corpus_purchase`（1 entry） | `entries=1 checks=11 failed=0 skipped=1` | 逐字相同 |

### V32.5 新登记的盲区：表达式级被调函数**不在闭包里**（一手）

`routine_refs` 只采语句级 `CallRoutine`。最小夹具（`prc_outer` 一条语句读 `t_ok`、一条赋值 `v := fnc_hidden()`，
`fnc_hidden` 的体读 `t_hidden`）实测：

```text
$ spinduck plan --dir <夹具> prc_outer
plan: target=prc_outer routines=1 tables=1 missing=0        ← 计划自称干净
$ spinduck plan --dir <夹具> fnc_hidden
plan: target=fnc_hidden routines=1 tables=0 missing=1
MISS relation:t_hidden <- fnc_hidden                        ← 同一个对象，独立入口就看得见
```

执行面两种后果**都响亮**（同一夹具，`--db` 指向已 deploy 出 `t_ok` 的库）：

```text
被调函数未登记 → [KERNEL-0010] Scalar Function with name fnc_hidden does not exist!   rc=1
被调函数已登记 → [KERNEL-0010] Table with name t_hidden does not exist!               rc=1
```

**判定**：本盲区不会把 D5 退回形态 C 的静默 `RETURN 0`（红还是红，只是由内核报、且不是一份清单）；
**但**若语料在被调体外自己套了 `WHEN OTHERS`——`case_corpus_purchase` 的主角正是这个形状——仍会归成
合法值。收口办法是把 PL 表达式里的函数调用纳入 `routine_refs`（建议编号 A2-5，本轮未排期），
`plan`/`deploy`/`call` 门三处一起受益。此条已入 design §8「缺件门的盲区」行与 architecture §13.7 的口径段。

### V32.6 发版形态二进制复核

`cargo build --release --features cli --locked` 产物（`spinduck 0.1.2`）：

```text
# 形态 C（purchase 夹具，三条 --file 齐全、不建表）
MISS relation:par_sys_purchase <- fnc_get_purchase_js_days
call: target=fnc_get_purchase_js_days missing=1 未执行（design D5 缺件即红）              rc=1

# 考题语料全链路：plan → deploy → call --plan（零 --file）
plan:   target=…prc_trd_deal_purchase routines=2 tables=7 missing=0
deploy: target=…prc_trd_deal_purchase ddl_files=7 routine_files=2 db=…/w.duckdb
call --plan … --db …/'20240102' null  →  OUT p_o_succeed = 0                                rc=0（门静默）
call --plan … --db …/fresh.duckdb 同一份计划换一个空库
                                    →  missing=7 未执行（design D5 缺件即红）                 rc=1
```

**`OUT p_o_succeed = 0` 只是管道证据**（计划→装配→注册→执行→回读这条链在真语料上通），
**不是**业务基线：库里没有任何数据，`0` 不来自任何一次比对。A5/B 阶段接 `compare` 才有裁决资格。

### V32.7 门禁（本机）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all` + `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features --tests -- -D warnings` | 干净（无 `#[allow]`、无新增 lint 抑制） |
| `cargo test --features native` | **324 passed / 0 failed** |
| `cargo test --features cli` | **372 passed / 0 failed**（含契约层 48） |
| `cargo audit` | 与 V31.6 同一集合：4 条全部 `zip 6.0.0`（RUSTSEC-2026-0367/0368/0369/0429），**零新增**（本轮零新依赖，`Cargo.lock` 未变）。一手：本机 `cargo audit --no-fetch --stale`，advisory-db 缓存提交 `899077d9`（2026-11-09）；`--no-fetch` 的原因是此刻取不到 GitHub，不是"跳过审计" |
| `verify` 两语料 | 逐字未变，见 V32.4 末表 |

### V32.8 仍是推算的 / 明确未做

- **内核门判红时 `--db` 文件已经创建**（一手）：`call --plan <干净计划> --db <不存在的路径>` 在缺件红之后
  留下一个 12,288 字节的空库文件（`fresh.duckdb`）。这不是疏漏而是判定顺序的必然——内核门的参照面**就是
  内核 catalog**，不开库读不到它。**未做**：`--dry-run` 之类的"只判不建"入口（要先定它归 `plan` 还是归 `call`）。
  计划门那一道不受影响：它在开库之前拒，`!db.exists()` 有断言（V32.2 循环 3）。
- **内核门的参照面是"开库那一刻"的 catalog**（推算，未在语料撞到）：例程自己 `CREATE TABLE` 再读的形态
  会被判成缺件。本轮 10 条 entry + purchase 夹具都没有这种写法，故只登记不处理。
- **golden（`--features gauss`）侧未本机实测**：门在 `cli/run.rs`，黄金层走进程内 `Engine::call` 不经 CLI 壳，
  故**推算**影响为 0；这条推断没有跑过 `--features gauss` 佐证（容器面限制见 V21.12/V25）。
- **`call --plan` 与 `--file` 同名例程的覆盖序**：本轮只测"计划源 + 计划外追加一个不同名桩包"的方向，
  同名先后注册谁生效**未取证**，留待 D9② 后一并补。
- **D9② 真 GBK 解码**：已授权、未动工（本轮零新依赖）。§4 那 3,149 个 GBK 文件在落地前一律跑不起来。
- **千万级数据、真实实例、回写**：一个都没碰。

### V32.9 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/plan/closure.rs` | `walk_chain`（BFS 抽成唯一实现）+ `kernel_gaps` + `kernel_has`；`build` 改回调式 | 口径收口（`plan` 侧输出中性，见 V32.4）+ 新能力（执行面判定） |
| `spinduck/src/plan/mod.rs` | `pub(crate) use closure::kernel_gaps;`；过期路径注释订正 | 接线 |
| `spinduck/src/cli/run.rs` | `call_target_and_sources` + `refuse_on_kernel_gaps`；`call_cmd` 走两道门（67→54 行，回到 AGENTS §3 上限内） | 行为改动（执行面） |
| `spinduck/src/cli/args.rs` | `CallTarget` 枚举、`--plan` 解析、usage 两行、缺目标用法错；单测 +1/改 1 | 新能力（CLI 仅壳） |
| `spinduck/tests/cli/call.rs` | +1 条门契约；2 条人类测试期望改判（V32.2 末表） | 新增测试 + **授权改判** |
| `spinduck/tests/cli/deploy.rs` | +4 条 `call --plan` 契约（正例、计划门、`ENV-0010` 缺计划、`ENV-0030` 缺例程源） | 新增测试 |
| `spinduck/tests/cli/usage.rs` | +1 条（`call` 无目标） | 新增测试 |
| `spinduck/tests/cli.rs` | 头部实证索引补 V32 一行 | 文档性 |
| `docs/architecture.md` | 新增 §13.7（`call` 的两道门 + 计划面入口 + 已知盲区） | 设计正文 |
| `docs/validation.md` | 本节 V32 | 实证登记 |
| `docs/design-production-gaussdb.md` | §1 三行改判（最危险缺口已封 / 裁决仍无人管 / A2 进度）、§3.3 现状段、§7 A2 行、§8 两行（`call --plan` 已落地 + 新增表达式级盲区行）、§9 DoD 2、附录复现命令现状 | 仅设计文档 |

---

## V33（2026-11-10）：D9② 真 GBK 解码——源码读入收口成一把判定，语料字节不动

范围：A2 的最后一项本机设计项（design D9②、§3.5、§7 A2 行）。用户 2026-11-10 授权新增运行时
依赖（Ask-first 项之一）。零 PR、零提交（工作树等用户过目）；零 CI 改动（V29.4 的
`cargo test --features cli` 一步已覆盖本层新增契约）。

### V33.1 交付面

| 能力 | 落点 | 契约 |
|---|---|---|
| UTF-8 严格 → GBK 严格 → `ENV-0030` 的**唯一**读入实现 | 新增 `spinduck/src/source.rs`（80 行，`read_text` / `decode` / `refused`） | 单测 3 条（`src/source.rs`） |
| 执行/计划面（`call`、`explain`、`plan`、`call --plan`、`deploy` 读计划） | `cli::read_source_strict` 改为薄封装，转调 `source::read_text` | 契约层改判 2 条 + 新增 3 条 |
| 核验/部署面（表 DDL、视图、例程注册、数据集种子） | `verify/deploy.rs` 四处 `std::fs::read_to_string` 全部换成 `source::read_text` | 契约层新增 1 条、改判 1 条 |
| 体检面（`compile`） | **未改**：`cli::read_source` 仍是有损解码 + `(non-utf8)` 标记（§3.5 末段的原始取舍） | `compile::should_mark_non_utf8_corpus_in_compile_report` 逐字不变，作为"另一面仍如此"的锁 |

一句话口径：**同一份源码、只差文件编码，现在只能给出同一个结果**；给不出结果的那种字节
（UTF-8 与 GBK 都不合法）仍是一条响亮红。

### V33.2 TDD 循环与一手 Red 记录

四个循环，每循环先有会失败的断言。原文如下（未润色）。

**循环 1**（模块落地，UTF-8 严格 + 两把都不成立即红）：`source.rs` 先只有一条
`from_utf8` 判定，两条单测同批写就、直接绿——本轮唯一"没有 Red"的一步，登记为搬家而非新行为：

```text
test source::tests::should_pass_valid_utf8_bytes_through_byte_for_byte ... ok
test source::tests::should_refuse_bytes_that_are_not_valid_utf8 ... ok
```

**循环 2 Red**（GBK 字节必须解成汉字；此刻还没有 GBK 分支）：

```text
test source::tests::should_decode_valid_gbk_bytes_to_their_unicode_text ... FAILED
panicked at src/source.rs:61:46:
called `Result::unwrap()` on an `Err` value: EngineError { domain: Env,
  code: ErrorCode("ENV-0030"),
  message: "读取 x.sql: stream did not contain valid UTF-8" }
```

Green 只加了四行（`encoding_rs::GBK.decode` + `had_errors` 即拒）与 `encoding_rs = "=0.8.44"`。
同批把循环 1 那条"非 UTF-8 即红"的输入换成 `0xFF 0xFE`（两把判定都不成立的字节），
改名 `should_refuse_bytes_that_are_valid_in_neither_encoding`——**它是新测试自己的一条**，
不是人类已有断言；红面本身没有因为 GBK 落地而放宽。

**循环 3 Red**（执行面：`call` 与 `explain`）。`call` 侧改判前后：

```text
test call::should_decode_a_gbk_source_and_return_the_same_literal_as_its_utf8_twin ... FAILED
assertion `left == right` failed: [ENV-0030] 读取 /var/folders/.../gbk.sql:
  stream did not contain valid UTF-8
  left: 1
 right: 0
```

`explain` 侧是同一条行为的另一半，它的 Red 输出**恰好就是新行为**（改判前的断言在等一条红，
实测已经给了 IR 行）：

```text
test rules_explain::should_exit_1_with_env_0030_when_explain_source_is_not_utf8 ... FAILED
assertion `left == right` failed: 0	assign	p_o := '中文'
  left: 0
 right: 1
```

Green = `read_source_strict` 的 13 行实现换成一行转调 `crate::source::read_text`。

**循环 4 Red**（核验/部署面：`verify` 的 GBK 包体）。两条一起红，一条是正例、一条是红面输入已换：

```text
test verify::should_verify_a_gbk_package_without_complaining_about_encoding ... FAILED
assertion `left == right` failed: [ENV-0030] 读取 /var/folders/.../sql/routines/pkg_a.sql:
  stream did not contain valid UTF-8
  left: 1
 right: 0

test verify::should_exit_1_with_env_0030_when_corpus_is_valid_in_neither_encoding ... FAILED
[ENV-0030] 读取 /var/folders/.../sql/routines/g.sql: stream did not contain valid UTF-8
```

Green = `verify/deploy.rs` 四处换调 `source::read_text`。

### V33.3 契约层：52 条，改判 3 条的原文

本轮改动全部落在**我自己新建的 CLI 契约层**（V28 起），人类已有测试零改动。三条 V29 时代的
期望值随行为改判，理由与 diff 含义：

| 测试 | 旧期望（V29.3） | 新期望（V33） | 行为为什么应该变 |
|---|---|---|---|
| `call::should_exit_1_with_env_0030_when_call_source_is_not_utf8` | GBK 源 → `ENV-0030` + rc=1，且不出现 `中文` | 拆成两条：`call::should_decode_a_gbk_source_and_return_the_same_literal_as_its_utf8_twin`（GBK 与 UTF-8 两份同内容源码**同为** `OUT p_o = 中文` rc=0）+ `call::should_exit_1_with_env_0030_when_call_source_is_valid_in_neither_encoding`（输入换成 `0xFF 0xFE`） | D9① 判红是"D9② 之前的中间态"，design §3.5 末段写明它的代价是 3,149 个文件跑不起来。旧断言里"不得出现 `中文`"这一半在 D9② 之后**语义反转**：GBK 的 `中文` 字节本来就该还原成 `中文` |
| `rules_explain::should_exit_1_with_env_0030_when_explain_source_is_not_utf8` | GBK 源 → rc=1 且不产出 IR | 拆成两条：`…should_explain_gbk_literal_verbatim_as_the_same_ir_as_its_utf8_twin`（`0\tassign\tp_o := '中文'`，两文件逐字相同）+ `…_valid_in_neither_encoding` | 同上：不产出 IR 的前提（"读不懂就是读不懂"）已不成立 |
| `verify::should_exit_1_with_env_0030_when_corpus_is_not_utf8` | GBK 包体 → `[ENV-0030] 读取` rc=1 | 拆成两条：`…should_verify_a_gbk_package_without_complaining_about_encoding`（rc=0 + `PASS Entry pkg_a.prc_gbk` + 显式断言**不含** `ENV-0030`）+ `…_valid_in_neither_encoding` | 核验面原本就是"判红形状"的参照方；它不改，`plan` 与 `verify` 对同一份 GBK 语料就会一个说闭包干净、一个报编码错（§3.3 缺件门反对的同一类漂移） |

新增夹具（同批，测试侧零魔法）：`tests/cli.rs` 的 `write_bytes`（按字节写夹具）、
`write_gbk_package` + `PACKAGE_GBK_HEAD/TAIL`（关键字全 ASCII、只有字面量是 GBK 字节，
即交付件真实形态）、`write_flag_routine`（`fnc_flag` 的 GBK/UTF-8 两版，字面量经
`encoding_rs::GBK.encode` 生成，不硬抄字节）。

**判据实验（design §3.5 的主角，永久保留）**：
`call::should_return_the_same_match_for_a_gbk_and_a_utf8_source_with_one_utf8_actual`——
同一份 `IF p_i_flag = '已成交' THEN 'MATCH' ELSE 'NO_MATCH'`，GBK 与 UTF-8 两份源文件，
同一个 UTF-8 实参 `'已成交'`：

```text
test result: ok. 1 passed; 0 failed
```

三代口径同图：改动前 `NO_MATCH`/`MATCH` 两条 rc=0（V27.4）→ D9① 后 `ENV-0030` rc=1（V29）
→ 现在两条同为 `MATCH` rc=0。本文最警惕的"静默错值"形态在这一条上给出正向证据，
而不只是"不再静默"。

### V33.4 非平凡性：把 GBK 分支摘掉，谁红

变异 = 只删 `had_errors` 那三句，让非 UTF-8 一律回红（即退回 D9① 形态），其余不动：

```rust
let (_text, _, _had_errors) = encoding_rs::GBK.decode(bytes);
Err(refused(path))
```

| 层 | 结果 |
|---|---|
| `cargo test --features cli --lib source::` | `should_decode_valid_gbk_bytes_to_their_unicode_text` **FAILED**（另两条仍绿——UTF-8 正例与"都不合法"红面不依赖 GBK 分支） |
| `cargo test --features cli --test cli` | **4 条 FAILED**：`call::should_decode_a_gbk_source…`、`call::should_return_the_same_match…`、`rules_explain::should_explain_gbk_literal_verbatim…`、`verify::should_verify_a_gbk_package…`；48 条仍绿 |

红的面正是新增的 4 条正例，一条不多一条不少 ⇒ 新行为由新断言守护，且解码分支不是死代码。
变异体已回滚（`git diff` 无残留，`src/source.rs` 与登记一致）。

### V33.5 语料侧：本仓语料输出中性 + 交付件侧的一手计数

**本仓五份 golden 语料逐文件编码普查**（`*.sql` 共 167 个）：UTF-8 **167 / 需 GBK 0 / 都不通 0**
（`case_corpus_min_tax` 3、`case_corpus_purchase` 8、`case_corpus_stock_kind` 3、
`case_split_agg` 4、`exam_split_trade` 149）。⇒ 本轮对仓内语料**不可能**改判任何输出，
下面两张"逐字未变"表因此是判定面而非巧合：

| entry（`--schema bigfund`） | V32.4 | V33 之后 |
|---|---|---|
| `prc_pre_deal_shjycf` / `prc_pre_deal_bjgsyh` / `prc_trd_hz_byfund` / `prc_trd_deal_purchase` / `prc_split_sh_cjqs` | 21 / 28 / 27 / 7 / 20 | 21 / 28 / 27 / 7 / 20 |
| `prc_general_deal_inst_sh_{buy,sell,other}` | 14 / 14 / 14 | 14 / 14 / 14 |
| `prc_gen_inst` / `prc_deal_bond_repurchase_inst` | 20 / 23 | 20 / 23 |

`routines` 计数与 `missing=0` 十条全不变。核验面：

| 语料 | V32.4 | V33 之后 |
|---|---|---|
| `golden/exam_split_trade`（10 entries） | `entries=10 checks=222 failed=0 skipped=9` | 逐字相同 |
| `golden/case_corpus_purchase`（1 entry） | `entries=1 checks=11 failed=0 skipped=1` | 逐字相同 |

**交付件侧（只读、只出计数，不外传、不入库、不抄内容）**——D9② 到底解锁了多少：
用**被测面同一把刀**（`encoding_rs::GBK.decode`，即 `source::decode` 的调用序列）逐文件统计，
14,611 个 `.sql`：

| 判定 | 文件数 | 含义 |
|---|---|---|
| UTF-8 严格可解 | **11,462** | 走第一把判定，与 D9① 之前完全同路径 |
| UTF-8 不合法、GBK 严格可解 | **3,149** | 这 3,149 个是 D9① 时代"一律 rc=1"的那一批，现在全部可读 |
| 两把判定都不成立 | **0** | 交付件里没有"真不可解"的文件；`ENV-0030` 那一道在真实语料上不被触发 |
| 同时是合法 UTF-8 与合法 GBK | **5,130** | UTF-8 优先**不是风格**：这批按 GBK 读必然乱码 |
| 非 UTF-8 里同时是合法 Big5 且文本不同 | **418** | 残留歧义面；CJK 占比 GBK 94.8% vs Big5 92.3%，机械化判据不成立，采信 GBK 的依据是交付来源（§13.8） |

复现（探针是一次性 scratch，**不入库**：它读仓库外路径，任何形式进 `tests/` 都会把门禁
绑到交付件上。计数不打印任何文件名或内容）：

```bash
# /tmp/enc_probe.rs：walk → from_utf8 优先，再 encoding_rs::GBK.decode 的 had_errors
rustc -O enc_probe.rs \
  --extern encoding_rs=spinduck/target/debug/deps/libencoding_rs-070bf67158c7409b.rlib \
  -L spinduck/target/debug/deps -o enc_probe
./enc_probe "<交付件根目录>"
# utf8=11462 gbk_strict_ok=3149 neither=0 total_sql=14611 utf8_and_gbk_decodable=5130
```

Big5 歧义面用 python 复核（`decode('gbk')` 与 `decode('big5')` 对同一批字节比较，仅出计数）。
**python 与 Rust 的 GBK 表不同源**（Python 的 `gbk` codec 对 0x80 等字节有自己的映射，
`encoding_rs` 按 WHATWG），所以主口径取 Rust 侧那份；两侧对 3,149/0 这一组数字**结论相同**。

### V33.6 发版形态二进制复核

`cargo build --release --features cli --locked` 产物（`spinduck 0.1.2`），自造两份同内容、
不同编码的夹具（`fnc_flag` 比较实参与 `'已成交'`）+ 一份 GBK 包项目：

```text
# 判据实验：两份源码只差编码，同一个 UTF-8 实参
call --file gbk.sql  fnc_flag "'已成交'"   →  RETURN MATCH      rc=0
call --file utf8.sql fnc_flag "'已成交'"   →  RETURN MATCH      rc=0     ← 逐字相同

# 两把判定都不成立（0xFF 0xFE）
call --file broken.sql prc_gbk null
   → [ENV-0030] 读取 /tmp/sd-v32-…/broken.sql:
     stream is neither valid UTF-8 nor valid GBK                          rc=1

# 核验面：GBK 包体（关键字全 ASCII，只有字面量是 GBK）
verify <项目>  →  PASS Entry pkg_a.prc_gbk
                  verify: entries=1 checks=2 failed=0 skipped=1           rc=0

# 体检面（刻意不变的那一面，同一份 GBK 文件）
compile <项目>/sql  →  OK …/sql/routines/pkg_a.sql  routines=1  (non-utf8)
                       summary: files=1 ok=1 failed=0 routines=1          rc=0
```

`--locked` 通过 = `Cargo.toml` 的新依赖与 `Cargo.lock` 一致；lockfile 的 diff 只有一行
（`encoding_rs` 进 `spinduck` 的依赖表），零新增 crate。

### V33.7 门禁（本机）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all` + `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features --tests -- -D warnings` | 干净（无 `#[allow]`、无新增 lint 抑制） |
| `cargo test --features native` | **327 passed / 0 failed**（V32.7 的 324 + `source::` 3 条） |
| `cargo test --features cli` | **379 passed / 0 failed**（含契约层 **52**，V32.7 的 48 + 4） |
| `cargo build --release --features cli --locked` | 通过，产物 `spinduck 0.1.2` |
| `cargo audit --no-fetch --stale` | 与 V32.7 同一集合：4 条全部 `zip 6.0.0`（RUSTSEC-2026-0367/0368/0369/0429），**零新增**。新提为直接依赖的 `encoding_rs 0.8.44` 无公告；advisory-db 缓存提交 `899077d9`（2026-11-09），`--no-fetch` 的原因是此刻取不到 GitHub，不是"跳过审计" |
| `plan` 十条 entry + `verify` 两语料 | 逐字未变，见 V33.5 三张表 |

### V33.8 仍是推算的 / 明确未做

- **`verify` 侧"文件读不到"的错误码由 `ENV-0030` 变 `ENV-0010`**（一手代码改动，**无契约覆盖**）：
  四处 `read_to_string` 原本把 IO 失败也归 `ENV-0030`，收口后 IO 失败走 `source::read_text` 的
  `ENV-0010 read <path>: <io>`。可达面只有"列举之后、读取之前文件消失"（TOCTOU）或目录被并发改动，
  本轮**没有**为它写契约，也没有实测过一次红。判红面（两把都不成立）另有契约，见 V33.3。
- **GBK 判定采信的是"交付件是简体中文环境"这一先验，不是检测器**（一手计数 418，判定推算）：
  见 V33.5 末行与 §13.8。若日后接入繁体/日文语料，这条会从理论变成静默错值——届时应显式指定
  编码而不是给 `source::decode` 加启发式。
- **`encoding_rs` 的 GBK 解码与 GaussDB 实例侧的 GBK 不是同一实现**（未取证）：本仓语料文件字节
  与实例 `SERVER_ENCODING` 的关系属 A0b（附录 B 的 `SHOW server_encoding`），实例未到手前
  不能声称"引擎还原出的汉字与实例端比较结果一致"。V33.3 的判据实验比的是**引擎自身两侧一致**，
  不是引擎 vs GaussDB。
- **`compile` 体检面未接解码**（刻意，design §3.5 末段）：它的 `(non-utf8)` 标记在 D9② 之后
  读起来像"还能再优化成 `(gbk)`"。**未做**，理由是不想让体检面参与判定——体检要的是
  一趟扫完并按错误码分类，一旦它也开始解码，"体检 OK"与"执行可跑"这两件事就再次混在一条
  退出码里。
- **golden 侧（`--features gauss`）未本机实测**：本轮改动全在 CLI 壳与读入层，黄金层走进程内
  `Engine::call` 不经这些读入点，故**推算**影响为 0；同 V32.8 的容器面限制（V21.12/V25）。
- **千万级数据、真实实例、回写**：一个都没碰。A2 之后（A3 起）全部要实例。

### V33.9 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/source.rs` | **新增**：`read_text` / `decode` / `refused` + 3 条单测 | 新能力（唯一读入判定） |
| `spinduck/src/lib.rs` | `pub mod source;` | 接线 |
| `spinduck/Cargo.toml` | `encoding_rs = "=0.8.44"`（注释写明"传递依赖提为直接依赖，零新增 crate"） | 新依赖（Ask-first，已授权） |
| `spinduck/Cargo.lock` | +1 行（`spinduck` 的依赖表多 `encoding_rs`），无新 crate 条目 | 同上 |
| `spinduck/src/cli/mod.rs` | `read_source_strict` 13 行实现 → 一行转调；`read_source`（有损面）**未动** | 行为改动 + 口径收口 |
| `spinduck/src/verify/deploy.rs` | 四处 `read_to_string` → `source::read_text` | 行为改动（核验/部署面） |
| `spinduck/tests/cli.rs` | +`write_bytes`、+`write_gbk_package` 与 `PACKAGE_GBK_*`、+`write_flag_routine`、`write_gbk_routine` 改为复用 `write_bytes`；头部索引补 V33 | 新增夹具 |
| `spinduck/tests/cli/call.rs` | 改判 1 条 → 2 条，+1 条判据实验（`should_return_the_same_match…`） | **授权改判** + 新增 |
| `spinduck/tests/cli/rules_explain.rs` | 改判 1 条 → 2 条（GBK 逐字 IR + 都不合法判红） | **授权改判** + 新增 |
| `spinduck/tests/cli/verify.rs` | 改判 1 条 → 2 条（GBK 包体 rc=0 + 都不合法判红） | **授权改判** + 新增 |
| `docs/architecture.md` | §6.1 编码条文改判（原"引擎不做运行时编码探测"作废）+ 新增 §13.8 | 设计正文 |
| `docs/validation.md` | 本节 V33 | 实证登记 |
| `docs/design-production-gaussdb.md` | §1 两行改判（编码能吃下 / A2 本机项全部完成）、§3.5 表与处置段、§4 编码行 + 新增歧义行、§6 D9 决策记录补 ②、§7 A2 行与退出判据、§8 编码行与 CI 数字、§9 DoD 2、附录 §3.5 复现现状 | 仅设计文档 |

---

## V34. A2-5 落地：表达式级被调函数**进闭包**，缺件门与计划第一次看见"被调函数它自己的表"

本轮收的是 V32.5 登记的最后一个本机盲区（design §8「缺件门的盲区」行）。判定口径不变
（一条 BFS、两个参照面、缺件即红），变的是**采集面**：PL 表达式里的裸名函数调用过去
只被内核看见，现在按例程目录命中后进闭包。三条 `plan` 契约 + 一条 `call` 契约 + 一条
采集面单测为新增，**本轮没有改判任何一条既有期望值**——既有契约一条不减、一条不改，
改的只有语料侧计划清单的长度（V34.4，属"计划漏件"被修掉的直接后果，逐条给数）。

### V34.1 交付面

| 项 | 内容 |
|---|---|
| 采集面 | `spinduck/src/verify/chain/mod.rs`：`Walk` 新增 `expr_names` 面，`walk_expr` 改为持 `&mut Walk`（一趟遍历同时交"结论"与"候选"），新增 `pub fn expr_func_names` |
| 闭包 | `spinduck/src/plan/closure.rs` 的 `walk_chain`：BFS 里在 `routine_refs` 之后加一段候选解析——裸名逐个 `eng.catalog.lookup` 精确命中才排进队列 |
| 口径来源 | `spinduck/src/runtime/eval.rs:89` 的 `eval_funccall`：单次 `catalog.lookup(name)`，未命中即 `eval_builtin` 下内核。闭包用**同一个键、同一个精确度**，不补包名（V34.7 记为有意的残余） |
| 三处受益 | `plan`（清单与 `[[missing]]`）、`deploy`（下发的例程源与表）、`call`/`call --plan`（内核门）——同一条 BFS，不存在第二套 |
| 契约 | `tests/cli/plan.rs` +3、`tests/cli/call.rs` +1、`src/verify/chain/tests.rs` +1 → 契约层 **56**（V33.7 的 52 + 4） |
| 文档 | architecture §13.7（原「已知盲区」条文改判为「表达式级被调函数进闭包」+「仍然看不见的两类」，判定行补采集面说明）、design §1 两行 + §7 A2 行与退出判据 + §8 盲区行改判 |

### V34.2 TDD 循环与 Red 原文

**循环 1（计划面）** — 夹具：`prc_outer` 一条赋值 `v_n := fnc_hidden()`；`fnc_hidden` 单独
一个源文件，其体读 `t_hidden`；DDL 只声明 `t_a`。

```text
test plan::should_put_an_expression_called_function_and_its_own_table_in_the_closure ... FAILED
assertion `left == right` failed: 被调函数的表没声明即缺件:
plan: target=prc_outer routines=1 tables=0 missing=0
  left: 0
 right: 1
test plan::should_deploy_the_expression_called_function_when_its_table_is_declared ... FAILED
routines 计**源文件**数（含被调函数那个文件）；tables 同样计文件数:
plan: target=prc_outer routines=1 tables=0 missing=0        ← 期望 routines=2 tables=1
```

Green 后同一批：`MISS relation:t_hidden <- fnc_hidden` + `routines=2`（两个例程源文件）、
干净形态 `routines=2 tables=1 missing=0`。第三条 `should_not_report_a_missing_object_for_a_builtin_expression_call`
（`v_n := trunc(1)`）在实现**之前**就是绿的，它是"新规则不许把内建函数判成缺件"的哨兵（V34.3 变异 M2 只红它和另外两条既有契约）。

**循环 2（执行面门）** — 夹具：`pkg_a.prc_outer(p_o OUT NUMBER)` 体里 `v := fnc_hidden();`，
`fnc_hidden` 读内核里没有的 `t_hidden`。实现落地后这条直接绿，故它的 Red 用**摘掉新循环**
的同一条闭包复现（与 V32/V33 同一手法）：

```text
$ spinduck call --file expr_gap.sql pkg_a.prc_outer            # 门放行 → 走到实参检查
[BIZ-0151] pkg_a.prc_outer 缺少实参 p_o（形参未声明 DEFAULT）
$ spinduck call --file expr_gap.sql pkg_a.prc_outer 0          # 补齐实参 → 真的执行
[KERNEL-0010] Catalog Error: Table with name t_hidden does not exist!
$ spinduck call --file expr_gap.sql pkg_a.prc_outer 0          # 新循环在位
MISS relation:t_hidden <- fnc_hidden
call: target=pkg_a.prc_outer missing=1 未执行（design D5 缺件即红）
```

契约锁第三种形态（rc=1、逐字清单、无 `OUT`、无 `KERNEL-0010`），把 V32.5 那句
"红还是红，只是由内核报、且不是一份清单"改判成门点名。

**循环 3（采集面单测）** — `src/verify/chain/tests.rs::should_report_bare_expression_calls_as_candidates_not_as_routine_refs`：
裸名 `trunc` 在 `routine_refs` 里必须**仍为空**（人类既有断言未动），在 `expr_func_names` 里必须出现；
嵌套实参 `PKG_A.fn_c(fnc_hidden())` 两个名字都进候选且归一小写。

**循环 4（端到端，非新增测试，取证）** — 同一个夹具走 `plan → deploy → call --plan` 全链：

```text
$ spinduck plan --dir /tmp/a25c pkg_a.prc_outer
plan: target=pkg_a.prc_outer routines=2 tables=1 missing=0
$ spinduck deploy /tmp/a25c/.spinduck/plan/pkg_a.prc_outer.toml --db /tmp/a25c/env.duckdb
deploy: target=pkg_a.prc_outer ddl_files=1 routine_files=2 db=/tmp/a25c/env.duckdb
$ spinduck call --plan /tmp/a25c/.spinduck/plan/pkg_a.prc_outer.toml --db /tmp/a25c/env.duckdb 0
OUT p_o = 3                                                  ← t_hidden 三行，闭包此前少一个例程源
```

### V34.3 非虚断言：两处变异各自只红该红的用例

| 变异 | 改法 | 红面 |
|---|---|---|
| M1 摘掉候选解析 | 新循环换成 `let _ = expr_func_names(&r.prog);` | 3 条新契约全红：2 条 `plan`（`routines=1 missing=0`）+ 1 条 `call`（红面随实参形态为 `BIZ-0151` 或 `KERNEL-0010`，V34.2） |
| M2 把候选当结论 | 未命中目录的裸名也 `missing.insert(gap("routine:{name}"))` | `cargo test --features cli --test cli` **53 passed / 3 failed**，三条恰是：新哨兵 `plan::should_not_report_a_missing_object_for_a_builtin_expression_call`（`MISS routine:trunc <- prc_builtin`）+ 两条既有 `call` 契约（`should_exit_1_with_the_missing_relation_when_deps_are_registered_but_tables_are_absent`、`should_ignore_ddl_and_seed_files_given_to_call`） |

M2 的第三条证人最有价值：语料夹具里 `nvl` 一类改名后的内建**确实出现**在表达式中，
放宽规则会把它们判成缺件——"保守"不是偷懒，是被这两条既有契约钉住的口径。

### V34.4 语料侧：计划清单变大、缺件仍为 0、核验面逐字未变

考题语料 10 条 `[[entry]]`（`--schema bigfund`）。"改动前"一列里 entry1/entry5 与 purchase 夹具
是**本轮实测**（把新循环摘掉后复跑同一条闭包），其余 8 条引 V32.4/V30.3 登记值，抽查吻合：

| entry | routines 前 → 后 | tables 前 → 后 | missing |
|---|---|---|---|
| `pkg_split_trade_step2.prc_pre_deal_shjycf` | **4 → 6**（一手） | 21 → 21 | 0 → 0 |
| `pkg_split_trade_step2.prc_pre_deal_bjgsyh` | 7 → 10 | 28 → 29 | 0 → 0 |
| `pkg_split_trade_step3_sh.prc_trd_hz_byfund` | 7 → 14 | 27 → 39 | 0 → 0 |
| `pkg_split_trade_step3_sh.prc_trd_deal_purchase` | 2 → 6 | 7 → 7 | 0 → 0 |
| `pkg_split_trade_step5.prc_split_sh_cjqs` | **7 → 24**（一手） | **20 → 41**（一手） | 0 → 0 |
| `pkg_split_qq.prc_general_deal_inst_sh_{buy,sell,other}` | 3 → 3（三条同） | 14 → 14 | 0 → 0 |
| `pkg_inst_domestic_purchase.prc_gen_inst` | 4 → 4 | 20 → 20 | 0 → 0 |
| `pkg_split_trade_step9.prc_deal_bond_repurchase_inst` | 3 → 4 | 23 → 24 | 0 → 0 |
| `case_corpus_purchase` 夹具同名 entry | **2 → 5**（一手） | 1 → 1 | 0 → 0 |

新进来的东西正是盲区藏的那一类（entry5 的 diff：例程源 +17 个文件、DDL +21 个文件，共 38 条），
前 5 个名字足以说明性质——`0_SIMPLIFIED_fnc_check_shghf_alter_date.sql`、
`0_SIMPLIFIED_fnc_com_get_between_days.sql`、`0_SIMPLIFIED_fnc_com_get_between_jxdays.sql`、
`0_SIMPLIFIED_fnc_com_getday.sql`、`0_SIMPLIFIED_fnc_com_if_deal_date.sql`：日历/区间这类
公共函数只在表达式里被调，`deploy` 出来的库过去**没有它们**，跑到的那条路径必然撞内核。

核验面（`verify`，采集面之一 `routine_refs` 未动 ⇒ 判定不变，仍复跑确认）：

| 语料 | V33 之后 | V34 之后 |
|---|---|---|
| `golden/exam_split_trade`（10 entries） | `entries=10 checks=222 failed=0 skipped=9` | 逐字相同 |
| `golden/case_corpus_purchase`（1 entry） | `entries=1 checks=11 failed=0 skipped=1` | 逐字相同 |

字节可复现（同一 entry 连跑两次 `cmp`，并与本轮第一次跑的结果比）：

```text
$ spinduck plan --dir golden/exam_split_trade --schema bigfund pkg_split_trade_step5.prc_split_sh_cjqs
plan: ….spinduck/plan/pkg_split_trade_step5.prc_split_sh_cjqs.toml (unchanged)
$ cmp r1.toml r2.toml && echo IDENTICAL ; cmp r2.toml first_run.toml && echo SAME_AS_FIRST_RUN
IDENTICAL / SAME_AS_FIRST_RUN
```

残余盲区的**大小**有数：把 M2（未命中即报缺件）在同一条闭包上跑考题语料，9 条 entry
共 23 行 `MISS routine:`，去重后只有 4 个名字——`coalesce`、`now`、`round`、`substring`，
全部是内建或规则改名后的内建（`NVL`→`coalesce`、`SYSDATE`→`now`、`SUBSTR`→`substring`）。
即"裸名调到登记为 `pkg.fnc_x` 的同包成员函数"这一形态在本语料为 **0 条**（一手，V34.1 口径表的最后一行）。

计划耗时（debug 构建，单次采样）：entry5 `2.67s → 2.69s`、entry1 `0.56s → 0.54s`——
每条例程的 IR 遍历从 2 趟变 3 趟，但被遍历的例程数仍是闭包规模决定，实测无可分辨差异
（另注：一次采样曾得 4.30s，本机噪声量级即此，故不给倍数）。

### V34.5 发版形态复核

`cargo build --release --features cli --locked`（`spinduck 0.1.2`）四条面各自实测：

| 面 | 命令 | 结果 |
|---|---|---|
| 计划缺件 | `plan --dir /tmp/a25c pkg_a.prc_outer`（`t_hidden` 的 DDL 文件移走） | `routines=2 tables=0 missing=1` + `MISS relation:t_hidden <- fnc_hidden` |
| 执行门 | `call --file 10_outer.sql --file 20_hidden.sql pkg_a.prc_outer 0` | `MISS relation:t_hidden <- fnc_hidden` + `call: … missing=1 未执行` rc=1 |
| 端到端 | `plan` → `deploy --db` → `call --plan … --db … 0` | `OUT p_o = 3` |
| 语料 | `plan --dir golden/exam_split_trade --schema bigfund …prc_split_sh_cjqs` | `routines=24 tables=41 missing=0` |

残余①在发版产物上同样成立（被调函数源码没进注册面时门无从判断）：
`call --file 10_outer.sql --file t_hidden.sql pkg_a.prc_outer 0` ⇒
`[KERNEL-0010] Catalog Error: Scalar Function with name fnc_hidden does not exist!`。

### V34.6 门禁（本机）

| 门禁 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 干净（本轮 `cargo fmt` 只动了新增测试与 `verify.rs` 的 use 列表换行） |
| `cargo clippy --all-features -- -D warnings` | 0 warning |
| `cargo test --features native` | **328 passed / 0 failed**（V33.7 的 327 + `verify::chain` 1 条） |
| `cargo test --features cli` | **384 passed / 0 failed**（= 328 + 契约层 56） |
| `cargo audit --no-fetch --stale` | 与 V33.7 同一集合：4 条全部 `zip 6.0.0`，**零新增**；本轮未加依赖 |
| `cargo build --release --features cli --locked` | 通过，V34.5 四场面即该产物 |

### V34.7 仍是推算的 / 明确未做

- **真实例侧完全未碰**：门与计划现在都只证明"项目 DDL 声明过 / 内核目录此刻有"，
  实例里那张表到底存不存在属 A0b/A1（实例未到手）。`call --db` 面在真实例上的缺件判定
  **推算**为不受影响（走同一个 `kernel_objects` 采集，输入是内核 catalog）。
- **残余②（同包成员函数裸名调用）不改**：收它要动 `eval_funccall` 的分派（加包名补全），
  属引擎行为改动，且与 `verify::run::resolve_routine` 的语句级口径并不等价——本轮按
  "门不比它守护的那次调用更宽或更严"钉死在精确查名，若将来要收口，另立编号并同步
  `kernel_gaps` 与 `build` 两侧。本语料实测该类为 0 条，故**推算**对现有夹具影响为 0。
- **交付件全量语料（14,611 文件）未跑 `plan`**：那个目录没有 `spinduck.toml`，
  就地建项目会改动交付件目录本身（AGENTS §6 语料纪律），本轮只在两个在库夹具上取证。
  ⇒ "真实交付代码里表达式级被调函数的命中量"仍是**推算**（按考题语料形态：日历/区间公共函数
  几乎必然命中，V34.4 的 17 个新例程源即证据链）。
- **M1/M2 两处变异不是发布形态**：它们只为证明断言非虚，跑完即还原；
  还原由 `grep -c expr_func_names src/plan/closure.rs == 2`（use 列表 + 循环）核对。
- **视图体展开的表达式调用面未扩**：`expand_view_bodies` 仍只看 SQL 文本的关系位，
  视图体里不会有 PL 表达式，故**推算**无影响。
- 计划文件的 `[[missing]]` 形状、`deploy` 非幂等、`plan.schema` 不下发三条限制**未变**
  （V31.5/V32.8 口径仍适用）。

### V34.8 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/verify/chain/mod.rs` | `Walk.expr_names` 面 + `walk_expr(&mut Walk)` + `pub fn expr_func_names`；`routine_refs` 的既有口径条文补一句"裸名候选由谁承担" | 新能力（采集面），**判定面未动** |
| `spinduck/src/verify.rs` | `pub(crate) use chain::expr_func_names` | 接线 |
| `spinduck/src/plan/closure.rs` | `walk_chain` 新增候选解析段（BFS 本体仍是那一份） | 行为改动（计划/部署/门三处同时） |
| `spinduck/tests/cli/plan.rs` | +3 条契约（缺件面 / 干净面 / 内建哨兵）+ `ROUTINES_EXPR_*`、`ROUTINE_FNC_HIDDEN` 夹具 | 新增测试 |
| `spinduck/tests/cli/call.rs` | +1 条门契约（红面逐字 + 不许出现 `KERNEL-0010`） | 新增测试 |
| `spinduck/src/verify/chain/tests.rs` | +1 条采集面单测 | 新增测试 |
| `spinduck/tests/cli.rs` | 头部实证索引补 V34 一行 | 文档性 |
| `docs/architecture.md` | §13.7：判定行 + 「已知盲区」条文改判为两条（进闭包 / 仍看不见的两类） | 设计正文 |
| `docs/validation.md` | 本节 V34 | 实证登记 |
| `docs/design-production-gaussdb.md` | §1 两行（源码进引擎 / A2 本机项）、§7 A2 行与退出判据、§8 盲区行改判 | 仅设计文档 |

---

## V35. A5 本机半边落地：`query` 把一条 SQL 的结果落成快照，`compare` 按值裁决；归一两份副本收口成一份

本轮收 design §6.2 / §7 A5 的**本机半边**：结果快照（`query`）与逐值比对（`compare`）。
实例半边（从 GaussDB 取数、`run <plan>` 端到端编排）仍卡在 A0b/A1/A3，本轮不碰。
另一条同时收的旧账是 `norm`：`tests/common/mod.rs` 与 `tests/corpus_forms/mod.rs` 各有一份
实现，本轮搬到 core 成唯一一份，两处测试目录改成 re-export。搬动是**纯收口**，等价性的
一手证据在 V35.3（diff 原文 + 三层复跑 + 容器黄金层）。

### V35.1 交付面

| 项 | 内容 |
|---|---|
| 归一收口 | `spinduck/src/norm.rs`（118 行）：`norm` / `norm_value` / `norm_scalar` + 8 条单测；`tests/common/mod.rs:53`、`tests/corpus_forms/mod.rs:153` 改成 re-export，**全文再无第二份 `fn norm(`**（`grep -rn "fn norm(" src tests \| grep -v ^src/norm.rs` ⇒ 空） |
| 快照载体 | `spinduck/src/snapshot/mod.rs`（460 行）：`Snapshot{schema, rows}`、`from_json_str` / `to_json_string`（单元格是内核给回的**文本**，DB NULL 是 JSON `null`）、`capture`（一句 SQL → 全量行）、`statements`（引号/注释安全的切句）——5 + 8 + 1 条单测 |
| 裁决 | `spinduck/src/snapshot/compare.rs`（470 行）：`compare`（位置序）、`compare_by_key`（键序，`--key`）、`Diff{kind,row,column,a,b}` 四型（value/arity/only-in-a/only-in-b）——17 条单测 |
| 壳 | `src/cli/query.rs`（55 行）、`src/cli/compare.rs`（75 行）：只做读入、渲染、退出码；判据一条都不在壳里（design §5.2 D1） |
| 参数面 | `src/cli/args.rs:113-114` 两条 dispatch + `parse_query` / `parse_compare` + `Command::Query/Compare` + `usage()` 两行；`src/cli/run.rs` 两个 arm 接线；+4 条解析单测 |
| 契约层 | `tests/cli/query.rs` 8 条、`tests/cli/compare.rs` 10 条 ⇒ 契约层 **56 → 74** |
| 内核取证 | `snapshot::kernel_evidence_tests::should_run_every_statement_and_return_only_the_last_result_set`（`native` gate，见 V35.2 循环 3） |
| 文档 | architecture §13.9（新增，`query`/`compare` 口径）；design §1 / §6.2 / §7 A5 / §8 改判 |

规模口径：新文件最长 470 行，§3 的 500 行上限与"含 `#[cfg(test)]` ≤700"都在面内。

### V35.2 TDD 循环与 Red 原文

**循环 1（归一收口，Refactor 型）** — 这条不是新行为，是"唯一实现"这条结构约束的取证。
本轮反向复跑了一次：把 `src/lib.rs` 的 `pub mod norm;` 注释掉，消费者立刻编译不过——

```text
error[E0432]: unresolved import `crate::norm`
  --> src/snapshot/compare.rs:10:12
```

合法 Red 形态（AGENTS §2：缺 API 导致编译失败即合法）。第一个红的是 lib 自己
（`error: could not compile spinduck (lib)`，`src/snapshot/compare.rs:10` 那条 use），
所以这条证明的是"core 里那一份在被真实使用"。至于**只剩一份**，钉住它的是 V35.1 那条
grep（全文再无第二处 `fn norm(`）与两处 re-export 的行号，不是这条编译错——两者别说
不同的事，别把前者读成后者。

**循环 2（CLI 面）** — 实现落地后摘掉 `args.rs` 的两条 dispatch 复现 Red 原文：

```text
$ cargo test --features cli --test cli          # 摘掉 "query"/"compare" 两行
test result: FAILED. 56 passed; 18 failed
---- query::should_dump_the_result_set_of_one_sql_file_as_a_snapshot stdout ----
assertion `left == right` failed: [ENG-0010] usage: unknown subcommand query
spinduck <command>
```

18 条红的**恰好**是新增的 8 条 `query` + 10 条 `compare`（名字逐条核过），既有 56 条一条不红
——这就是"新增两条子命令对既有 CLI 面零影响"的正面证据，不是"看起来没红"。

**循环 3（多语句拒绝，前提改判）** — 这条契约的 Red 不是实现前的红，是**前提被内核证伪**后的
改判：第一版实现把整段文本交给内核、指望它报语法错，实测 rc=0、快照文件照样落盘：

```text
$ spinduck query env.duckdb --sql-file two.sql --out dirty.json
query: rows=1 cols=1 out=dirty.json            ← 内核跑了两句，只回最后一句
```

于是采集面改成"发出去之前切句判定"（`statements`，引号 `''`/`""`、`--`、`/* */` 安全），
内核那条事实本身永久钉在 `kernel_evidence_tests` 里：

```text
test snapshot::kernel_evidence_tests::should_run_every_statement_and_return_only_the_last_result_set ... ok
```

单测侧的对应断言不止"报红"，还断言 `ex.calls().is_empty()`——**判红不许先把第一句发给内核**，
因为侧效应（INSERT）一旦发生就无法回退。

**循环 4（字节契约的结尾换行）** — 发版冒烟里 `cat` 把快照和下一条提示符黏在一起，暴露
`to_json_string` 缺结尾 `\n`。改判先落测试（2 条 core 字节断言 + 2 条 CLI 字节断言改成
`}\n` 结尾），再改实现。四条断言现在分别覆盖"有行"和"空 rows"两条落盘路径（V35.4 的
M5/M5b 各只红自己那一对）。

### V35.3 归一收口的等价性：diff 原文 + 怪癖锁定

两份副本与 core 实现的 diff（`git show HEAD:…` 取旧体，去掉参数名差异后逐字比）：

```text
### tests/common/mod.rs 旧体 vs src/norm.rs
2c2
<     let Some(s) = v else {          ← 只是形参名 v → s
5c5
<     let t = s.trim().trim_start_matches('+');
>     let t = t.trim().trim_start_matches('+');
### tests/corpus_forms/mod.rs 旧体 vs src/norm.rs
5a6
>     // A 模式（Oracle 语义）：空字符串即 NULL，两侧归一同口径   ← 只是补的一行注释
```

即：**语句序列完全相同**，差的只有标识符命名与一行注释 ⇒ 同输入必然同输出（结构论证）。
行为侧另给一手证据：用 `norm` 的三层复跑逐条绿——`corpus_sql_forms` 9/0、`cases_native` 4/0、
`gauss` 黄金层 6/0（V35.6），期望值一个没收。

怪癖照搬并锁进单测，不改判：

```text
norm(Some("-.0")) == "-0"     // 负号在去尾零后不消失：-.0 → -0.0 → -0
norm(Some("100")) == "100"    // 无小数点不去尾零（否则 100 会被削成 1）
```

前者锁在 `should_strip_trailing_zeros_and_dot`，后者锁在 `should_keep_integer_text_untouched`。

### V35.4 非虚断言：六处变异各自只红该红的用例

每条都是临时改实现、跑 `cargo test --features cli --no-fail-fast`、还原（还原由 `cmp` 与
改前备份**逐字节相同**核对，并复跑 `294 lib + 74 契约` 全绿）。基数 445。

| 变异 | 改法 | 结果 | 红的用例（逐字） |
|---|---|---|---|
| M1 摘掉多语句拒绝 | `many => {return Err(…)}` 改成 `many => many[many.len()-1]`（静默只快照最后一句） | **443/2** | `snapshot::capture_tests::should_refuse_a_file_with_more_than_one_statement_before_running_any`、`query::should_reject_a_file_that_holds_more_than_one_statement` |
| M2 摘掉归一（改逐字比较） | `compare_pair` 里 `norm(…)` 换成 `clone().unwrap_or("NULL")` | **442/3** | `snapshot::compare::tests::should_judge_values_after_norm_not_by_text`、`…::should_normalize_key_cells_before_matching`、契约 `compare::should_exit_0_when_values_match_after_norm` |
| M3 未知 `--key` 列退化 | `key_indices` 的 `ok_or_else(ENV-0031)` 换成 `.unwrap_or(0)`（悄悄拿首列） | **443/2** | `snapshot::compare::tests::should_reject_a_key_column_that_is_not_in_the_schema`、契约 `compare::should_reject_a_key_column_that_is_not_in_the_snapshot` |
| M4 早停 | `compare` 末尾 `out.truncate(1)`（只报第一条差异） | **443/2** | `snapshot::compare::tests::should_report_every_difference_instead_of_stopping_at_the_first`、契约 `compare::should_exit_1_and_list_every_value_difference` |
| M5 有行路径缺结尾换行 | `"\n  ]\n}\n"` → `"\n  ]\n}"` | **443/2** | `snapshot::tests::should_write_null_for_database_null_and_keep_cell_text_verbatim`、契约 `query::should_dump_the_result_set_of_one_sql_file_as_a_snapshot` |
| M5b 空 rows 路径缺结尾换行 | `"]\n}\n"` → `"]\n}"` | **443/2** | `snapshot::tests::should_read_an_empty_result_set_as_zero_rows`、契约 `query::should_snapshot_an_empty_result_set_as_zero_rows` |

三条"静默退化"里最有价值的是 M3：它把"列名不存在"从红变成**按第一列照样比、照样能绿**，
这在验证场景里是最坏的一类假绿。契约把它钉在 `ENV-0031` 且消息里列出有的列名，
`compare` 因此**永不**从键序退化成位置序。

### V35.5 发版形态冒烟（`cargo build --release --features cli --locked`，`spinduck 0.1.2`）

夹具：`t_a(id INT)` + `prc_ins`，`plan → deploy --db work.duckdb → call … 1 / … 2` ⇒ 库里两行。

| 面 | 命令 | 实测输出（逐字） | rc |
|---|---|---|---|
| 快照落盘 | `query work.duckdb --sql-file q.sql --out a.json` | `query: rows=2 cols=1 out=a.json` | 0 |
| 字节可读物 | `od -c a.json \| tail -1` | `] \n   } \n`（结尾确有换行） | — |
| 多语句拒 | `query work.duckdb --sql-file two.sql --out dirty.json` | `[ENV-0031] query: SQL 文件里有 2 条语句，一次只跑一条（拆开写）`；随后 `ls dirty.json` ⇒ `No such file or directory` | 1 |
| 按值绿 | `compare a.json b.json`（b 写 `["1.0"]`、`["+2"]`） | `compare: rows=2/2 diffs=0` | 0 |
| 按值红 | `compare a.json c.json`（c 第二行 `["3"]`） | `DIFF value row=2 col=id a=2 b=3` + `compare: rows=2/2 diffs=1 逐值不一致` | 1 |
| 键序 | `compare k1.json k2.json --key k`（两侧到达序相反） | `compare: key=k rows=2/2 diffs=0` | 0 |
| usage | `query work.duckdb --sql-file q.sql`（缺 `--out`） | `[ENG-0010] usage: query <本地 DuckDB 文件> --sql-file <F.sql> --out <F.json>（缺 --out）` | 2 |
| usage | `compare a.json`（只给一份） | `[ENG-0010] usage: compare <a.json> <b.json> [--key k1,k2]（实际给了 1 个快照文件）` | 2 |

契约层同款形状的整行/列数不等红（`tests/cli/compare.rs:71`、`:130`）：

```text
DIFF only-in-a row=2 a=[2]
compare: rows=2/1 diffs=1 逐值不一致
DIFF arity row=1 a=2 列 b=3 列
compare: rows=1/1 diffs=1 逐值不一致
```

### V35.6 门禁（本机）

| 门禁 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 干净 |
| `cargo clippy --all-features -- -D warnings` | 0 warning |
| `cargo test --features native` | **371 passed / 0 failed**（V34 的 328 + 43：core 39 条 + `cli::args` 4 条） |
| `cargo test --features cli` | **445 passed / 0 failed**（= 371 + 契约层 74） |
| `cargo test --features cli --test cli` | **74 passed / 0 failed**（V34 的 56 + query 8 + compare 10） |
| `cargo test --features gauss` | **377 passed / 0 failed**（= 371 + 实例黄金层 6；容器 `spinduck-gaussdb` healthy，口令与端口由 `docker inspect` / `docker port` 在命令替换里取，全程未打印） |
| `cargo audit --no-fetch --stale` | 与 V33/V34 同一集合：4 条全部 `zip 6.0.0`，**零新增**；本轮未加依赖 |
| `cargo build --release --features cli --locked` | 通过，V35.5 八场面即该产物 |

`gauss` 层本轮有一次**配置错被读成行为错**的踩坑，登记在此以免下轮误判：第一次跑取
`SPINDUCK_OG_PORT` 时直接读了容器的 `GS_PORT`（那是**容器内**端口 5432），宿主映射其实是
`127.0.0.1:5433`，6 条全红：

```text
thread '…' panicked at tests/common/mod.rs:46:75:
connect gaussdb: EngineError { domain: Env, code: ErrorCode("ENV-0020"), message: "gaussdb: error connecting to server" }
```

改成 `docker port` 反查宿主端口后同一批全绿——红面是 `ENV-0020`（环境域）而不是任何比对断言，
这条区分本身也是故障域归位在实证里的用处。

### V35.7 仍是推算的 / 明确未做

- **实例半边未动**：`query` 的 `--db` 只接本地 DuckDB 文件；"从 GaussDB 取同一份形状的快照"
  依赖 A3 的 COPY OUT/流式通道，本轮不预判其口径。`run <plan>` 端到端编排同样未做。
- **千万级内存上限是推算的**：`capture` 把 `RowStream` 全量落进 `Vec`（接缝本来就是全量语义），
  本机只一手证明了"不是截断分页"（`should_fetch_every_row_not_a_capped_page`，5000 行）。
  千万行会不会 OOM 属**推算**，结论挂在 A3：大结果集必须走 COPY OUT 而不是这个接缝。
- **口径差异是选择不是缺陷**：`compare` 判**值**（经 `norm`），列名不判、列数判；
  这与 golden 层的逐字基线口径不同，已在 architecture §13.9 写明理由（基线常用 `cnt`、
  本地给回 `n` 是常态）。
- **一次一句的代价**：多步验证要多次 `query` 调用或多份 SQL 文件；换来的是"判红不产生侧效应"，
  本轮认为这笔交换值得，口径写在 §13.9。
- **GBK 语料在 `query` 面判红**：SQL 文件必须能按 UTF-8 解码（契约
  `query::should_refuse_a_gbk_sql_file_because_the_statement_must_be_decoded`）。语料侧真 GBK
  解码走 D9② 那条已落地的读入收口，但**快照面不接非 UTF-8 语句文本**是本轮口径。
- **`--key` 的多列/重复键**由单测覆盖（`should_use_every_key_column_when_matching`、
  `should_use_every_key_column_and_compare_duplicates_in_file_order`），真实例上的键分布**未测**。
- **M1–M5b 六处变异不是发布形态**：只为证明断言非虚，跑完即还原，还原证据是 `cmp` 逐字节相同
  + 全绿复跑。

### V35.8 本轮改动落点

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/norm.rs` | 新增：唯一一份 `norm`/`norm_value`/`norm_scalar` + 8 条单测 | 收口（从两份测试副本搬入，语句序列未变） |
| `spinduck/tests/common/mod.rs` | `pub use spinduck_core::norm::norm;` 替掉本地副本 | 收口接线 |
| `spinduck/tests/corpus_forms/mod.rs` | `pub use spinduck_core::norm::{norm, norm_scalar, norm_value};` 替掉本地副本 | 收口接线 |
| `spinduck/src/snapshot/mod.rs` | 新增：`Snapshot` + JSON 契约 + `capture` + `statements`；`kernel_evidence_tests` 永久钉内核事实 | 新能力 |
| `spinduck/src/snapshot/compare.rs` | 新增：位置序 / 键序逐值裁决 + 四型 `Diff` | 新能力 |
| `spinduck/src/cli/query.rs`、`src/cli/compare.rs` | 新增壳：读入→core→渲染→退出码 | 新能力（零判据） |
| `spinduck/src/cli/args.rs` | `Command::Query/Compare` + `parse_query`/`parse_compare` + dispatch + usage + 4 条解析单测 | 新能力 |
| `spinduck/src/cli/run.rs` | 两个 arm 接线 | 接线 |
| `spinduck/src/lib.rs` | `pub mod norm;`、`pub mod snapshot;` | 接线 |
| `spinduck/tests/cli/query.rs`、`tests/cli/compare.rs` | 新增 8 + 10 条契约（含 `seeded_db` 夹具复用） | 新增测试 |
| `spinduck/tests/cli.rs` | 头部实证索引 + 职责清单补 `query`/`compare` | 文档性 |
| `docs/architecture.md` | 新增 §13.9（口径四段表 + 退出码）；§13.8 表行扩为"执行/计划/部署/核验/比对面" | 设计正文 |
| `docs/validation.md` | 本节 V35 | 实证登记 |
| `docs/design-production-gaussdb.md` | §1 A5 行、§6.2 口径、§7 A5 本机半边判据、§8 残余 | 仅设计文档 |
## V36. 与主干 V26（性能轮 / `v0.1.3`）合并：编号让位、两套 CLI 契约并存，全部门禁在合并树上重取

**为什么有这一节**：PR #8 的基线是 `cbaaae5`（V25 之后），主干随后合入 PR #7 =
`bc887de`（三段引擎优化 + 批量搬运，实证登记 **它自己的 V26**，版本 bump 到 0.1.3）。
两条线都在 V25 后面追加了一节 `## V26`，且都改了 `call` 的 CLI 面与 `tests/cli.rs`。
CI 因此根本不启动：`gh pr view 8 --json statusCheckRollup` 实测返回 `[]`，
`mergeStateStatus = CONFLICTING`。**先合并、再取证**是唯一的顺序（AGENTS §9 的
"tag 打在 CI 已验证的 main head"同样要求分支先能对上 main）。

**冲突面（一手：`git pull --no-rebase origin main` 的实测输出）**

| 文件 | hunk | 接法 |
|---|---|---|
| `.github/workflows/ci.yml` | 1 | 采主干：Test job 单步 `cargo test --features cli`（`cli ⊇ native`），注释里主干那两步并存的口径退役 |
| `spinduck/Cargo.toml` | 1 | 本分支的 `[[test]] guard_patterns_outer_join_residual` 保留 + `[[test]] cli` 的 `required-features` 理由改写为合并后口径 |
| `spinduck/src/cli/args.rs` | 3 | `parse_call` 头（`--plan` 预摘 + 两面旗标）、`usage()` 文本、测试簇（两条都留，主干那条的期望换成 `target: CallTarget::Routine(...)`） |
| `spinduck/src/cli/run.rs` | 1 | `eng.batch_commit` / `eng.step_budget` 赋值挪到注册循环**之前**，源码循环仍用本分支的 `sources` + `read_source_strict` |
| `spinduck/tests/cli.rs` | 1（add/add） | 采本分支的"夹具 + `#[path]` 分派器"结构，主干那 3 条二进制契约折进 `tests/cli/call.rs` |
| `docs/validation.md` | 1（大块） | 编号让位，见下 |

**编号让位的判定**：主干的 V26 已在 `origin/main` 并随 `v0.1.3` 发布，引用面一手计数
= 该节内 **49 处** + V23 区前向引用 **2 处**（`> 后续事实（2026-11-08，V26.5 回写）`，
指的是主干自己那节的 sha256/参照实例口径）；本分支的 V26–V34 只活在未合并的 PR 里。
已发布的证据编号不动，未发布的让位——所以**我这侧整段 +1**（V26→V27 … V34→V35），
并把主干那节排在前面，恢复"编号单调 = 文件顺序单调"这条排版不变量。

改写面（脚本单次遍历 `V(2[6-9]|3[0-4]) → V(n+1)`，实测计数）：

| 文件 | 改写 | 说明 |
|---|---|---|
| `docs/validation.md` 本分支段 | 240 处 | 含 9 个 `## V..` 小节标题与全部自引用 |
| `docs/design-production-gaussdb.md` | 127 处 | 该文件整体是本分支新增，引用只可能指本分支各节 |
| `docs/architecture.md` | 9 行 | 只改指向我这几节的行；主干那 3 行（`V26` 批量 COMMIT、`V26.3/V26.7`、`V26` BATCH_SKIP）逐字未动 |
| `spinduck/tests/cli.rs` | 8 行 | 头部实证索引 + 两条夹具注释 |
| `spinduck/tests/cli/{call,compile,deploy,rules_explain,verify}.rs`、`tests/guard/guard_patterns_outer_join_residual.rs` | 6 + 2 行 | 注释里的节号 |
| `spinduck/src/{cli/args.rs,sql/patterns.rs,verify/deploy.rs,verify/chain/tests.rs}` | 各 1 行 | 同上 |
| 主干侧引用面（`README.md` 3 行、`AGENTS.md` 1 行、`ci.yml` 1 行、`src/io/gauss.rs` 4 行、`src/exec.rs` 1 行、`src/cli/args.rs` 的 batch_commit 文档行 1 行） | **0 改动** | 这些 "V26" 全部指主干那节 |

判据：合并后的文件里 `^## V` 序列实测为 `1..35` 且单调（脚本断言 `monotonic: True`），
`V26` 只剩一节。

**代码接合的 Red 口径（诚实说明）**：上表前三行是**接合**，不是新行为——两侧原有测试
各自原样保留、合并后全绿，没有需要复现的 Red；`call --plan` 的位置参数口径与本分支
V32（原 V31）那条 `should_take_every_positional_as_an_argument_when_call_uses_a_plan`
一致，主干的 `should_parse_call_with_step_budget` 只是把期望里的 `routine:` 换成
`target: CallTarget(...)`，断言强度未变。

**本轮唯一新增行为 = 计划面也吃下执行面开关**（两条线各自的测试面都覆盖不到的组合）：
`spinduck/tests/cli/deploy.rs` 的
`should_apply_the_execution_flags_on_the_plan_face_too`（1 条，两半）。

- 半边 1：`deploy` 出计划与落盘库后，`call --plan P --db D --batch-commit --step-budget 1000000 0`
  的 stdout 与不带开关时**逐字相同**（`OUT p_o = 1\n`），stderr 恒空。
- 半边 2（判据的判别力所在）：`call --plan <不存在的文件> --step-budget 0` ⇒
  **退出码 2（usage）**、消息含 `--step-budget`、且**不含** `[ENV-0010]`——用法判定排在计划 IO 之前。
- 非空断言证据（变异）：删掉 `src/cli/args.rs` 里 `--step-budget` 的 `if v == 0 { … }` 守卫后单跑该条，
  实测转红：`assertion left == right failed: usage 要先于计划读取: [ENV-0010] read /tmp/.../nope.toml: No such file or directory`
  + `left: 1 / right: 2`；随后 `cp` 还原并 `cmp` 确认逐字节一致。
  同一条路径单独给缺计划文件时仍是 `EXIT_FAIL + [ENV-0010]`（既有契约
  `should_exit_1_with_env_0010_when_the_plan_file_given_to_call_is_absent`，未改判）。

**门禁（全部在合并树上重取，本机）**

| 门禁 | 命令 | 实测 |
|---|---|---|
| Format | `cargo fmt --all -- --check` | 干净（接合期间 `usage()` 与测试簇由 `cargo fmt --all` 收敛） |
| Clippy | `cargo clippy --all-features -- -D warnings` | `Finished`，零告警 |
| 单元+集成（内核通道） | `cargo test --features native` | **397 passed / 0 failed**（23 个 result 行全部 `0 failed`） |
| 含契约层（CI Test 同口径） | `cargo test --features cli` | **475 passed / 0 failed**，其中 `tests/cli.rs` 收集 **78 条** = 上一轮实测 74（`1bcec83` 提交说明同数）+ 主干折入 3 + 本轮新增 1 |
| 黄金层（A 模式容器） | `cargo test --features gauss`（`SPINDUCK_OG_PORT` 取自 `docker port spinduck-gaussdb 5432/tcp` = 5433；口令经 `docker exec … printenv GS_PASSWORD` 注入环境，不落命令行、不入库） | **6 passed / 0 failed**（`gauss_e2e` 2、`golden_corpus_gauss` 2、`golden_corpus_purchase` 1、`golden_gauss` 1）；`gauss_sha256` 收集 0 条 = **feature 级门控**（该层要 `--features gauss-sha256`），不是环境假绿 |
| Security Audit | `cargo audit` | 4 条公告，编号与 CI 豁免清单**逐条相同**（`RUSTSEC-2026-0367/0368/0369/0429`，构建期 `zip`）；豁免面未扩大 |

**发版二进制冒烟（`cargo build --release --features cli --locked`，合并树重取）**
契约层跑的是 `CARGO_BIN_EXE_spinduck`（release 目录下同一 target 的 debug 版），
下面这组跑的是**发版口径的 release 产物**，逐字记录：

```text
$ spinduck version
spinduck 0.1.3                                    ← 主干 bump 的结果，二进制内自述一致

$ spinduck help | grep -n 'call '
20:  call [--db <f>] [--rules <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] <routine> [args...]
21:  call --plan <计划.toml> [--db <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] [args...]

$ spinduck plan prc_seed_count | tail -1
plan: target=prc_seed_count routines=1 tables=1 missing=0
$ spinduck deploy <plan> --db work.duckdb | tail -1
deploy: target=prc_seed_count ddl_files=1 routine_files=1 db=/tmp/.../work.duckdb
$ spinduck call --plan <plan> --db work.duckdb --batch-commit --step-budget 1000000 0
OUT p_o = 1                                       rc=0
$ spinduck call --plan <plan> --step-budget 0
[ENG-0010] usage: --step-budget <n>（正整数）      rc=2   ← 未碰计划文件，见 V36 变异取证

$ spinduck query work.duckdb --sql-file q.sql --out a.json
query: rows=1 cols=1 out=/tmp/.../a.json          ← `od -c` 末字节仍是 \n
$ spinduck compare a.json b.json ; echo rc=$?     → compare: rows=1/1 diffs=0 / rc=0
$ spinduck compare a.json c.json --key n          → DIFF only-in-a row=1 a=[1] / DIFF only-in-b row=1 b=[9]
                                                     compare: key=n rows=1/1 diffs=2 逐值不一致 / rc=1

$ spinduck call --plan <缺件计划> --db work.duckdb --batch-commit --step-budget 1000000 0
MISS relation:t_a <- prc_seed_count
MISS sequence:seq_a <- prc_seed_count
call: target=prc_seed_count missing=2 拒绝执行（补齐缺件后重跑 plan）   rc=1
ls work.duckdb → No such file or directory        ← 红之后不留半成品库
```

两面并存最直接的产物侧证据：release 二进制上 `call --plan` + 两个执行面开关同条命令生效，
且缺件门仍然早于开库（rc=1 且没有库文件）。

**CI 一手日志（run `42779751418`，head `aa8588f`，五个 job 全 `success`）**

推送后 `gh pr view 8 --json mergeStateStatus,statusCheckRollup` 的形态先变：从
CONFLICTING + `statusCheckRollup: []`（CI 根本不启动）变成 UNSTABLE + 五个 job
`IN_PROGRESS`，最终 `mergeStateStatus = CLEAN`、`mergeable = MERGEABLE`。
逐 job 核对（`gh run view 42779751418 --json conclusion,jobs`）：
`audit / format / golden / test / clippy` 五个 conclusion 全 `success`，run conclusion `success`。

不采信绿勾，直接查断言输出（`gh run view 42779751418 --log`，按 job 切分后聚合）：

| job | 一手输出 | 与本机对照 |
|---|---|---|
| `test` | `test result: ok. 78 passed; 0 failed`（`Running tests/cli.rs`）；该 job 全部 result 行求和 **475 passed**、`0 failed` 行数 23 | 本机 `--features cli` 同为 **475 / 0**，契约层同为 **78** |
| `golden` | 步骤命令行 `cargo test --features gauss -- --nocapture`；套件 `gauss_e2e` 2、`golden_corpus_gauss` 2、`golden_corpus_purchase` 1、`golden_gauss` 1 逐条 `... ok`；该 job 求和 **403 passed** | 本机 `--features gauss` 同为 **403 / 0**（黄金层同样 6 条） |
| `clippy` / `audit` / `format` | 无 result 行（各自的判据是零告警 / 豁免外零公告 / 零 diff） | 本机同口径全绿 |

整份日志 `grep -cE "test result: FAILED|panicked"` = **0**。两个 job 的总数与本机逐项相等，
说明 CI 与本地不是"两套口径刚好都绿"，而是同一条命令面（`--features cli` / `--features gauss`）。

**追平到 head SHA 的那一次（run `42780608895`，head `cde7ae1`）**：上面这张表核的是 `aa8588f`，
而 PR 的最终 head 因这两个纯文档提交后移了两次，所以把同一套抓取重跑一遍——
`gh run view 42780608895 --log` 按 job 聚合：`test` **475 passed**、
`Running tests/cli.rs` 仍是 `test result: ok. 78 passed; 0 failed`、`golden` **403 passed**、
全日志 `test result: FAILED|panicked` 计数 **0**，与上表逐项相等；
`gh pr view 8` 侧 `mergeStateStatus = CLEAN`、`mergeable = MERGEABLE`。
即**"CI 已验证的 head SHA"这条口径成立的是 `cde7ae1`**（数字面同一棵代码树，文档提交不动判定面）。

更早的合并提交那一次（run `42779313763`，head `d6e58bc`）`conclusion = success`、五 job 全绿，
但那一轮只核到 job conclusion、没抓断言输出，故不作为"CI 已验证"的引用对象。
写下本段的这一个提交又是纯文档——按 V25 定的顺序，它不再往文档里追登记（否则无限递归），
终局口径是：**要 bump/tag 时，tag 必须指在当次 CI 已验证的 main head 上**（AGENTS §9）。

**仍是推算的 / 未覆盖**

- 把上面这段 CI 证据写进文档的提交本身会让 CI 再跑一次；那一轮是纯文档改动
  （`git diff` 只碰 `docs/validation.md`），不在本节的判定面内，也不在本节登记范围内。
- `--batch-commit` 与 `--step-budget` 在**千万级游标循环**上的实际放大效果仍无一手数字：
  本轮新增的组合契约只证明"计划面吃下开关、结果逐字不变"，性能与预算阈值属 A3/A4（需实例）。
- **AGENTS §3 规模**：合并后 `src/cli/args.rs` 859 行、`src/cli/run.rs` 588 行，均越过
  单文件 500 行（`args.rs` 连「含 `#[cfg(test)]` ≤700」也超）。越限点一手定位
  （`git show <rev>:<path> | wc -l`）：

  | ref | `args.rs` | `run.rs` |
  |---|---|---|
  | 主干 `bc887de` | 468 | 499 |
  | 本分支 `934d5ad`（合并前倒数第二个提交） | 423 | 488 |
  | 本分支 `1bcec83`（A2/A5 五条子命令那一次落地） | **812** | **577** |

  也就是说**越限不是历史累积，是本 PR 那一次 `feat` 提交造成的**（上面两个 ref 都在限内），
  合并只把它从 812/577 推到 859/588；前几轮汇报里「连续加子命令累积的」那句是错的口径，
  此处按实测改判。本轮不顺手拆：拆分是纯结构改动，要按 §3「按职责拆子模块」单独一轮走，
  且 `args.rs` 已含 `query`/`compare`/`plan`/`deploy`/`call` 五套解析器，切分线清晰
  （每个解析器一文件 + 共享夹具），适合作为下一轮的独立 PR。
- 主干那节的执行面数字（memo/prepared/COPY）在合并树上未复测——合并没碰它们的判据代码，
  但 `run.rs` 的注册循环换成了 `read_source_strict`（本分支 D9① 的判定），
  若主干曾按旧 `read_source` 元组接口计时，口径差异属读入路径，不影响其结论的判据面。

**改动落点**

| 文件 | 改动 | 性质 |
|---|---|---|
| `spinduck/src/cli/args.rs` | `parse_call` 合并两面旗标 + `--plan` 预摘；`usage()` 两条 call 行补开关；两条测试并存 | 接合 |
| `spinduck/src/cli/run.rs` | `call_cmd` 内开关赋值前置于注册循环；源码循环保留严格读入 | 接合 |
| `spinduck/tests/cli.rs` + `tests/cli/call.rs` | 主干 3 条二进制契约折进分派器结构（`cli()` 夹具：stdout/stderr 分离断言） | 接合 |
| `spinduck/tests/cli/deploy.rs` | 新增 1 条计划面组合契约 | 新行为 |
| `docs/validation.md`、`docs/architecture.md`、`docs/design-production-gaussdb.md`、若干注释行 | V26–V34 → V27–V35 编号让位 + 本节 V36 | 实证与引用面 |
| `.github/workflows/ci.yml`、`spinduck/Cargo.toml` | 采主干单步 `--features cli` 口径；`[[test]]` 注释改写 | 接合 |

---

## V37. 审核应修：`compare --key` 的键列只在本地侧校验 ⇒ 参照侧改名时产出 `diffs=0` 的假绿

**触发**：PR #8 的审核结论（`gh pr view 8 --json reviews` 一手：author `c2j`、state
`COMMENTED`、submittedAt `2026-11-10T09:22:14Z`）。🔴 一条：`compare_by_key` 调
`key_indices(a, key)` 只查左快照的 schema，参照侧从未校验；🟡 三条不阻断（规模越限、
CI 未跑、`Snapshot::from_json_str` 不校验行宽）。

**这条缺口是真的，且最坏的形态被 Red 一手抓到**。修法前的实现是：

```rust
let idx = key_indices(a, key)?;      // 只问左快照
… key_tuple(rb, &idx) …             // 却按同一组下标位置读右快照
```

三条单测先写、先跑，全部红（`cargo test --features cli --lib should_reject_a_key_column`，
一手 panic 原文，未截断）：

```
should_reject_a_key_column_renamed_only_in_the_reference_snapshot
  called `Result::unwrap_err()` on an `Ok` value: []
should_reject_a_key_column_missing_in_the_narrower_reference_snapshot
  called `Result::unwrap_err()` on an `Ok` value: [Diff { kind: OnlyInA, row: 1, … },
  Diff { kind: OnlyInA, row: 2, … }, Diff { kind: OnlyInB, row: 1, … b: "[x]" },
  Diff { kind: OnlyInB, row: 2, … b: "[y]" }]
should_reject_a_key_column_that_sits_at_a_different_position_on_the_right
  called `Result::unwrap_err()` on an `Ok` value: [Diff { kind: OnlyInA, row: 1, … },
  Diff { kind: OnlyInB, row: 1, … b: "[x, 1]" }]
```

第一行的 `Ok value: []` 就是审核点名的**假绿**：参照侧把键列改名成 `k_id`、列数不变，
`key_tuple` 按位置读到参照侧的第 0 列（真实键值 1、2），两侧值恰好对上 ⇒
`diffs=0`、退出码 0。这不是"比得不严"，是**把没比成说成比过了**——`compare` 模块头与
architecture §13.9 的「找不到即 `ENV-0031`，绝不退化成位置序」对参照侧本来就没生效。
第二、三行是同一条缺口的另外两种给法：参照侧更窄时右侧键恒 `NULL`，判成整片
`OnlyIn*` 洪水（报因指错方向）；参照侧只是换了位置时同样读到错列。

**修法**（`src/snapshot/compare.rs`，一个行为 = 键列两侧都校验）：新增 `key_indices_both`，
对 a、b 各跑一遍 `key_indices`，再要求两组下标**相等**（列序不一致时"第 j 列对第 j 列"
没有意义，故判红而不是报成 `DIFF`），`compare_by_key` 取它的返回值为唯一 `idx`。
错误码沿用 `ENV-0031`（§5 归域：输入形状不对这一类，与 `case.toml` 缺 `[case]` 同族），
消息带上参照侧真实列名——审核要求的"点名"落在诊断文本里，不是只落在码上。

三条单测（形态各一）+ 两条 CLI 二进制契约（`tests/cli/compare.rs`：参照侧改名、参照侧换位；
都断言 `[ENV-0031]` 且 stdout 不含 `diffs=` / `DIFF `，即**判红早于产出**）同批落地。
既有那条 `should_reject_a_key_column_that_is_not_in_the_schema` 用 `&a, &a` 同一份快照，
结构上看不见两侧的不对称——保留（它锁的是本地侧），缺口由新三条补。

**非空洞性（一手变异）**：把参照侧那遍改回 `let idx_b = idx_a.clone();`（等价于
"只校验左快照"，其余一字未动），5 条新测试全红、其余照绿：

| 套件 | 变异后 | 还原后 |
|---|---|---|
| `--lib snapshot::compare` | 17 passed; **3 failed** | 20 passed; 0 failed |
| `--test cli compare` | 10 passed; **2 failed** | 12 passed; 0 failed |

随后按字节还原（`cp /tmp/compare.rs.bak`），判据不是"看起来差不多"。

**门禁（本机，均在改动后的树上重跑）**

| 命令 | 结果 |
|---|---|
| `cargo fmt --all` + `--check` | 干净 |
| `cargo clippy --all-features -- -D warnings` | 无告警 |
| `cargo test --features cli --lib` | 319 passed; 0 failed（316 → +3） |
| `cargo test --features cli --test cli` | 80 passed; 0 failed（78 → +2） |
| `cargo test --features cli` | 480 passed; 0 failed（475 → +5） |
| `cargo test --features gauss --no-fail-fast` | 406 passed; 0 failed（口令由 `docker exec spinduck-gaussdb printenv GS_PASSWORD` 取入 env，未落 argv、未落文件） |
| `cargo audit` | 本机抓取 advisory-db 失败，改用 `--no-fetch` 复核，见下注 |

> audit 那格说清楚：本机 `cargo audit` 这轮 `couldn't fetch advisory database … error sending
> request for url (…/info/refs)`，属网络面而非依赖面；`--no-fetch` 用本地缓存库跑出
> **4 条 `zip` 漏洞，全部为既有豁免项**（口径见 architecture「审计豁免」与 V25），
> 与本轮改动无关。CI 的 audit job 会带网络重取，作为该格的正式证据面。

**与 CI 口径的对账**（审核 🟡「CI 未跑：本分支尚无 checks」）：该结论写在 09:22:14Z，
而 `e7f238f` 的 run `42789980924` 起在 09:21:09Z（早 65 秒），审核时确实看不到；现在
`gh run view` 一手 = 五个 job 全 `success`，`--log` 断言面 `test result: ok` 44 行、
`passed` 合计 **878**（test job 475 + golden job 403）、`FAILED|panicked` 命中 **0**。
所以本 head 的 §9 门禁是**已满足**的，合并门槛只随本轮新增提交移动。数字互证：本机
`--features gauss` = 406，CI 在 `957cb8d` = 403，差值恰为本轮新增的 3 条单测——
golden job 下一轮预期 406，若不等必须现场解释，不接受"数字差不多"。

**🟡 两条按审核口径留给下一轮**（不在本轮动，避免把结构改动混进正确性修复）：
`src/cli/args.rs` 859 行 / `src/cli/run.rs` 588 行的 §3 拆分轮，以及顺带补
`Snapshot::from_json_str` 的行宽 == schema 长度形态校验。本轮 `compare.rs` 自身规模
= 代码 225 行 / 含 `#[cfg(test)]` 总 537 行，在"含测试总行 ≤700"限内，不构成新增越限。

## V38. CLI 一致性修复轮：F1–F8 按 docs/spinduck-cli-修复设计.md 全量落地（2026-11-11）

来源：`docs/spinduck-cli-一致性验证报告.md`（实测 13 子命令 + 边界用例）→ 修复设计经人工批复
（D1=种子<项目发现<--rules；D2=A query 改 --db；D3 不加别名；D4 不做逐命令 --help；D5 不去重
--file；D6=parse 层读计划 target）。本轮依赖面零变更（`Cargo.toml`/`Cargo.lock` 无 diff）。

**行为面**（全部先 Red 后 Green，测试名可检索）：

| 项 | 修复 | 守护/行为测试 |
|---|---|---|
| F1 | `call` 两形态叠加项目规则发现（`rules_for_corpus`→`rules_for_paths`；计划形态取 `project.rules_files`）；优先级 seed < 项目发现 < `--rules` | `should_load_project_rules_for_call_routine_form`（修复前实测红：KERNEL-0010 mynvl missing）/ `…_call_plan_form` / `should_apply_rules_flag_after_project_rules_for_call` / `should_call_with_seed_rules_when_no_project_declared` |
| F4 | `compile` 拒绝 `--` 前缀（原把 `--help` 当路径读，ENV-0010 exit 1） | `should_reject_compile_with_flag_argument` |
| F5 | 8 解析器的单值旗标重复 → `重复旗标 <f>` 用法错；`call --file` 有意多次不纳管 | `should_reject_duplicate_single_value_flag`（17 例表驱动）/ `should_accept_repeated_file_flag_for_call` |
| F6 | `call --plan` 首位置参数 == 计划 target → 用法错（原静默当实参报 BIZ-0112）；计划不可读时 parse 跳过检查（I/O 面留给执行层） | `should_reject_call_plan_with_target_as_first_positional` / `should_skip_target_check_when_plan_unreadable` |
| F2-A | `query` 库文件改 `--db` 旗标，位置参数一律拒；契约测试同批更新（tests/cli/query.rs、compare.rs 夹具） | `should_parse_query_with_all_three_flags_in_any_order` / `should_reject_query_with_a_missing_flag_or_a_positional` / `should_exit_2_when_a_positional_is_given` |
| F7 | `locate()` 消息去"verify 以项目声明为起点"从句（verify/plan 共用） | `should_locate_error_mention_only_project_declaration` / `should_plan_error_reuse_neutral_locate_message` |
| F8 | usage：`call --plan` 形态补 `--rules` + 规则三通道口径一行；`read_source_strict` 注释修正（I/O=ENV-0010 / 编码=ENV-0030） | `usage_snapshot_should_mention_rules_in_plan_form` |
| F3 | `plan --dir` 取舍注释（位置参数被 routine 占用，不改接口） | 无行为面 |

**门禁证据**（本机，2026-11-11）：`cargo fmt --all -- --check` 干净；`cargo clippy
--all-features -- -D warnings` 干净（F1 返回三元组触发 `type_complexity`，按规范不用
`#[allow]`，收口为 `CallTargeting` 结构体）；`cargo test --features cli` 全绿（lib 331 +
cli 契约 80，0 failed）；`cargo audit` 4 条 = 既登记的 `zip` 豁免清单
（RUSTSEC-2026-0367/0368/0369/0429，与 ci.yml §audit 逐条一致），依赖零变更非本轮引入。

**二进制实证复核**（`target/debug/spinduck`，重建后）：`compile --help` → ENG-0010 exit 2；
`deploy --db a --db b` → `重复旗标 --db` exit 2；`query --db …` 三旗标任意顺序 exit 0、旧
位置形态 → `不接受位置参数` exit 2；`call --plan p.toml fnc_fee`（=target）→ 指认式用法错
exit 2、真实参 `100` → RETURN 0.08 exit 0；规则项目 `compile` 绿且 `call --file`（不带
--rules）同绿（RETURN 5）——报告 F1 的"体检绿、运行红"路径关闭；`plan`/`verify` 定位失败
消息不再含 verify 字样；usage `call --plan` 行含 `--rules` 与规则口径说明。

## V27. 物化 COPY STREAM 快路径实证（V27 提案 PR-A，2026-11-11）

> 设计与决策链见 `docs/materialize-copystream-design.md`（含 fastaas 参照、
> 真实例 17 表 ≈740 万行 / 64 分钟的现状画像与 PR 划分）。本节只记实证。

- **能力验证**：rust-opengauss（rev `b95ed0b`，当前锁定）sync 通道具备
  `Client::copy_out → CopyOutReader`（`crates/opengauss/src/client.rs:520`），
  零依赖变更。
- **首次全链路探针**（openGauss 5.0.0 容器 → duckdb-rs 内存库，100k 行含
  NULL 列与 DECIMAL 列）：unload 390ms + load 210ms = **600ms**，≈166k 行/s；
  行数与 `sum(amt)` 精确核对。
- **CLI 端到端实测**（release 二进制，落盘库，v0.1.6+V27）：100k 行
  **0.216s（≈463k 行/s）**——对比 V26.4 INSERT 路径 25.6s 为 **~118 倍**；
  `sum(amt) = 7,500,075,000.00` 精确。
- **语义保真**（`should_preserve_null_empty_and_special_cells_via_copystream`）：
  NULL / 多行含逗号引号字符串 / DECIMAL(15,3) 精度逐值往返无损。**A 模式口径
  修正（D14）**：`datcompatibility=A` 下内核层 `'' ≡ NULL`（Oracle 语义），
  源端 CSV 即写 `\N`——空串保真只对 PG 类源端成立，A 模式无空串可保；
  CSV bytes 探针存证（row 2 `v=''` 源端即 `\N`）。
- **降级守护**（`should_fall_back_to_insert_batch_when_fast_path_fails`）：
  临时目录不可创建 → 自动回落 INSERT，行数与数据正确，`outcome.mode` 可断言。
- **实现要点存证**：源端列名经 `prepare` 语句元数据获取——简单协议 0 行查询
  无 Row 消息、`fetch_rows` 的 schema 为空（首版实现即踩此坑，表现为
  `COPY t ()` 语法错，探针定位后修正）。
- **遗留**：TraceSink 分时事件（`mode/unload_ms/load_ms/rows/shards`）随
  JSONL 契约评审单独落地（设计文档 §3.4）；并行分片（PR-B）未实施。
- **PR-A2 多表并行（同窗口落地）**：`--table` 可重复/逗号分隔，`--jobs` 默认 4；
  进程内 `std::thread::scope` 工作线程消费表队列，每线程独立 GaussDB 连接 +
  独立 DuckDB 连接句柄（`DuckExecutor::from_connection` + `try_clone`，同库
  MVCC 并发写不同表）；单表失败不中断其余表，结果按输入序聚合，任一失败
  退出码 1。契约：`--where` 与多表同用为用法错（exit 2，parse 层判红，
  `should_reject_where_with_multi_table_before_touching_db` 锁定不建库）；
  `--jobs 0`/非数字用法错。容器 e2e：
  `should_materialize_multiple_tables_in_parallel_with_failure_isolation`
  （3 表 2 并发，含源端缺表失败隔离样本，快路径 `CopyStream` 断言）。
  CLI 端到端：4 张夹具表 `--jobs 2` 全部成功 + 1 张源端缺表 FAIL 隔离，
  总耗时 0.092s（真实例吞吐收益随 PR-A 的 CPU 让出而放大，§1.1）。
