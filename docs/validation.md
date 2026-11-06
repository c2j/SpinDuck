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
| `case_corpus_purchase` | 4 | `out p_o_succeed` = `0`；指令表 8 列 × 2 行（A999 异常行处理口径）；量额合计 `[2, 300, 30000, 6]`（3 行游标输入 → 2 行指令）；`error` = `BIZ-0151|… 缺少实参 p_o_succeed（形参未声明 DEFAULT）` |

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
- 在库项目的同一命令：`verify ../golden/case_corpus_purchase` →
  `entries=1 checks=11 failed=0`，并把运行态快照落到 `.spinduck/conf/spinduck.toml`。

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
