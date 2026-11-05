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
| 口令经 `.env` 注入 | compose `${GS_PASSWORD:?}` + `127.0.0.1` 绑定 + `.env.example`；README/golden 文档去明文 | `grep Enmo@123`（golden/README/docs/spinduck）= 0 |
| fastaas 子模块保留 | 现状（私有项目）；CI 可达性留待仓库私有化时评估 | — |

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
