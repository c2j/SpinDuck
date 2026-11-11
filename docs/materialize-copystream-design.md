# 物化 COPY STREAM 快路径设计（V27 提案）

> 状态：**PR-A（单流 COPY STREAM）与 PR-A2（多表并行）已落地**（validation
> V27）；**未开始**：PR-B 并行分片（§3.5）、显式逃生门 `mode = "insert_batch"`
> （§3.3，仅自动回落已实现）、指纹跳过（§3.7）、TraceSink 分时事件（§3.4，
> 降级可观测暂以 `MaterializeOutcome.degraded` + CLI stderr WARN 承载）。
> 本文档记录可行性验证结论与实现方案；实证数字落地后按仓库纪律更新
> `docs/validation.md`（V27 节），机制描述在实现合入时同步
> `docs/architecture.md` §13.1——三者不互相越界。

## 1. 背景与问题

物化通道（GaussDB → 本地 DuckDB）现状（`spinduck/src/io/gauss.rs::materialize`）：

- `SELECT` 全量拉回（简单查询协议、文本承载），**`simple_query` 把整个结果集缓冲在内存**；
- 本地拼 500 行/语句的多值 INSERT 下发，逐行文本往返。

V26.4 实测吞吐（回环容器）：物化 100k 行 **25.6s（≈3,900 行/s）**，而同仓库回写
（驱动 `copy_in`）98k 行仅 0.89s（≈110,000 行/s）——30 倍差距全部来自物化侧的
逐行路径，且大表物化还有内存上界风险。

参照实现：fastaas `dota-adapter` 的 `ParquetUnloadMaterializer`（P0 优化Pull），
其 COPY STREAM 快路径在生产口径下验证过同一组合（openGauss → DuckDB）。

### 1.1 真实例实测（2026-11-11，17 张表全量物化，≈740 万行 / ≈64 分钟）

| 信号 | 数据 | 设计含义 |
|---|---|---|
| CPU 瓶颈在本侧 | 最大表 user CPU ≈ 82% 墙钟（4.36M 行 36m19s 中 29m50s） | 逐行拼 INSERT + DuckDB 解析是主要成本，COPY STREAM 直接消除，真实例收益 ≥ 回环实测的 42 倍 |
| 行/s 随列数劣化 | 147k 行宽表仅 760 行/s（全场最差），窄表 ~5,000 行/s | 文本路径成本按列线性放大；COPY 路径常数项小，宽表受益最大 |
| 内存隐患 | `fetch_rows`（simple_query）**全量缓冲**整个结果集 | 4.36M 行宽表 = GB 级驻留，是可放量风险而非仅性能问题；COPY 流式顺带修复；**INSERT 回落路径保留此限制，须在 §5 声明** |
| 并行分片阈值 | >100k 行的表 9 张，最大 4.36M 行（36 分钟） | §3.5 阈值 100k 获实证支撑；但表间并行（§3.6）覆盖更广，单表分片降为兜底 |
| 小表固定开销 | 0.2–0.6s/表（连接 + 开库） | 忽略不计，不做连接池/复用优化 |

**决策结论（2026-11-11 二次修正）**：实施顺序 **PR-A（单流 COPY STREAM + D14 保真
+ 降级 + TraceSink 分时）→ PR-A2（多表并行 CLI，§3.6）→ PR-B（单表分片，§3.5，
降为"超大单表仍慢时再做"）**。17 表实测 64 分钟的预期收敛：PR-A 后 5–8 分钟，
PR-A2（jobs=4）后 1.5–3 分钟；PR-B 只对 PR-A2 后仍慢的超宽超大单表有意义。

## 2. 可行性验证结论（2026-11-11 实测）

| 验证点 | 结论 |
|---|---|
| rust-opengauss fork（rev `b95ed0b`）sync 通道 | **具备** `Client::copy_out → CopyOutReader`（`crates/opengauss/src/client.rs:520`，协议层 `CopyOutResponse` 完整）；`gaussdb::sync` 门面已导出该类型，**无需升依赖** |
| 端到端实测（openGauss 5.0.0 容器 → duckdb-rs 内存库） | 100k 行（含 NULL 列、DECIMAL 列）：unload 390ms + load 210ms = **600ms 全链路**，≈ 166,000 行/s |
| 数值保真 | 行数精确 100,000；`sum(amt)` = 750,007,500.00 精确匹配 |
| NULL 保真 | `NULL '\N'` 显式标记两侧对齐，NULL 列往返后 count/sum 无损 |

**结论：快路径可行，预期收益约 42 倍（600ms vs 25.6s），且消除整表内存缓冲。**

## 3. 设计

### 3.1 快路径（默认）

```
GaussDB                          本地
─────────                        ─────────────────────────────
COPY (SELECT * FROM <t> [WHERE <w>])
  TO STDOUT WITH CSV HEADER
  DELIMITER ',' NULL '\N'
        │ CopyOutReader（流式）
        ▼
   临时 CSV 文件（run 目录，§11 布局）
        │
        ▼
DuckDB: COPY <t> FROM '<csv>'
  (FORMAT CSV, HEADER true, DELIMITER ',',
   QUOTE '"', ESCAPE '"', NULLSTR '\N')
```

- **读取端**：`GaussConn::copy_to_csv(table, where, csv_path) -> Result<u64>`，
  流式 `std::io::copy` 落盘，不整表驻留内存；
- **写入端**：经 `SqlExecutor::execute` 下发 DuckDB `COPY ... FROM`（CSV reader 是
  duckdb 的向量化快路径）；目标表必须**已存在**（`ensure_local_table` 守卫前置，
  ENV-0015 口径不变）；
- 临时文件放项目 run 目录，成功即删（fastaas `CLEANUP_ON_SUCCESS` 口径），失败保留
  以便诊断，并在错误消息中给出路径。

### 3.2 空串/NULL 保真约定（D14，硬约束）

PG CSV 写出：空串 → `""`（带引号空字段）、NULL → 空字段；DuckDB 默认 `nullstr=''`
且先去引号再比对 ⇒ **两者都塌缩成 NULL**。物化是真表快照，`''` 与 NULL 是不同取值
（NULL 语义谓词、JOIN/EXISTS 键会因此多丢行且运行仍"成功"）。

**约定：两侧显式 `NULL '\N'`**——空串保持 `""`，NULL 保持 `\N`。此约定是快路径的
正确性前提，回归测试必须覆盖三类单元格往返：空串、NULL、含 `,"` 换行符的字符串。

> **A 模式实测修正（2026-11-11，CSV bytes 探针）**：`datcompatibility=A` 下
> openGauss 内核层 `'' ≡ NULL`（Oracle 语义），源端 CSV 对空串即写 `\N`——
> A 模式无空串可保，D14 只对 PG 类源端构成约束。回归测试按 A 模式口径锁定
> "传输层不产生额外塌缩"（与 INSERT 路径同形）。

### 3.3 模式选择（评审决定，2026-11-11）

- **默认 = COPY STREAM**；**每表可显式指定**走现有 500 行批量 INSERT 路径——载体用
  §13.1 既有的项目声明（`spinduck.toml` 物化清单），形如 `mode = "insert_batch"`；
  显式指定是对"特殊表"的逃生门（如源端禁 COPY 权限、表上有触发器语义、列形态
  走文本更稳），不是快路径的降级开关。**落地状态：未实现**——当前只有运行期
  自动回落（§3.4）；本条保留为设计目标，落在后续 PR；
- 快路径**运行期失败仍自动回落 INSERT**（3.5 阶梯），与显式指定互不干扰：
  显式指定 = 不尝试快路径；默认 = 先快路径后自动回落；
- CLI 面不加开关（默认快路径 + 自动降级），fastaas 的 `copy-stream-enabled` 配置位
  在 SpinDuck 无对应配置面，不引入。

### 3.4 降级阶梯

```
默认：COPY STREAM ──失败──▶ 自动回落 500 行批量 INSERT
显式 mode = "insert_batch" ──▶ 直接走 INSERT，不尝试快路径
（后续项，见 §7：大表并行分片 ──分片失败──▶ 单流 COPY ──▶ INSERT）
```

- 降级必须**可观测**：`materialize` 返回 `MaterializeOutcome { rows, mode,
  degraded }`——回落时 `degraded` 携带快路径失败原因（PR-A 已落地，测试可断言
  `mode=InsertBatch`）；CLI 侧把 `degraded` 写成 stderr `WARN` 行（#17，stdout
  契约不变）；TraceSink 分时事件（`unload_ms`/`load_ms`/`shards` 等）属
  JSONL 契约（§11.2）破坏性变更，**随契约评审单独落地**，落地后 stderr 只留摘要。
- **回落边界（#16，硬约束）**：只有「入库前失败」才回落——`COPY FROM` 已成功
  入库之后的任何失败（行数查询、CSV 行数交叉核对、收尾）一律带 `loaded = true`
  上抛，**绝不回落 INSERT**（不清表只追加，回落即两倍行数）；CSV 清理失败是唯一
  例外，按 §3.1「保留供诊断」降级为成功。

### 3.5 并行分片 COPY（后续项，无主键表同样适用）

单 COPY 流受单连接限制；真实例走网络时并行分片是线性扩展的主要手段
（回环容器收益有限）。**分片不需要主键**（评审澄清，2026-11-11）：

- **ctid 范围分片（首选）**：`ctid` 是 openGauss/PG 堆表物理元组地址，与业务键
  无关；按页范围切片 N 连接并行 `COPY TO STDOUT`，各分片在 `REPEATABLE READ`
  快照下一致性有保证。适用于普通堆表（本项目场景全部满足）；
- **任意列 hash 取模（兜底）**：`abs(hash(<任一列>)::bigint) % N = i`——NULL 可
  hash、确定性无重叠，业务键与主键完全不需要；代价是哈希倾斜与每分片过滤成本；
- **护栏**：行数估算低于阈值（初值 100k，实测后调）不并行；ctid 不可用或分片
  失败自动回落单流，再回落 INSERT——三级阶梯，每级 TraceSink 可观测；
  分片数初值按源端核数与网络带宽定（真实例上先用 2/4 档实测再定默认值）。

### 3.6 多表并行（PR-A2，已落地，见 validation V27）

一次 CLI 调用并行物化多张表，比单表分片（§3.5）更简单且覆盖所有表。

**必须做在进程内，不能靠 shell 并行**：DuckDB 同一库文件跨进程有写锁（`for t in
… &` 直接被拒）；进程内多连接（`connection().try_clone()`，既有基建）由 MVCC
处理并发写不同表。前置依赖：PR-A 先落地——旧 INSERT 路径 user CPU 82%（§1.1），
CPU 被文本成本占满时并行无收益；COPY STREAM 让出 CPU 后瓶颈回到 IO/网络，
表间并行才有意义。

**CLI 契约**：

```bash
# 现有契约不变（单表 + --where 完全向后兼容）
spinduck materialize --db aaspubs.db --table t1 --where "bcrq='20250715'"
# 新增：多表（--table 可重复/逗号分隔，取并集；**重复表名 = 用法错**，#18：
# 同表并发双写会冲突或静默灌两遍，宁可红不悄悄去重）
# --where 与多表同用 = 用法错（语义歧义，EXIT_USAGE=2）
spinduck materialize --db aaspubs.db --table t1 --table t2 --table t3
spinduck materialize --db aaspubs.db --table t1,t2,t3 --jobs 2
```

- **执行模型**：`--jobs`（默认 4）个工作线程；每线程独立的 GaussDB 连接
  （`copy_out` 需 `&mut client`）+ 独立的 DuckDB 连接句柄；表队列分发，先完成
  的线程取下一张表；
- **失败语义**：单表失败**不中断**其余表（快照批次语义），结束统一报告每表
  `OK rows=N` / `FAIL <域-####>`，任一失败退出码 1；每表输出格式与现有单表
  契约一致（多行而非改格式）；
- **可选增强（不进首版）**：`--table @plan` 读取计划 `[sources].materialize`
  清单，消灭 runbook §4 的 shell 循环；
- 并发度上限 = min(jobs, 源端 max_connections 余量)；真实例默认 4，先 2/4 档
  实测再定。

### 3.7 指纹跳过（独立后续项，本设计不含）

`COUNT(*) + MAX(updated_at)` 元组做切片指纹，同分区未变则复用已有本地表（fastaas
issue #85 形态）。三条纪律原样继承：**保守失败**（探针失败/无 `updated_at` 列不跳，
不把源端故障误判为数据未变）、**opt-in 白名单**、缓存键含分区值。落地形态挂在
§13.1 既有的"物化时间戳与过滤条件入 catalog"规划上，作为独立 PR。

## 4. 测试计划（TDD 顺序）

> **守护测试落位例外口径（评审 P3-4）**：本通道的守护测试落在 `src/io/gauss/`
> 的 `#[cfg(test)]`（`finalize_copystream` / `apply_fast_failure` 为私有接缝，
> 集成测试不可见），**不进** `tests/guard/` 集中登记——AGENTS §4.3 的集中登记
> 以「集成层可见」为前提，此处为例外，删改仍按 Ask first。

**PR-A（单流 COPY STREAM）**：

1. **Red（往返保真守护）**：容器夹具建含四类单元格的表——NULL、空串、含 `,"` 与
   换行的字符串、DECIMAL 精度（`NUMERIC(15,3)` 边界值）——断言快路径物化后逐值
   相等（含空串 ≠ NULL）；
2. **Green**：`GaussConn::copy_to_csv` + `materialize` 接入快路径；
3. **降级守护**：人为制造快路径失败（如只读临时目录不可写），断言回落 INSERT
   路径且降级可见——TraceSink 事件未落地，当前以 `degraded` 字段 + stderr `WARN`
   承载（#17）；**入库后失败（#16）不回落**，由
   `should_not_fall_back_to_insert_when_copy_already_loaded` 锁定；
4. **行数交叉核对**：快路径返回行数（CSV 行数-1）与物化后 `count(*)` 一致，
   不一致即 ENV 错误（对齐回写通道的行数校验纪律）。**已落地**（#16/#20-5）：
   `finalize_copystream` 内引号感知统计 CSV 数据行数并与行数差值核对，
   `should_err_loaded_when_csv_rows_mismatch_loaded_rows` 锁定；
5. **吞吐实证**：100k 夹具计时入 `validation.md` V27（含环境声明与"一次性测量"
   口径注记，同 V26.4 纪律）。

**PR-A2（多表并行 CLI，§3.6）**：

6. **契约守护**：`--table` 重复/逗号分隔解析（**重复表名 = 用法错**，#18：
   `should_reject_duplicate_table_names_as_usage_error`）；`--where` 与多表同用 →
   用法错（EXIT_USAGE=2，不连源不建库）；单表形式与现契约逐字一致；
7. **并发行为**（容器探针）：jobs=2 物化 3 张夹具表，断言全部成功、输出行数
   与单表一致；`--jobs 0` / 负数 → 用法错；
8. **失败隔离**：夹具中一张本地不存在的表（`ensure_local_table` 一线红），
   断言其余表仍物化成功、该表报 FAIL、整体退出码 1。**口径修正（#20-1）**：
   现有用例证明的是「本地缺表隔离」；**源端故障**（连接断开/copy_out 被拒）下的
   隔离样本待补（夹具表存在但 `--where` 引用不存在的列，使失败点落在 prepare/
   copy_out 上）；
9. **并发写同一 DuckDB 文件**：多线程各持独立连接句柄写不同表，行数核对
   （MVCC 并发写的回归守护）。

## 5. 边界（不声称做到的）

- 不并行分页拉取：单 COPY 流已接近回环上界，多流并行属后续优化项（§3.5）；
- **INSERT 回落路径保留"整表结果集缓冲"的内存特性**（现 `fetch_rows` 口径）：
  降级发生在大表上时内存压力与现版本相同，不新增也不修复——修复只属于快路径；
- 不做 Parquet 中间落盘：fastaas 成功路径同样不导出 Parquet，CSV 临时文件用完即删；
- 不动 `sync` 回写通道（已是 COPY 最优形态）；
- 键序不承诺：CSV 由源端 `SELECT` 顺序产出，物化表无 ORDER BY 语义（与现路径一致）。
- **标识符不加引号（#21-1）**：`COPY {table} ({cols})`、`INSERT INTO {table}` 中的
  表名/列名为裸文本拼接（INSERT 路径同口径，非快路径回归）。带引号建的大小写
  敏感列（`"BizDate"`）、含空格/保留字列名会让快路径 SQL 语法错或错列——失败形态
  是整表回落（入库前，安全）。列名加引号方案待评估；在此之前**不承诺**此类标识符。
- **值恰为字面 `\N` 的往返未测（#21-2）**：openGauss 侧写出 `"\N"`（带引号），
  DuckDB CSV reader 的 nullstr 比对与去引号次序决定其塌缩为 NULL 与否；未验证前
  **不承诺**该值往返保真（待夹具补 `chr(92)||'N'` 行后定论）。

## 6. 评审决议（2026-11-11）

- **保留批量 INSERT 路径**为可显式指定的模式（§3.3），COPY STREAM 为默认——特殊表
  有逃生门，快路径有自动回落；
- **并行分片 COPY 列为后续项**（§3.5）：无主键/联合主键表经 ctid 或 hash 取模同样
  可并行，不构成障碍；
- **明确不做**：二进制 COPY（`BinaryCopyOutIter` + 逐值 appender 的收益须实测，
  且 CSV 文本路径已达标，评审决定不投入）；DuckDB postgres 扩展（openGauss 协议
  兼容性未验证 + 动态扩展与静态发版纪律冲突）；压缩传输（回环无收益，真实例另议）。

## 7. 开放问题

- ~~临时文件位置~~ **已定（PR-A）**：与目标库同目录（`db.parent()`），失败诊断
  现场自包含，成功即删、失败保留（文件名含表名与 pid，PR-A2 多表并行天然不冲突）；
- `COPY ... FROM` 的文件路径转义（Windows 反斜杠）沿用 fastaas 的双写处理，
  归 `std::path` 层处理而非字符串拼接（§10 平台纪律）；
- 并行分片（§3.5）的默认分片数与阈值：待真实例 2/4 档实测后定，不拍脑袋。
