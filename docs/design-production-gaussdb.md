# 生产 GaussDB 接入与存过运行——设计与差距分析

> 目标（用户诉求口径，四条）：**连接一台有真实数据的 GaussDB 实例**，用 **release 得到的
> `spinduck` 二进制**运行 SpinDuck 项目的**存储过程**，数据量为**千万级**，并且**要回写实例**。
>
> 本文是**设计与差距分析**（含被否方案），不是机制说明也不是实证记录：
> 落地后各能力面的**机制**段迁 `docs/architecture.md`（§13 数据接入与回写的延伸），
> 一切**实测数字**登记 `docs/validation.md` 新小节 **V26**。本文引用行号截至当前
> `main`（`cbaaae5`），代码变更后必须复核。
>
> **修订记录**：v1 初稿 → v2 外部审核（影子表 DDL 例外口、harness env 守卫、CSV 双向方言
> 风险、A0 千万行判据降级、`server_encoding` 入待确认）→ v3 本轮（破坏性开关拆成两个、
> DoD #3 收紧、A1 补 CI 定义变更、`norm` 两份一起收口、编码探测入 A0 判据）→
> **v4（A0 取证回写，四处口径订正）**：① 原稿把"执行 DDL 的部署能力"整体判为不存在，
> 与本仓 `validation.md` V20 自相矛盾——`deploy_project` / `verify` 已具备建表 + 缺件红，
> 差距重述为"未接到 `call` 通道、库不落盘、资产按目录而非闭包清单"（§3.1、§6.2、R3）；
> ② §3.3 的复现形态按命令逐字重跑，改为 A/B/C 三形态并落到根因（`call` 只 `register` +
> 语料自带 `WHEN OTHERS` 兜底），不再写"引擎吞异常"；③ §4 交付件编码由"GBK"订正为
> **混合编码**（UTF-8 11,462 / 需 GBK 3,149），并新增实测：三条源码入口对非 UTF-8
> 的行为**互不一致**，`call` 那条是静默变形（§3.5）；④ "语料 8 处"由"错声明"改判为
> **口径歧义**（数字本身对得上 `config/corpus_forms.toml`，缺的是"哪份语料"的限定，§4 硬事实 #2）。
> 本轮数字全部登记 `docs/validation.md` V26。

---

## 1 结论摘要

| 问题 | 结论 |
|---|---|
| release 二进制能不能连真实 GaussDB | **不能**。发版矩阵构建 `--features cli`，不含 `gauss`；`materialize`/`sync` 直接返回 `[ENV-0012] ... requires feature 'gauss'`（`spinduck/src/cli/run.rs:292`、`.github/workflows/release.yml:111`） |
| TLS 要不要自己实现 | **不要**。`gaussdb::sync` 驱动侧已有（`tls-native-tls` + `MakeTlsConnector`），本仓写死 `NoTls` 且依赖未开 TLS feature（`spinduck/src/io/gauss.rs:11`、`spinduck/Cargo.toml:34`）。换连接器即可，属 Ask-first（动依赖 feature） |
| 存过源码怎么进引擎 | **两条路，好坏各一条**。`verify` 走 `deploy_project`：表/视图/序列 DDL + 桩 + 例程 + 种子全下发，闭包缺件即红（`spinduck/src/verify/deploy.rs:61`、V20）——但库开在**内存**（`spinduck/src/verify/deploy.rs:34`），跑完即弃，`call` 接不上。`call --file <sql>...` 才是目标链路实际用的那条，**只 `register` 例程、不执行任何表/视图/序列 DDL**（`spinduck/src/engine.rs:124`）→ 缺表时实测 `RETURN 0` 且 rc=0（§3.3 三形态，V26.1 取证）。这是**最危险缺口**——**V31（A2-4）已封**：`call` 两条入口现在都在执行例程之前过同一把缺件刀，形态 C 变成 `MISS relation:par_sys_purchase <- …` + rc=1；`--plan` 面另免掉逐个 `--file`。表达式级被调函数这一半曾对门与计划**隐形**，**V33（A2-5）已收口**：裸名调用的被调函数按例程目录命中后进同一条 BFS，它自己的表同时进计划与门（§8 盲区行改判为"两类残余"） |
| 真实交付件的编码能不能吃下 | **D9② 后：能，且是显式地能**（V32）。三代形态：① 改动前——`compile` 有损解码并打 `(non-utf8)`；`call --file` 同样有损但**把标记丢弃**（`spinduck/src/cli/run.rs:244`）→ 中文字面量静默变形，同一份源码仅编码不同就 `RETURN MATCH` / `RETURN NO_MATCH` 两条都 rc=0；`verify` 走 `read_to_string` → 直接红。② D9①（V28）——执行面判红 `[ENV-0030]` + rc=1，静默消失，代价是 §4 那 3,149 个 GBK 文件一律跑不起来。③ D9②（V32）——读入时两把严格判定 UTF-8 → GBK，都不合法才红；交付件 3,149 个 GBK 文件**全部** GBK 严格可解（实测计数见 V32.4，两侧同源码不同编码现给同一个 `MATCH`）。`compile` 体检面仍是有损 + `(non-utf8)`，那是 §3.5 末段的刻意保留 |
| 千万级取数/回写能不能用 | **不能直接用**。取数把整个结果集收进 `Vec` 再逐行拼字面量 `INSERT`；回写逐行 `execute`、无事务、无幂等（`spinduck/src/io/gauss.rs:88,111,142`） |
| 能不能拿它当复现证据 | **本地夹具能，真实例不能**。夹具侧早有逐值裁决：`[[case.assert]]` 四维（call/out/rows/error）跑进程内内核 + golden `expected.json` 比基线（V21）。**V34 把这条裁决从夹具搬进了 CLI 链**：`query <db> --sql-file --out` 落一份结果快照 JSON，`compare a.json b.json [--key]` 按值判红（rc=1 + `DIFF` 行），于是 §3.3 形态 C 那类"CLI 链上无人比对返回值"的绿在本机已不可能悄悄过去——前提是你把 `query`/`compare` 挂进验证脚本。**没变的一半**：参照侧（GaussDB 实例）仍没有采集入口，快照两边都只能来自内核/夹具，"接到外部实例"属 A3/A4 之后（§7 A5 实例半边） |
| codeweb 能不能作 crate 引入 | **不能**（四条阻塞，§5 D4）。**能**作进程外分析器产出 JSON 计划，A 路径已在真实交付件上跑通（§4） |
| 离"能在真实例上出可信结果"多远 | 8 个阶段（§7：A0a/A0b + A1–A6）。**A0a（本机取证，零功能改动）已完成**；A0b 与 A1 起都要等实例到手。**A2 的设计内本机项已全部完成**：D9①（编码判红，V28）、`plan`（V29）、`deploy`（V30）、缺件红扩到执行面 + `call --plan`（V31）、D9② 真 GBK 解码（V32）、A2-5 表达式级被调函数进闭包（V33）。**A5 的本机半边也已完成（V34）**：`query` 落结果快照 + `compare` 逐值裁决 + 归一两份副本收口成 core 单一实现。⇒ **本机可推进项到此清零**：A0b/A1/A3/A4/A6 与 A5 的实例半边全部要实例 |

---

## 2 目标链路与角色划分

```
真实 GaussDB 实例（千万级，读写）
      │  ①取数（COPY OUT）      ④回写（COPY IN + 事务）
      ▼                            ▲
  本地 DuckDB 库文件  ──②存过执行──▶ 结果表 ──③逐值比对──▶ 报告/红
                      （spinduck 二进制）
      ▲
      └─ ①′ 对象闭包与 DDL 清单：codeweb 语义图 → run-plan
```

**角色必须分开，否则会把生产库当参照库踩**：

| 角色 | 是谁 | 允许的动作 |
|---|---|---|
| 参照实现（reference） | 仍是 `golden/gaussdb-docker` 的 openGauss 5.0.0 **A 模式**容器 | 整体重建 schema、部署语料例程、产出 `expected.json` 基线 |
| 数据源/汇（source/sink） | 用户的真实 GaussDB 实例 | 只读取数；回写仅限**显式白名单**表；对**既有对象**禁止 DROP/TRUNCATE/重建；影子表**新建** DDL 属例外，须 `role=source` 且 `SPINDUCK_OG_ALLOW_SHADOW_CREATE=1`，并限 `sd_shadow_` 前缀 + 目标表不存在时才建（§6.4） |

> 现状红线：黄金 harness 无条件执行
> `DROP SCHEMA IF EXISTS gaussdb CASCADE;`（`spinduck/tests/common/mod.rs:200`）。
> 把 `SPINDUCK_OG_HOST` 指向真实例就会触发它。阶段 A1 的**准入门禁**是这条红线先被
> 角色开关 + 破坏性操作门禁挡住（§6.4）。

---

## 3 现状实测基线

### 3.1 能力面盘点（`Command` 枚举共 9 条，`spinduck/src/cli/args.rs:10`）

`Version / Help / Compile / RulesProbe / Explain / Materialize / Sync / Verify / Call`。

**"执行 DDL 的部署"这一环已经存在，只是没接到目标链路上**（本仓 `validation.md` V20 登记的
就是它，本文初稿判成"不存在"是错的）：`spinduck verify [项目目录]` → `verify_project` →
`deploy_project`（`spinduck/src/verify/deploy.rs:61`）按 **表/序列 DDL → 视图 → `sys_dummy` →
桩包与例程 → 种子** 顺序下发，再按 `[[entry]]` 做闭包核验，缺件报 FAIL 并退出 1
（`spinduck/tests/verify_chain.rs:245` `should_render_reports_and_exit_1_when_chain_has_gaps`）。
`cli` feature 含 `native`（`spinduck/Cargo.toml` features 段），所以**发版二进制里这条链是通的**
——实测：`release` 二进制对 `golden/case_corpus_purchase` 跑 `verify` 得
`entries=1 checks=11 failed=0 skipped=1`、rc=0。

真正缺的是把这套机制接到"真实例 + 真实闭包"上的**四件事**：

| 缺口 | 现状 | 后果 |
|---|---|---|
| 库不落盘 | `verify_project` 用 `DuckExecutor::open_in_memory`（`spinduck/src/verify/deploy.rs:34`），核验完即弃 | 部署好的表不能交给 `call`/`materialize`/`sync`，只能重跑一遍 verify |
| 资产按**目录**发现，不按**闭包** | `ddl_files()` 等一律 `sql_files(dir, false)`，且该函数**忽略递归参数**（`spinduck/src/project/fs.rs:49`） | 交付件那种深层碎目录（`018_FUNCTION/`…）扫不到，也无法表达"只要这 4 个函数 + 14 张表" |
| 入口必须带包限定 | `qualified_routine` 拒绝不含 `.` 的名字（`spinduck/src/project/entry.rs:50`） | 交付件 2,148 个独立函数不能作 `[[entry]]`，闭包门禁覆盖不到它们 |
| `call` 自己那条路没有部署 | `call --file` 只 `register`（§3.3） | 用户实际用的命令恰好是**没有**门禁的那条 |

`query`（跑一条 SQL 落盘）与 `compare`（逐值裁决）**确实不存在**；`deploy`/`--plan` 作为独立
子命令也不存在，但其内核能力有一半可以复用 `deploy_project`（§6.2 据此重写）。

### 3.2 通道与数据面事实

| 位置 | 事实 | 后果 |
|---|---|---|
| `spinduck/src/io/gauss.rs:49,68` | `NoTls` 在 `connect`/`connect_parts` 两个入口写死（import 在 `:11`），`gaussdb` 依赖 `default-features = false, features = ["sync"]` | 只能连明文实例；`sslmode=require` 类实例连不上 |
| `spinduck/src/io/gauss.rs:88` `fetch_rows` | `simple_query` 一次性返回 `Vec<SimpleQueryMessage>`，全部行文本驻留内存 | 千万行 = 数 GB 客户端内存，先 OOM 后慢 |
| `spinduck/src/io/gauss.rs:111` `materialize` | 逐行拼 `'…'` 字面量 `INSERT`，且**要求 DuckDB 侧表已存在**（注释自陈） | 无批量、无类型保真校验、表不存在即错 |
| `spinduck/src/io/gauss.rs:142` `sync_append` | `SYNC_BATCH=1000` **只批读**；GaussDB 侧每行一次 `execute`；无事务、无幂等 | 千万行 = 千万次往返；中途失败留下半截数据且无法判定 |
| `spinduck/src/cli/run.rs:228` `call_cmd` | 全程不触碰 GaussDB：既不自动物化，也不回写 | 多命令手工编排，无端到端可重复契约 |

### 3.3 静默假复现：三种命令形态，两种结局（本轮逐字重跑，登记 V26.1）

被测函数 = `golden/case_corpus_purchase/sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql` 的
`fnc_get_purchase_js_days('20250715', '0130000001')`，参照基线 `'90'`
（`golden/case_corpus_purchase/cases/baselines/expected.json` 场景 1）。二进制 = 本机
`cargo build --release --features cli --locked` 产物。三条命令只差 `--file` 传哪些文件：

| 形态 | `--file` 传了什么 | 实测输出 | rc |
|---|---|---|---|
| **A** | 只有主角函数 | `[KERNEL-0010] Catalog Error: Scalar Function with name fnc_com_if_deal_date does not exist!` | **1** |
| **B** | 主角 + `sql/ddl/tables.sql` + `cases/datasets/seed.sql`（试图"顺手把建表也喂进去"） | 同 A，**一字不变** | **1** |
| **C** | 主角 + `0_SIMPLIFIED_fnc_com_getday.sql` + `0_SIMPLIFIED_fnc_com_if_deal_date.sql`（补齐依赖函数，仍不建表） | `RETURN 0` | **0** |

> **现状（V31，A2-4 已落地）**：上表是 V26.1 的缺陷快照，三条命令的 `--file` 传法不变，结局变了——
> 缺件门发生在 `eng.call` 之前，A/B/C 在 CLI 面上**同形**：
>
> ```text
> MISS relation:par_sys_purchase <- fnc_get_purchase_js_days
> call: target=fnc_get_purchase_js_days missing=1 未执行（design D5 缺件即红）   rc=1
> ```
>
> 三条守护期望值随行为同批改写（用户 2026-11-10 授权，见 V31.2），内核面那条
> `KERNEL-0010 … does not exist` 在本夹具上**不再可达**（红提前了），其证据仍留在
> V26.1 与 `tests/guard/guard_register_silent_drop.rs`。下面三点解释的是"为什么必须提前"，
> 口径不变。

三点必须记准，否则会把结论用错：

1. **B 与 A 完全同输出**——表 DDL 与种子经 `--file` 递交后被静默丢弃：`Engine::register`
   只编译 `CreateProcedure | CreateFunction | CreatePackageBody`
   （`spinduck/src/engine.rs:124,138`），其余语句既不执行也不告警。**"我明明把建表脚本传进去了"
   不构成任何证据**。
2. **C 的 0 不是引擎吞异常**，而是语料自己的兜底：`golden/case_corpus_purchase/sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql:14-23`
   是 `BEGIN SELECT t.purchase_days INTO v_purchase_days FROM par_sys_purchase t WHERE … ;
   EXCEPTION WHEN OTHERS THEN v_purchase_days := '0'; END;`。表不存在 → 走 `WHEN OTHERS`
   → `'0'`。引擎**忠实执行**了语料语义，错的是"库里没有表"这个前提。
   （A/B 之所以响，是因为它们死在**这段兜底之外**的 `:29` —— 调用未注册的
   `fnc_com_if_deal_date`，外层没有处理器。）
3. **`0` 恰好是这份基线里的合法值**（场景 3「品种无配置 → `SELECT INTO` 走 `WHEN OTHERS`
   归 `'0'`」、场景 4「日期不在生效区间 → 同归 `'0'`」）。所以 C 的产物**逐字看起来像一次
   正确的复现**，连值域都对得上——这正是它比报错危险的地方，违反 AGENTS §4.1
   「跳过不等于通过」的精神。

由此得出两条落地约束（分别归 D5 与 §6.2）：**红必须在执行例程之前**（缺件不许产出
`RETURN`），以及**门禁必须挂在 `call` 实际走的那条路上**（§3.1 的 `verify` 闭包核验帮不上它）。

### 3.4 引擎在真实数据规模上的已知上界

- `sqlfn` 参数元组缓存 `MAX_TUPLES = 100_000`（`spinduck/src/sql/sqlfn/mod.rs:27`）：
  SQL 上下文里的例程调用点先按 **DISTINCT 绑定元组** 拉取，`fetch_many(max + 1)` 把
  100,001 行**全部读进内存**后才判超限，报 `BIZ-0157`
  （`spinduck/src/runtime/interp/dynamic.rs:59-68`）。
  千万级表上这个元组数**极可能超**，且是"先炸内存再拒绝"。
- 黄金 README 与 V23 已登记的其他口径（集合下标读取、未登记过程级函数 → `KERNEL-0010`、
  上下文互引 `BIZ-0160`）在真实交付件上的**命中量未知**，属 A0b 待量化项。
  其中"未登记过程级函数 → `KERNEL-0010`"一类**本轮已有一次真实命中**：§3.3 的形态 A/B
  正是它（调用未注册的 `fnc_com_if_deal_date`），且这条是**响的**，不需要担心静默。

### 3.5 编码：四条源码入口两种行为，执行面静默已由 D9① 判红、GBK 已由 D9② 真解码（V26.4 实测 + V28/V32 落地）

`read_source`（`spinduck/src/cli/mod.rs:47`）对非 UTF-8 字节做 `from_utf8_lossy`——**不是
GBK 解码**，非法字节一律变 U+FFFD。它对注释无害，对**中文字面量有害**。下表是 **A0a 实测到的
改动前**分布（登记 V26.4；V28 订正：原表漏了 `explain --file` 一条，它与 `call` 走同一处丢弃
标记，因此同样静默——受影响的是四条入口，不是三条）：

| 入口 | 读源码方式 | 非 UTF-8 实测行为（改动前） | rc | 现状 |
|---|---|---|---|---|
| `compile <path>` | `read_source` + 保留标记 | `OK … routines=1  (non-utf8)`（标记来自 `spinduck/src/cli/run.rs:168`） | 0 | **仍不变**（体检面要"扫完并分类"，§3.5 末段） |
| `call --file <sql>` | `read_source` + **丢弃标记**（`spinduck/src/cli/run.rs:244`） | 无任何提示，中文字面量已变形 | 0 | **D9①（V28）判红 → D9②（V32）真解码**：现在走 `crate::source::read_text`，UTF-8 → GBK 两把严格判定，GBK 源正常返回、字面量逐字还原；两把都不成立才 `[ENV-0030] … neither valid UTF-8 nor valid GBK` + rc=1 |
| `explain --file <sql>` | 同上丢弃标记（`spinduck/src/cli/run.rs:188`） | `0\tassign\tp_o := '…U+FFFD…'`，无提示 | 0 | **同上**：GBK 源产出与 UTF-8 twin 逐字相同的 IR 行 |
| `verify`（= `deploy_*`） | `std::fs::read_to_string`（`spinduck/src/verify/deploy.rs` 四处） | `[ENV-0030] 读取 …: stream did not contain valid UTF-8` | 1 | **同上收口**：四处改走 `source::read_text`，GBK 包体核验 `rc=0` 且 `PASS Entry`；判红形状仍是 `ENV-0030`，只是尾巴说出两把判定 |

`call` 那条的后果用一份自造最小夹具实测（GBK 与 UTF-8 **同内容两文件**，同一 UTF-8 参数
`'已成交'`，函数体是 `IF p_i_flag = '已成交' THEN 'MATCH' ELSE 'NO_MATCH'`）。**下面这段是
D9① 落地前的实测（V26.4）**，D9① 后 GBK 那条变 `[ENV-0030]` + rc=1，**D9② 后两条同为
`RETURN MATCH` + rc=0**（判据实验已在 `tests/cli/call.rs` 永久锁定，V32.3）：

```
GBK  源 + UTF-8 参数 → RETURN NO_MATCH   rc=0      ← 改动前；D9① 为 rc=1；现为 RETURN MATCH rc=0
UTF-8 源 + UTF-8 参数 → RETURN MATCH      rc=0      ← 不变
```

**同源码、同参数、只差文件编码 → 结果相反且两条都绿（改动前）。** 真实交付件里这种字面量不是边角：
§4 的编码分解显示需要 GBK 的 3,149 个文件里，**2,384 个（75.7%）在单引号字面量内出现中日韩
字符，合计 182,723 处**，只有 761 个是注释级。也就是说 `call` 一条链上的有损解码足以把
`WHERE status = '已成交'` 变成永不成立的比较——本文最警惕的**静默错值**形态，而且它今天就
可达，不需要千万级数据。

处置口径：这是 **R9 的真实形态**（初稿写的"CLI 无解码能力"不成立——能力有，但是**有损**且
`call` 连告警都没有）。两个选项里 **D9① 已落地**（§7 A2，登记 V28）：执行面改用
`read_source_strict`，非 UTF-8 一律 `[ENV-0030]` + rc=1。**D9② 已落地**（V32）：同一处读入
按 UTF-8 → GBK 两把严格判定解码，都不成立才红，`read_source_strict` 与 `verify` 侧四处收口到
`crate::source::decode` 一份实现。"交付侧先把语料转成 UTF-8"这条路随之作废——它要么改动语料
（AGENTS §6 不许），要么在 CLI 上开 `--encoding` 人工指定（同一份字节可由使用者选成两种文本，
正是本轮要消灭的形态）。`compile` 的有损 + `(non-utf8)` 面**刻意保留**：语料体检要的是一趟扫完
并按错误码分类，停在第一个编码问题会让体检本身没法用。风险换了形，没消失：现在的残留面是
**编码歧义**（3,149 个 GBK 可解文件里 418 个同时是合法 Big5 且两种解法文本不同），
判定依据是交付来源而非检测器，计数与判据见 architecture §13.8 与 validation V32.4。

---

## 4 真实交付件画像（codeweb A 路径实测）

分析对象：真实交付的存过与表结构目录（**目录名与本地路径从略**，下文"交付件"即指它），
**仅本地分析**：不外传、不入库、不复制进 SpinDuck 仓库；本文只记数量与形态，不抄内容。

| 指标 | 实测值 | 口径 |
|---|---|---|
| `.sql` 文件数 | **14,611**（共 263 MB） | `find -name '*.sql' \| wc -l`；`du -sh` |
| 总行数 | **5,628,370** | `find -print0 \| xargs -0 wc -l` 后**把所有批次的 total 相加**。本文 v3 及以前记的 147,618 是 `… \| tail -1` 只取到最后一批的部分和，**是错的**（14,611 个文件必然分批，`wc` 每批各打一行 total）；V26.3 保留该纠错 |
| 编码 | **混合**：UTF-8 可解 **11,462** / 需 GBK **3,149** / 两者都不通 **0**。需 GBK 的集中在 `数据拆分PROGRAM--GAUSS`（2,710 / 2,947 文件）、`012_VIEW`（395 / 1,210）、`sequence`（44 / 1,188），`sql/` **0** | python 逐文件 `decode('utf-8')` 失败再试 `decode('gbk')`；目录构成 `012_VIEW` 1,210 + `sequence` 1,188 + `数据拆分PROGRAM--GAUSS` 2,947 + `sql` 9,266 = 14,611。**D9② 后用被测面同一把刀复核**（`encoding_rs`，非 python）：**11,462 / 3,149 / 0 逐字相同**，即"两把判定都不成立"的文件在真实交付件里不存在（V32.4） |
| 编码歧义面 | 非 UTF-8 的 3,149 个文件中 **418 个**同时是合法 Big5 **且两种解法文本不同**；CJK 统一表意文字占比 GBK 94.8% / Big5 92.3%，**机械化判据不足以判别**——采信 GBK 的依据是交付来源（简体中文 GaussDB） | python 逐文件对同一批字节再 `decode('big5')` 比较；计数不携带任何文件内容。判据入 architecture §13.8 的残留风险条 |
| 非 UTF-8 里的**中文字面量** | 3,149 个需 GBK 的文件中，**2,384 个（75.7%）** 在单引号内出现 CJK，合计 **182,723** 处；仅注释含 CJK 的 761 个 | 正则 `'…[\u4e00-\u9fff]…'`（GBk 解码后匹配）；后果见 §3.5 |
| 图谱规模 | **43,432 节点 / 286,311 边**，14,611 文件，22.2 s，0 errors | `codeweb stats` |
| 对象构成 | 18,433 procedure / 2,148 function / 1,715 package / 9,315 table / 1,190 view / 762 sequence / 9,493 index | `codeweb stats` |
| 高危锁冲突 | **292 条**（`severity=high` 默认过滤） | `codeweb conflicts` |
| Oracle `(+)` 标记 | **7,906 处**，分布在 **574 个文件 / 7,695 个命中行**；标记在 `=` 右侧 4,876 处、左侧 1,921 处、不紧邻 `=` 余下 1,109 处。按目录：`012_VIEW` 293 处 / 100 文件，`数据拆分PROGRAM--GAUSS` 7,613 处 / 474 文件，`sql` 与 `sequence` **0**。单文件最高 213 处 | `grep -roF`（出现处）/ `-rlF`（文件）/ `-rcF` 求和（命中行，`grep -c` 计行不计次）/ `-roE`（左右侧） |
| 单过程闭包示例 | `pkg_split_trade_step3_sh.prc_trd_deal_purchase` → **4 个函数 + 1 个日志过程**（均带包限定）与 **14 张去重表**，含读 / 插 / 删插混合 | `codeweb trace --exact`，本机会话产物 |

**由此得到的四条硬事实**：

1. **交付件是"拆碎"的**：一文件一对象、混合编码、按目录分类且**层级深**（`012_VIEW/`、`sql/`、
   `数据拆分PROGRAM--GAUSS/018_FUNCTION/`、`sequence/`）。`call --file` 逐个手填不成立，**必须有
   闭包→文件清单的自动生成**（§6.2）；且现有资产发现**不看子目录**
   （`spinduck/src/project/fs.rs:49` 的 `sql_files` 忽略递归参数），`verify` 的目录口径也吃不下。
2. **`patterns.rs:67` 的"语料 8 处"是口径歧义，不是数字错**（v3 判为"错声明"，本轮核实后改判）：
   该数由 `scripts/extract_corpus_forms.py` 生成、落在 `config/corpus_forms.toml:54`
   （`oracle_outer_join_marker = 8`），口径是**本地考题语料（147 个文件，不入库）**里正则
   `\(\s*\+\s*\)` 的**出现次数**，全部来自 F140，并被两条门禁测试绑定——数字与它自己的口径是对的。
   缺的只是"哪一份语料"这个限定词，而 §4 的 7,906 是**交付件 23** 同口径的数。**两者不可比，
   也不该拿一个去否另一个**。注释已按此改写（`spinduck/src/sql/patterns.rs:67`）。
   真正的问题在改写器的两个**故意不改写**分支（标记侧不在 FROM 末位、两侧各带标记 → 单测
   `should_not_rewrite_outer_join_when_marker_side_is_not_last` /
   `should_not_rewrite_outer_join_across_two_marker_sides`）：7,906 处面前，"不改写"意味着把
   `(+)` 留给 DuckDB。**本轮实测到底是不是静默**：守护测试
   `spinduck/tests/guard/guard_patterns_outer_join_residual.rs` 三条全绿，第 3 条证明残留标记
   在**内核解析阶段就报错**（`Parser Error: syntax error at or near ")"`，回显含 `b.id(+)`），
   不是静默错值——§8 那条"预期显式失败"的推测由此升级为实测。剩余未量化的是**命中量**：
   7,906 处里有多少落进不改写分支，仍待 A0b 按形态逐类统计。
   注：`\(\+\) *= 标识符 \(\+\)` 口径下双侧同标为 **0 处**，"164 处"这一说项在本文口径下
   不可复现，**不予采信**，需要时另立精确定义再统计。
3. **编码是既成事实，不是待接能力**：SpinDuck 不是"解不了 GBK"，而是`call` 路径**用有损解码
   假装解得了**（§3.5 实测：同源码仅编码不同 → `MATCH`/`NO_MATCH`，两条 rc 都是 0；
   **该静默形态已由 D9① 消除，登记 V28**，现在是 GBK 一律 rc=1）。
   2,384 个文件、18 万处字面量在射程内。
4. **并行锁语义是新增差距面**：292 条高危冲突说明真实业务靠"过程间表级串行"约束正确性；
   DuckDB 是单写者快照模型，**没有** GaussDB 的行锁语义可对齐。真实例上并发跑多个过程
   （或与其他作业并发）不是性能问题而是**正确性**问题（§6.6）。

---

## 5 差距清单与决策记录

### 5.1 差距清单 R1–R10（编号以本文为准）

| # | 差距 | 判定口径（什么算补上了） |
|---|---|---|
| R1 | 发版产物无 GaussDB 通道 | 发布资产里的二进制 `materialize` 不再 `[ENV-0012]`；release 矩阵 feature 组合有 CLI 契约测试 |
| R2 | TLS/认证面未接 | 同一份 `--features cli,gauss` 产物能连 `sslmode=require` 与 `disable` 两种实例，口令仅经 env |
| R3 | `call` 那条路没有部署与门禁（本文 §3.3 三形态）。**部署能力本身已有**：`deploy_project` + `verify` 的闭包缺件红（V20）——差距是它不落盘、按目录而非闭包发现、`call` 不走它 | 有 `deploy` + `--plan`；缺任一对象即红，且**红之前不许产出 `RETURN`**；部署产物是一个可被 `call` 打开的 `--db` 文件 |
| R4 | 千万级取数 | 单表 1,000 万行 `materialize` 有实测时长与峰值 RSS；逐行字面量 INSERT 路径删除 |
| R5 | 千万级回写 | 有事务边界 + 幂等模式（append / overwrite / upsert-key）+ 失败可判定不残留 |
| R6 | 结果闭环缺失 | 有 `query`/`compare`；外部实例上能出一条"逐值比对报告"，红=退出码非 0 |
| R7 | 引擎语义上界在真实语料的命中量 | `MAX_TUPLES`、集合下标、包限定名、`(+)` 不改写分支四类各有命中统计（V26）。`(+)` 的**后果**一类本轮已定：残留标记 = 内核显式报错，非静默（§4 硬事实 #2） |
| R8 | 并行锁语义 | 明确声明支持/不支持，并在文档与 CLI 侧给出**单写者串行**的强制口径 |
| R9 | 源码入口的编码处理**四处两种**（§3.5：执行面 `call`/`explain` 静默有损、体检面 `compile` 打标记、核验面 `verify` 判红），其中执行面那条静默是本文最警惕的形态（实测 MATCH/NO_MATCH 分叉）；另有对象形态：14,611 碎文件深目录、包限定名扁平化 | **两条都已处置**（A2-D9① V28：执行面判红；A2-D9② V32：读入时 UTF-8 → GBK 两把严格判定，执行/计划/部署/核验四面收口到 `crate::source::decode` 一份实现，GBK 语料可跑且字面量逐字还原，体检面 `compile` 按 §3.5 口径保留有损 + 标记）。**剩余差距**：编码歧义（418 个文件同时合法 Big5，采信 GBK 的依据是交付来源，§13.8）；fixture 与真实闭包的表清单差异归零或有书面豁免 |
| R10 | 外部实例安全红线 | 角色开关 + 破坏性门禁 + 回写白名单，缺任一即拒连 |

### 5.2 决策记录（含被否方案）

> **D1 通道走 CLI，不引入服务或新 bin**：AGENTS §1 固定"单 crate 多 bin、CLI 零引擎逻辑"，
> 新增 `spinduck-agent`/HTTP 服务 = 新 `[[bin]]`（Ask-first）且把编排逻辑推离可测边界。
> 编排在 core 侧（`deploy/plan/compare` 全部有单元+集成测试），CLI 只做渲染与退出码。
>
> **D2 TLS 用驱动现成能力，否掉"自建 TLS"**：`gaussdb::sync` 已有 `tls-native-tls` +
> `MakeTlsConnector`，本仓只是没开。**否**自写 TLS 握手（重复造轮子 + 密码套件面扩大）。
> `sslmode` 从 env 读（`SPINDUCK_OG_SSLMODE`，默认 `disable` 保持现状口径），连接器在
> `GaussConn::connect_parts` 单点选择。**Ask-first**：动依赖 feature（native-tls 引入
> 平台 OpenSSL 依赖，触碰 AGENTS §10 平台矩阵与 `cargo audit` 豁免面）。
>
> **D3 取数/回写改 COPY 双向 + 事务 + 幂等模式**：驱动侧 `copy_out`/`copy_in` 已可用。
> **否**"保留逐行 INSERT、只加批量参数"：往返次数是数量级问题，不是常数问题。
> 首版落地走**零接缝变更**路线：DuckDB 侧用现有 `SqlExecutor::execute`
> 跑 `INSERT INTO t SELECT * FROM read_csv('<临时文件>')`，GaussDB 侧 COPY OUT 写同一文件；
> 只有当"临时目录不可注入"成为阻塞时才提 `BulkSink` 新接缝或 `SqlExecutor` 签名变更
> （后者 = Ask-first，破坏所有消费者）。
>
> **D4 codeweb 走进程外契约，否掉 crate 引入**。四条实测阻塞：
> ① 无 `[lib]`/`[[bin]]` 声明、仅 `src/main.rs`，**没有可链接的库目标**；
> ② 仓库**无 LICENSE 文件**（`Cargo.toml` 也没 `license` 字段），法律面不清；
> ③ 它 pin `ogsql-parser git tag v0.10.0`，而 SpinDuck 按 AGENTS §1 锁**本地 path 依赖
> `../lib/ogsql-parser` v0.11.x** —— 同一进程两份解析器实现，规则探针（AGENTS §4.1 探针层）
> 语义会被撕裂；
> ④ feature 面切不干净：`cargo build --no-default-features --features cli` **编译失败**
> （`Edge::ContainsSql` not found；`graph/mod.rs:793` 门控、`store.rs:1933` /
> `export/json.rs:902` / `mermaid.rs:156` 未门控），必须带默认 features（含 tui/jsp），
> 依赖面比 SpinDuck 全仓还大。
> **契约**：codeweb 二进制（外部工具，不进 `cargo audit` 面）→ `export -f json|ndjson`
> （机器可读全图）与 `conflicts`（JSON，`schema_version: 1`，默认 `severity_filter = high`）
> → SpinDuck 的 `plan` 输入；`trace --exact` 只有 tree/path 文本形态，**不作为契约面**。
> 计划文件里记 generator 版本，同输入必须同字节，红时先怀疑口径再怀疑实现。
>
> **D5 缺件即红，禁止静默**（回应 §3.3）：`--plan` 展开后，闭包内任一表/视图/序列/函数
> 未登记 → 列出**缺件清单**并退出非 0，且在红之前**不执行**目标例程。规则 `[rejects]`
> 语义不变，不动 golden。
>
> **D6 回写默认影子（shadow）**：真实例上的首次回写目标必须是 `sd_shadow_<table>` 影子表
> 或影子 schema，与源表同构、由 `plan` 生成 DDL，人工比对通过后才允许切到正式表；
> 配置面 `writeback.mode = shadow | append | overwrite | upsert`，默认 `shadow`。
> 影子表的**新建授权**走 §6.4 `SPINDUCK_OG_ALLOW_SHADOW_CREATE`（`role=source` 侧），与参照库的
> 整体重建授权（`ALLOW_SCHEMA_DROP`，`role=reference` 侧）分开。
>
> **D7 `MAX_TUPLES` 处置先量化后决策**：三个选项（提高上界 / 分批下沉缓存 / 维持显式拒绝）
> **现在都不选**——A0b 先在真实例上统计"18,433 个过程里有多少调用点的 DISTINCT 元组数
> 会破 100,000"。附带修掉"先 `fetch_many(max+1)` 再判超限"的形态（该处本可以带 LIMIT 探测，
> 不必把 100,001 行读进内存）。**该修复是行为改动**，先写守护测试锁定"现形态报
> `BIZ-0157` 且不提前读满 max+1 行才判超限"的现状，修复归属 A6（与量化结论同批落地），
> 不在 A0/A1 顺手改——A0 只补证据（A0b 之前无实例可测）。
>
> **D8 生产口径不改动黄金层语义承诺**：真实 GaussDB 实例**不**替换参照实现。
> `golden/README.md` "已知边界"里"若能访问真 GaussDB 就用同一夹具重跑替换参照"这一条
> **暂缓**——夹具与断言不变的前提是参照侧可安全重建，真实例不满足。参照仍是 A 模式容器，
> 真实例只做数据源/汇（§2）。
>
> **D9 非 UTF-8 源码：先让 `call` 变响，再谈真解码**。§3.5 实测的静默变形必须在任何
> 生产链路之前消失。两个选项：
> ① **零依赖**：`call --file` 把 `read_source` 的 `non_utf8` 透出来并**判红**（新增错误码，
> 走 AGENTS §5 `ENV-####` + 探针），口径向 `verify` 的 `[ENV-0030]` 看齐——三条入口立刻一致；
> ② **真解码**：引入 GBK 编码表（`encoding_rs` 一类）在 `read_source` 里按 GBK 解码。
> **否**只加告警不判红的中间态：§3.5 已证明 2,384 个文件里字面量带业务含义，
> "警告 + rc=0"在生产链路上等同于放行错值。
> 顺序定为 **①（归 A2，行为改动，先写守护测试）→ ②（新依赖 = Ask-first，另批）**。
> 附带约束：转码发生在**读入时**，语料文件逐字不动（AGENTS §6 语料纪律 + architecture §13.1）。
>
> **① 已落地（V28），落地形态与上面的写法有两处出入，都是往更简的方向**：
> (a) **复用** `ENV-0030` 而非新增错误码——与 `verify` 逐字符同形状是判红想要的结果，新增码
> 反而制造第 5 种入口行为，且免掉 AGENTS §5 的探针义务；(b) 实现是执行面改调
> `read_source_strict`（读到非 UTF-8 直接红），不是"透出 `non_utf8` 再判红"——观察到的契约相同，
> 但少一个中间态；判红点在 `register` 之前，所以红的那次不产出任何 `OUT`/`RETURN`/IR。
> **② 已落地（V32，Ask-first 已由用户授权 2026-11-10）**，落地形态与上面的写法有三处出入：
> (a) 解码不在 `read_source` 里，而是新增 `crate::source::{read_text, decode}` 作**唯一**读入实现——
> `read_source` 是 `compile` 体检面的有损入口，把 GBK 塞进它等于偷偷改掉 §3.5 刻意保留的那一面；
> (b) 判定顺序固定 **UTF-8 优先**，不是"探测到 GBK 就用 GBK"：交付件 11,462 个 UTF-8 文件里
> 5,130 个的字节同时是合法 GBK，按 GBK 读必然乱码（V32.4 一手计数）；
> (c) 消费面从"执行面两条"扩到**四条**（`read_source_strict` 覆盖 `call`/`explain`/`plan`/`call --plan`，
> 另把 `verify/deploy.rs` 四处 `read_to_string` 一并收口）——只改执行面会留下"`plan` 说闭包干净、
> `verify` 报编码红"的漂移，与 §3.3 缺件门反对的是同一类形态。附带约束原样成立：转码发生在
> **读入时**，语料文件逐字不动，引擎也不提供 `--encoding` 人工指定。

---

## 6 目标架构（新增能力面）

### 6.1 `run-plan`：把闭包变成可复现的运行单元

codeweb 图 → 一个目录内的计划文件（`.spinduck/plan/*.toml`，运行态布局遵循 architecture §11）。
下方是**字段示意**（含省略号）；**最终 schema 已随 A2 落地并写进 architecture §13.5**（实证 V29），
与草图有三处出入：对象前缀取 `relation:`/`sequence:`/`routine:`（链路抽取把视图与表归一类，没有
`table:`）、`generator` 现记 SpinDuck 自身版本（codeweb 接入后换它的版本，字段语义不变）、闭包来源
是 SpinDuck 自有的静态链路抽取而非 codeweb 图。

```toml
[plan]
target      = "pkg_split_trade_step3_sh.prc_trd_deal_purchase"
generator   = "codeweb 0.9.7"          # 产出方与版本，红时先怀疑口径
schema      = "bigfund"

[sources]
ddl_routines = ["sql/routines/pkg_split_trade_step3_sh.sql", …]   # 闭包内例程源
ddl_tables   = ["…"]                    # 表/视图/序列 DDL（deploy 用）
materialize  = ["par_sys_purchase", "par_fund_seat_para", …]       # 只读物化清单
writeback    = ["dat_trd_repurchase"]   # 回写白名单（默认空）

[[missing]]                              # 缺件红清单（非空即拒绝执行）
object = "table:par_oper_close_date"
from   = "pkg_util_rate.fnc_com_getday"
```

来源分工：**闭包/边/冲突**来自 codeweb；**对象 DDL 文本与类型保真**仍由 SpinDuck 自己解析
（codeweb 的图不含 DuckDB 列类型口径，architecture §13.1 类型保真是 SpinDuck 的承诺）。

### 6.2 新增子命令（四条，CLI 仅壳）

| 命令 | 职责 | 关键契约 |
|---|---|---|
| `deploy <plan>`（**已落地**，V30） | 按计划把清单里的资产装进**落盘**的本地 DuckDB：表/序列 → 视图（走真视图体，不是占位）→ `sys_dummy` → 登记例程 | **复用 `verify/deploy.rs` 的逐文件实现**（`deploy_ddl_file`/`deploy_view_file`/`ensure_engine_provided_tables`/`register_routine_files` 放开 `pub(crate)`），不重写一套建表口径。design 草图里的三点差异全部落地：⑴ 输入是**计划里的文件清单**（`sources.ddl_routines`/`ddl_tables`），不做目录发现，清单外资产一律不下发；⑵ 库**落盘**到 `--db`（开库发生在 CLI 接缝，`verify` 侧仍 `open_in_memory`，接缝签名未动；开库排在文件核对之后，否则留下半成品库）；⑶ 缺件红不再依赖 `[[entry]]`——`[[missing]]` 非空即 rc=1 且**不开库**，计划列出的文件缺失即 `ENV-0030`。草图"视图占位"的说法作废：视图体读的基表已进清单（V30.4 ②③），占位形态在本轮实测中为 `BODY=9 PLACEHOLDER=0`。落地后新增三条口径限制（原草图未预见）：**非幂等**（重跑撞 `KERNEL-0070`）、**例程不落盘**（进程内目录；换进程 `call --db` 不带 `--file` 得 `BIZ-0150`，免传要靠 `call --plan` = A2-4）、`plan.schema` 只透传不下发（对象进内核默认目录 `main`） |
| `plan [--dir <项目目录>] [--schema <名>] <routine>`（**已落地**，V29） | 目录扫描 + 静态闭包 → 生成计划并落盘 `<home>/plan/<target>.toml`（codeweb 图接入是替换输入，不是重写闭包） | 计划可复现（同输入同字节，V29.3 有真语料 `cmp` 证据）。三个现存限制里两条现状变化：⑴ `sql_files` **仍不递归**（§3.1），闭包按 `ddl/` + `ddl/tables/` + `ddl/views/` 三个固定目录发现资产；⑵ `[[entry]]` 的包限定要求（`spinduck/src/project/entry.rs:50`）**不约束 `plan`**——目标例程按目录命中即可，故独立函数无需先补 `[[entry]]`；⑶ "给独立函数合成可定位的全名"在闭包侧已由裸名补全 + 尾段匹配解决（V29.3 那三条 `MISS routine:` 即其 Red），**留下的缺口是 `plan` 不校验 `[[entry]]` 声明面**，入口本身齐不齐归 `verify` |
| `query <db> --sql-file F --out F.json`（**已落地**，V34） | 对**本地 DuckDB** 跑一条 SQL，结果集落成快照 JSON（`{"schema": […], "rows": [[文本|null]]}`） | 壳只做"读文件 → core → 渲染 → 退出码"，切句与形状判定全在 `src/snapshot/mod.rs::capture`。四条草图没写、实测逼出来的口径：⑴ **一次只接受一条语句**——一手发现：DuckDB 的 `prepare` 对 `SELECT 1; SELECT 2` **不报错**，两句都执行而只回最后一句的结果集，故"交给内核报红"这条路不存在，切句必须发生在发出去之前（`''`/`""`/`--`/`/* */` 安全），判红时一个 `--out` 字节都不写；⑵ 读 SQL 排在开库之前（`DuckExecutor::open` 即建文件）；⑶ 单元格落的是**内核给回的文本**，归一留给 `compare`——口径调整不必重跑查询；⑷ 结果集整体驻留（`RowStream` 契约如此），千万级取数属 A3 的 COPY OUT 改造，本轮不为它动 `SqlExecutor` 接缝签名。**仍缺的一半**：`--db` 只接本地文件，参照侧实例采集要等 A3 |
| `compare a.json b.json [--key k1,k2]`（**已落地**，V34） | 两份快照**逐值**裁决：绿 = `diffs=0` rc=0；红 = `DIFF …` 行 + `diffs={k} 逐值不一致` rc=1 | 判**值**不判文本（列名不是判据、列数是），四型差异 `value`/`arity`/`only-in-a`/`only-in-b`，行号 1 起，列名取**左快照**；一趟跑完所有行**不早停**；`--key` 下按归一化键元组配对（键列大小写不敏感），**键列找不到即 `ENV-0031`，绝不退化成位置序**。归一口径现收口成 core 单一实现 `src/norm.rs`（草图里点名的两份副本 `tests/common/mod.rs:50`、`tests/corpus_forms/mod.rs:149` 已改成 re-export，语句序列逐句相同、只差形参名与一行注释，见 V34.3）——草图判断"只搬一份会漂移"成立，两份一起搬 |

### 6.3 数据面改造

```
取数：GaussDB COPY OUT → CSV(TEXT, NULL '\N') → DuckDB read_csv → INSERT INTO t SELECT …
回写：DuckDB COPY (SELECT …) TO CSV → GaussDB COPY IN（单事务/表）→ 行数校验 → COMMIT
```

类型保真沿用 architecture §13.1（文本承载 + DuckDB 列类型转换），但**必须显式声明** `FORMAT CSV,
NULL '\N', DELIMITER ','`，并对含分隔符/换行的文本走 `QUOTE ''''`；`compare` 之前不允许
用"行数相等"代替"逐值相等"。

> **方言一致性是新增风险面**：取数链是 GaussDB `COPY OUT` 写 CSV、DuckDB `read_csv` 读；
> 回写链是 DuckDB `COPY TO` 写、GaussDB `COPY IN` 读——两侧 NULL/QUOTE/ESCAPE/换行字段
> 的编码方式**不是同一实现**，方言不齐会静默错值（恰是本文最警惕的失败形态）。A3/A4
> 必须有"含分隔符/换行/NULL 歧义字段的往返逐值一致"用例（§8 风险表）。

### 6.4 安全门禁（R10）

| 开关 | 取值 | 效果 |
|---|---|---|
| `SPINDUCK_OG_ROLE` | `reference` / `source`（默认 `source`） | `source` 下 harness 与 CLI 一律**拒绝** `DROP SCHEMA`/整体重建 |
| `SPINDUCK_OG_ALLOW_SCHEMA_DROP` | `1` 才放行 | **仅 `role=reference` 生效**（整体重建参照库）；`role=source` 下即使误设也拒绝。仅 CI 容器环境设置 |
| `SPINDUCK_OG_ALLOW_SHADOW_CREATE` | `1` 才放行 | **仅 `role=source` 生效**，且只放行 `CREATE TABLE sd_shadow_*`：前缀 + 目标表不存在 + `writeback.mode=shadow` 三重约束；不放行任何 DROP/TRUNCATE/ALTER（D6 的例外口） |
| 回写白名单 | `plan.sources.writeback` | 不在名单的表 → 新增 `ENV-####` 错误拒绝（错误码挂探针，AGENTS §5），无隐式默认 |

> 两个开关各管一种授权、互斥生效——v2 用同一个 `ALLOW_DESTRUCTIVE` 同时表达"reference 可整体重建"与"source 可新建影子表"，会让"生产误设也不生效"这句自相矛盾，本轮拆开。

口令仍只经 `SPINDUCK_OG_PASSWORD` env（architecture §10.4 口径不变），不进 argv、不进计划文件、不入库。

### 6.5 与 golden 的关系

`golden/` 的**语义承诺**不动：容器、夹具、`expected.json` 与断言一条不改、不删。
唯一允许的文件级改动是 A1 给 harness 的 `reset_reference` 加 env 守卫包装——只有
`role=reference` 且 `SPINDUCK_OG_ALLOW_SCHEMA_DROP=1` 同时成立才执行 `DROP SCHEMA`
（堵 §2 红线），包装前后 CI golden job 行为逐字节等价，且该改动有守护测试锁定两种
取值组合（放行组合仍执行重建，其余三组合一律拒绝）；该文件属"人类已有测试"，改动
走 Ask-first。生产路径的新增测试落在 `spinduck/tests/`（真实内核集成层，按版本 gate）与
CLI 契约层（退出码 + stdout），**不新增 golden 用例**——golden 的语义承诺是"参照实现逐行一致"，
不是"连得上外部实例"。
> **CLI 契约层的落点（本轮已补齐）**：AGENTS §4.1 登记的 `spinduck/tests/cli.rs` 此前**不存在**，
> 现存 CLI 契约测试是 crate 内单测（如 `spinduck/src/cli/run.rs:479`
> `should_fail_verify_when_project_file_is_missing`——`run_str` 进程内调 `run()` 拿返回码，
> **不起子进程**，所以"OS 观察到的退出码"与"stderr 恒空"当时无人裁决）。本轮按 AGENTS
> 新建该层：27 条用例逐个真子进程跑 `--features cli` 构建的二进制，锁退出码 + stdout
> （落点与实证见 validation V27）。规范文本未改动。D9① 落地后本层为 **29 条**（V28.3/V28.6），
> `plan`/`deploy`/缺件门三轮到 **48 条**（V31.7），D9② 后 **52 条**（V32.3）。
> 曾留下的一条代价已由用户决策消化：`[[test]] cli` 带 `required-features = ["cli"]`（该 bin 需要
> `cli`），而常驻 CI 的 Test job 原本只跑 `--features native`，因此这一层**不进常驻门禁**。
> V28 起 Test job 追加 `cargo test --features cli` 一步（用户授权），本机口径
> `cargo test --features cli` = 325 passed / 0 failed（native 口径仍 296，不含本层）。
> **AGENTS §9 的 Test job 描述未同步**（仍写只跑 `--features native`）——规范正文属人类所有，
> 是否在 PR 里一并改由用户定；GitHub 侧这一步也还没跑过。

### 6.6 并发与锁（R8）

真实交付件 292 条高危冲突 ⇒ 生产正确性依赖 GaussDB 侧的串行化。口径：
**SpinDuck 的回写以"单写者、一表一事务"为强制模型**，并发多过程写同一表不在承诺范围内；
需要并发时由外部调度器（原业务侧的 job 序列）保证互斥，`plan` 里以
`[concurrency] writer_lock = "table"` 显式声明。此项只承诺**不假装支持**，不实现锁。

---

## 7 分阶段落地与门禁

每阶段一个 TDD 循环簇，退出判据必须给命令与实测数字（AGENTS §11 汇报口径）。

| 阶段 | 内容 | 交付物 | 退出判据 | Ask-first |
|---|---|---|---|---|
| **A0a 本机取证**（零功能改动，**本轮已完成**） | 交付件画像重测（含两处纠错：行数 `tail -1` 缺陷、"GBK"改判混合编码）；`(+)` 不改写分支的**后果**实测（守护测试 3 条，内核显式报错）；§3.3 三形态复现；三条源码入口的编码行为实测（§3.5）；`patterns.rs` 注释口径改写；`verify`+`deploy_project` 能力盘点（§3.1 改判） | V26.1–V26.5 + 注释修正 + `guard_patterns_outer_join_residual.rs` | 每个数字有可复现命令；**已完成**，见 V26 | 否 |
| **A0b 实例侧取证**（**待实例**，非本机可完成） | 真实例握手（`sslmode`、`SELECT datcompatibility`、`SHOW server_encoding` / `client_encoding`，附录 B 五条）；千万表 `materialize` 实测——**现路径按 §3.2 会先 OOM**，故降级口径：10⁵/10⁶ 行逐档实测时长+RSS 曲线，10⁷ 行仅在成功时记录，OOM/失败本身作为一手数字登记；`sync_append` 1 万行实测；`(+)` **四类形态命中量**统计（后果已在 A0a 定为显式红）；闭包示例例程在 `register` 上的**编译通过率** | V26 追加小节 | 每个数字有可复现命令；V26 里**明写哪些仍是推算**（含 10⁷ 行是否属推算的判定） | 否 |
| **A1 通道可达** | 发版 feature 组合加 `gauss`；TLS 连接器 + `SPINDUCK_OG_SSLMODE`；角色开关与破坏性门禁（§6.4） | 产物 + CLI 契约测试（落点见 §6.5 注）+ harness 门禁 + **`.github/workflows/ci.yml` 的 golden job env 追加**（现仅注入 `HOST/PORT/USER/DB`（`ci.yml:116`）与 `PASSWORD`（`:195`），须补 `SPINDUCK_OG_ROLE=reference` + `SPINDUCK_OG_ALLOW_SCHEMA_DROP=1`，否则 §6.4 的"仅 CI 设置"不成立） | 真实例 `materialize` 小表绿；`role=source` 时 `DROP SCHEMA` 路径必红；CI golden job 不受影响；workflow 改动单独说明（改 CI 定义属敏感项，不夹带在其他阶段里） | 是（依赖 feature、CLI 契约、CI 定义） |
| **A2 计划与部署** | `plan` + `deploy`（复用 `deploy_project`，库落盘到 `--db`）+ 缺件红扩到 `call` 路径（D5）+ 编码 **D9①**（`call` 见非 UTF-8 判红；D9② 真 GBK 解码另批 Ask-first）—— **D9① 已完成（V28）；`plan` 已完成（V29：schema 入 architecture §13.5，`plan` 子命令 + 6 条契约）；`deploy` 已完成（V30：CLI 仅壳 + `plan::deploy` 的 resolve/apply 两段 + 复用 `verify/deploy.rs` 逐文件实现 + 7 条契约；顺带修掉两处让计划漏件的闭包缺陷——视图体基表从不展开、逗号连表只取第一个关系，见 V30.4）；缺件红扩到 `call` + `call --plan` **已完成（V31**：闭包 BFS 收口成一份（`plan::closure::walk_chain`），`plan` 以项目 DDL 声明为参照、`call` 以内核目录为参照；`call` 另加 `--plan` 入口，目标与例程源清单都从计划取，契约 6 条 + `args` 单测 2 条，见 V31**）。**D9② 真 GBK 解码已完成（V32**：新增 `crate::source`（UTF-8 严格 → GBK 严格 → `ENV-0030`），`read_source_strict` 与 `verify/deploy.rs` 四处收口到它，`encoding_rs` 由传递依赖提为直接依赖、`Cargo.lock` 只多一行，契约 52 条**）；**A2-5 表达式级被调函数进闭包已完成（V33**：链路抽取新增第三个采集面 `chain::expr_func_names`（交**候选**而非结论），`walk_chain` 逐个拿例程目录精确查名，命中才把被调函数排进 BFS——判定口径与 `eval_funccall` 的单次 `catalog.lookup` 逐字同源，故未命中的裸名（`trunc`/改名后的 `nvl`…）丢弃而非报假缺件；`plan`/`deploy`/`call` 门三处同一条 BFS 一起受益，契约 56 条：`plan` 侧 3 条 + `call` 侧 1 条 + 采集面单测 1 条**）——A2 的设计内本机项到此**全部完成**） | 计划 schema + `deploy`/`call` 测试 + 编码守护测试（编码那条已入 `tests/cli/{call,rules_explain}.rs`，V28.3；计划面入 `tests/cli/plan.rs`，V29.1；部署面入 `tests/cli/deploy.rs`，V30.1；执行面门入 `tests/cli/call.rs`，计划面入口入 `tests/cli/deploy.rs` + `usage.rs`，V31.1） | §4 闭包示例的 `[[missing]]` 为空（**已到**，V29.3：考题语料 10 条 entry 全部 `missing=0`，含逐字节可复现与 release 产物复核）；缺一张表 → 非 0 且**不执行例程**（**已到**，V31.1：形态 C 现在 `MISS relation:par_sys_purchase <- …` + rc=1 且无 `RETURN`；`plan`/`deploy` 侧的缺件 rc=1 之外，执行面第一次真的红在 `eng.call` 之前。判据的另一半——"门不是永红"——有同批次正例：`deploy` 后的库上 `call --plan` 跑通考题 entry `pkg_split_trade_step3_sh.prc_trd_deal_purchase`，门静默、`OUT p_o_succeed = 0`、rc=0，V31.3）；GBK 源经 `call` → 非 0（**已到过又改判**，V28.3 判红；D9② 后该形态换成"GBK 源 rc=0 且字面量逐字还原、两把判定都不成立才非 0"，V32.3 同时锁两侧——`explain`/`verify` 同步）；被调函数在表达式里被调 ⇒ 它自己的表进计划、并进门的清单（**已到**，V33.2/V33.3：最小夹具 `plan` 报 `MISS relation:t_hidden <- fnc_hidden`、`call` 同形红且不执行；判据的另一半同样有实测——考题语料 10 条 entry 的 `routines`/`tables` 清单同步变大（entry5 7/20 → 24/41）而 `missing` 仍全为 0，见 V33.4） | 是（新子命令 + 行为改动） |
| **A3 千万级取数** | COPY OUT + `read_csv` 批量装载，删逐行 INSERT | `materialize` 新实现 + 回归 | 1,000 万行实测 < 逐行路径且 RSS 有上界记录；含分隔符/换行/NULL 歧义字段的**往返逐值一致**用例绿（§6.3 方言风险） | 视实现（触接缝才提） |
| **A4 回写** | COPY IN + 事务 + 幂等模式 + 影子表（D3/D6） | `sync` 新实现 + 断点/失败测试 | 中途 kill 后源表零残留或全量可判定；影子表与源表逐值一致（含 CSV 方言歧义字段用例） | 是（CLI 契约） |
| **A5 结果闭环** | `query` + `compare` + 端到端编排（`run <plan>`） | 报告格式 + CLI 契约 | **本机半边已到（V34）**：`query`/`compare` 两条子命令 + 18 条契约（契约层 56 → 74）、发版产物八场面冒烟逐字（含多语句红不落盘、`od -c` 证结尾换行）、归一收口三层复跑全绿；判据的另一半同样有实测——把 `args.rs` 两条 dispatch 摘掉即 `56 passed / 18 failed`（V34.2），说明这 18 条真在守着新命令而非陪跑。**实例半边仍未到**：参照侧采集（GaussDB → 同款快照）依赖 A3，`run <plan>` 一条命令跑通"真实例 → 本地 → 回写 → 比对"仍待 A4/A5 | 是（新子命令） |
| **A6 语义治理** | 按 A0b 量化处置 R7（D7）与 R8 声明 | V26 追加 + 机制入 architecture | 每类命中要么有测试要么有书面"不支持" | 视选项 |

依赖：A0a（已完成）→ **A0b 与 A1 都要等实例** → A2 → {A3, A4} → A5 → A6。A3/A4 可并行，
但 A5 需要两者都绿。**A2 与 A0b 无依赖关系**：`deploy` 落盘、缺件红、`call` 判红三件事在
本机构架 + 现有 golden 夹具上就能测完，等实例期间可以推进。

> **V34 对该依赖的一处提前**：A5 的**本机半边**（`query`/`compare`）不等 A3/A4 就能落地并取证，
> 因为它两边读的都是内核快照、不触碰实例通道——用户指令即"等实例期间把这条也收掉"。
> 被提前的只有载体与裁决，**编排（`run <plan>`）与参照侧采集仍在 A3/A4 之后**，
> 所以"A5 需要两者都绿"这句对 A5 的**退出判据整体**仍然成立（见上表的两半拆分）。

---

## 8 风险与未决（诚实清单）

| 项 | 状态 |
|---|---|
| 真实实例的 `sslmode` 与 `datcompatibility` | **未实测**（A0b 首要动作，等实例）。SpinDuck 的映射口径建立在 A 模式上，非 A 模式实例的函数语义差异未评估 |
| 千万级实测时长/RSS | **只有推算，无一手数字**；对外声称性能一律禁止（A0b 之前 V26 也不会有这类数字） |
| `(+)` 不改写分支的真实后果 | **已实测（A0a，V26.2）**：残留 `(+)` 在内核**解析阶段显式报错**（`Parser Error: syntax error at or near ")"`），**不是静默错值**——§8 初稿的推测成立，优先级不抬高。守护 `spinduck/tests/guard/guard_patterns_outer_join_residual.rs`。仍未量化的是**命中量**（7,906 处里多少落进不改写分支，归 A0b） |
| **非 UTF-8 源码经 `call` 静默变形** | **已实测（A0a，V26.4）→ 两步处置完成（A2-D9① V28 + A2-D9② V32）**：改动前同源码仅编码不同 → `MATCH` / `NO_MATCH` 且两条 rc=0；D9① 后执行面一律 `[ENV-0030]` + rc=1；D9② 后按 UTF-8 → GBK 两把严格判定解码，两侧同结果、`compile` 体检面不变，回归面 `tests/cli/{call,rules_explain,verify,compile}.rs`（V32.3）。**风险换了形**：现在的残留面是**编码歧义**——3,149 个 GBK 可解文件里 418 个同时也是合法 Big5 且文本不同，采信 GBK 的依据是交付来源（简体中文 GaussDB）而非检测器，计数与判据见 architecture §13.8 / validation V32.4 |
| **CLI 契约层自身的 CI 覆盖** | 已补：常驻 Test job 在 `cargo test --features native` 之后加一步 `cargo test --features cli`（用户授权，V28.4）⇒ `[[test]] cli` 现在进常驻门禁。**留下的是一条新口径**：AGENTS §9 把 Test job 写成只跑 `--features native`，与 workflow 现状不一致，规范正文要不要同步由用户定（本轮未改 AGENTS.md）。CI 侧这一步是否真的绿只有 GitHub 跑过才知道，本机数字不代替它（该轮实测 325/296，D9② 后为 native 327 / cli 379，见 V32.6） |
| codeweb 契约稳定性 | `store.bincode` 456 MB、`schema_version: 1`；跨版本读取不保证。缓解已落地：计划文件记 `generator`（V29，architecture §13.5）。**codeweb 图尚未接入**——A2-2 的闭包由 SpinDuck 自有的静态链路抽取产出（architecture §13.5 口径表），接入时替换的只是那一处输入，计划 schema 与契约不变 |
| **本文自测数字自身的可靠性** | 已踩到一次：`wc -l` 接 `… \| tail -1` 只取到"最后一批 total"，14,611 个文件必然分批 → 记成 147,618（真值 5,628,370）。同口径教训适用于任何 `xargs` 聚合：**必须把所有批次求和**。V26 里每条数字都附命令，就是为了让人能这样复查 |
| 包限定名扁平化 | `register` 产出的名字面是 `pkg.proc` 小写拼接（`spinduck/src/engine.rs:138-190`），真实闭包里同名函数在不同包下的可分辨性未验证。**同包裸名调用这一半已解决**（V29.3：闭包按调用方所属包补全，与 `verify` 同口径，考题语料三条 `MISS routine:` 假缺件转绿）；**跨包同名仍未验证**。真实闭包 14 表里 **4 张**不在 `case_corpus_purchase` 夹具 DDL（`par_oper_close_date`、`par_sys_marketrate`、`split_log_info`、`sys_dummy`）——这条是 codeweb 时代的手数推算，A2-2 的自有闭包**未复现过它**；其中 `sys_dummy` 已由 V29.4 ② 改判为"引擎自建对象"（既不是缺件也不进部署清单）。**推算的"4 张缺表"已由 `deploy`/计划实跑复现否证（V30.5，一手）**：`case_corpus_purchase` 夹具计划 `ddl_tables=1`、`missing=0`，四个名字里 `par_oper_close_date`/`par_sys_marketrate`/`split_log_info` 在其 `sql/` 下**一次都不出现**（`sys_dummy` 为引擎自建）——即"4 张缺表"不属 SpinDuck 侧闭包，要它才成立的是当年 codeweb 的手工数口径。动 golden fixture 不再是待办，对齐 fixture = 动 golden 仍属 **Ask-first** |
| ~~**视图体从未下发内核**~~（V29.7 登记 → **V30.4 已修**） | 两条成因叠加，都已实测：① **部署面**——`deploy_view` 原先用文本 `contains("CREATE VIEW")` 决定是否走视图体，而语料 9 个 `1_V_*.sql` 在关键字中间插了 `/*FORCE*/` ⇒ 判定不成立，静默落到 `CREATE OR REPLACE VIEW <名> AS SELECT 1` 占位分支（**V30 起与注册面共用同一个 `creates_view()` 判定，注释不参与**）；② **闭包面**——计划从不展开视图体，视图读的基表压根不在 `ddl_tables` 清单里，即便走真视图体也会因基表缺失而内核报错；③ 同一把刀口还漏了逗号连表（`FROM t_a t, t_b b` 只取第一个关系），`verify` 与 `plan` 一起少算。实测口径（V30.4）：考题语料视图探针 `BODY=9 PLACEHOLDER=0`；计划表数 12→28（bjgsyh）、15→27（hz_byfund）、18→20（cjqs 与 gen_inst）、22→23（step9）；10 份计划仍全部 `missing=0`；`verify` 两语料判定**逐字未变**（无 PASS↔FAIL 翻转）。残留三条：语料 11 个视图文件两两无引用，视图套视图的递归分支只有单测形态覆盖；**4 张视图不在任何 entry 闭包内**（`V_DAT_INST_SECU_ACNT_INFO`/`V_DAT_INST_SECU_DEAL_INFO`/`V_JK_BDP_DIC_ALL_KIND`/`V_JK_DAT_INST_INDEX`），本轮既不下发也不探测；黄金层（`--features gauss`）是否受影响**未本机实测**，只有 CI golden job 有资格回答 |
| **`deploy` 落地的三条限制**（V30 新登记，design 草图未预见） | ① **非幂等**（一手）：对已装配的库文件重跑即撞 `[KERNEL-0070] 建表 …: Catalog Error: Table with name "dat_trd_repurchase" already exists!` rc=1；语义是"从零装配一个库"，重来换文件或先删。**未做** `--force`/逐对象 `IF NOT EXISTS` 化（要先定"跳过还是覆盖"口径）。② **例程不落盘**（一手）：注册发生在**进程内**例程目录（`spinduck/src/catalog.rs` 的 `register`），库文件里只有表/序列/视图与数据，故换进程 `call --db` 不带 `--file` 得 `[BIZ-0150] routine … not registered`——"部署好的库可独立跑存过"这句不成立，免传源码要靠 A2-4 的 `call --plan`（**V31 已落地**：目标例程与闭包内例程源都从计划取，`--file` 退化为"计划外追加"；正例见 V31.3）。③ **`plan.schema` 不下发**：对象一律进内核默认目录 `main`（与 `verify` 既有判定口径一致）；§6.1 草图给了 `schema` 字段但未约定 `deploy` 用它建 schema，本轮记为未落地，architecture §13.6 登记为偏差而非假装支持 |
| **缺件门的盲区：表达式级被调函数**（V31 登记 → **V33 收口**，一手） | 原形态：`routine_refs` 只采**语句级** `CallRoutine`，PL 赋值/条件里的 `v := fnc_x()` 不进闭包 ⇒ `plan prc_outer` = `routines=1 tables=1 missing=0`（**计划自称干净**），同项目 `plan fnc_hidden` = `missing=1`；执行面后果响亮（`KERNEL-0010` Scalar Function / Table does not exist），不是静默值，但语料在被调体外自套 `WHEN OTHERS` 时仍会归成合法值。**收口办法与偏差**：不改 `routine_refs` 的口径（裸名一律算例程 = 每个 `trunc` 都成假缺件），而是新增采集面 `chain::expr_func_names` 交**候选名**，由 `walk_chain` 拿例程目录逐个精确查名——命中才进 BFS，于是被调函数**它自己的表**同时进计划与门；判据与 `runtime::eval::eval_funccall` 逐字同源（那里也只做一次 `catalog.lookup`），所以门不比它守护的调用更宽或更严。**剩下两类仍看不见**（一手实测，V33.4）：① 被调函数源码没进本次注册面（`call` 少给一个 `--file`）——目录里没有该名字，门与运行期同样无从判断，后果仍是内核响亮报错；② 同包成员函数按裸名调用（登记名 `pkg.fnc_x`）——缺口在**分派**面而非闭包面，闭包若补全包名就要为一次根本不会命中的调用铺表。考题语料里 ② 实测为 0 条：把"未命中的裸名也报缺件"这一变体在同一条闭包上跑出来，9 条 entry 共 23 行 `MISS routine:`，去重后只有 4 个名字（`coalesce`、`now`、`round`、`substring`），全部是内建或规则改名后的内建（`NVL`→`coalesce`、`SYSDATE`→`now`、`SUBSTR`→`substring`）——即语料里没有"裸名调到同包成员函数"这一形态 |
| DuckDB 单写者 vs GaussDB 行锁 | 见 §6.6，只承诺不假装支持 |
| **结果快照的内存天花板**（V34 新登记） | `query` 的 `capture` 把结果集**整体驻留**（`RowStream` 的契约就是快照：`len()` + `fetch_many(n)`），本机一手证明的只有"不是截断分页"（5000 行，V34.7）。千万行会不会 OOM 属**推算**，不给数字；出口是 A3 的 COPY OUT 流式装载，届时要么扩接缝（Ask first，§1）要么让 `query` 走独立通道。**不为它提前改签名**的代价写清：A5 本机半边现在能裁决的是**聚合输出与小表全量**，不是大表全量 |
| **DuckDB 对多语句不报错**（V34 一手发现） | `prepare`/`query` 收到 `SELECT 1; SELECT 2` 时 rc=0、两句都执行、只回最后一句的结果集——即"把整段文本交给内核，指望它报语法错"这条路在内核侧**不成立**。这条事实永久钉在 `spinduck/src/snapshot/mod.rs` 的 `kernel_evidence_tests`（`native` gate），`capture` 因此在发出去之前切句拒多语句并保证零侧效应。它同时是 §6.3 未来任何"一次喂一段脚本"设计的**前置警告**：内核的宽容不等于我们的宽容 |
| CSV 双向方言一致性（§6.3） | **未实测**：GaussDB COPY 与 DuckDB COPY 的 NULL/QUOTE/ESCAPE/换行编码不是同一实现；A3/A4 各有一条歧义字段往返用例兜底，方言不齐属显式红 |
| native-tls 对平台矩阵（AGENTS §10）与 `cargo audit` 豁免面的影响 | 未评估，A1 前必须给结论 |

---

## 9 验收标准（Definition of Done）

1. `spinduck-<ver>-<os>-<arch>` 发布包内二进制能在**真实 GaussDB**（TLS 或明文，实测确认的那一种）
   上，用一条命令完成：闭包 → 建表 → 物化（千万级，有实测数字）→ 执行存过 → 回写（影子）→ 逐值比对。
2. 上述链路红绿可判定：缺件、缺表、超限、编码不可解、比对不一致五类各自有**非 0 退出码 +
   可读定位**，且**不存在**"rc=0 但值为 0/空"的静默形态（§3.3 的三形态与 §3.5 的
   MATCH/NO_MATCH 分叉各有回归守护，永久保留）。
   进度：**编码一类已达成并扩到四面**——D9① 后分叉不复存在（`call`/`explain` 判红 +
   同段体 UTF-8 仍正常返回，V28.3）；D9② 后"编码不可解"缩到**两把判定都不成立**的字节，
   守护随之换成三条锁：GBK 源与 UTF-8 twin **同为 `RETURN MATCH` rc=0**（判据实验，V32.3）、
   `explain` 的 GBK 与 UTF-8 给逐字相同的 IR 行、`verify` 的 GBK 包体 `rc=0` 且 `PASS Entry`；
   判红那侧的输入改成 `0xFF 0xFE`（UTF-8 与 GBK 都不合法），`compile` 的 `(non-utf8)` 面不变。
   **缺件一类已达成（V31，A2-4）**——`call` 两条入口都在 `eng.call` 之前红：形态 A/B/C 同形为
   `MISS relation:par_sys_purchase <- …` + rc=1，且红之前不产出 `RETURN`/`OUT`；`call --plan`
   另加一条同口径的计划门（缺件即拒、**连库都不开**）。三形态的守护本身在案，只是期望值记的
   是当时的缺陷快照（§3.3 现状段给出改判后原文）。表达式级被调函数**也已进闭包（V33，A2-5）**：
   最小夹具现在 `MISS relation:t_hidden <- fnc_hidden` + rc=1，考题语料 10 条 entry 的清单同步
   变大而 `missing` 仍为 0（V33.4）。**残留**两类（§8 盲区行改判后的口径）：被调函数源码没进
   本次注册面、同包成员函数按裸名调用——前者仍由内核响亮报错，后者缺口在解释器分派面而非本门。
3. `role=source` 下对**既有对象**的 DROP/TRUNCATE/重建路径必红；`sd_shadow_*` 新建只在 `SPINDUCK_OG_ALLOW_SHADOW_CREATE=1` 下放行（§6.4 例外口，不属于本条的"必红"面）；CI 五常驻门禁 + golden job 全绿，golden 断言与基线**零改动**。
4. V26 登记全部实测数字与命令，并逐条标注"已实测 / 仍是推算"。
5. 机制段迁入 `docs/architecture.md`（§13 延伸 + 决策记录追加 D1–D9 编号），本文降级为历史设计依据或并入。

---

## 附录 A 命令速查

```bash
# —— 现状可用（需 --features cli,gauss 自建产物；发版产物不含 gauss）——
cd spinduck && cargo build --release --features cli,gauss
export SPINDUCK_OG_HOST=… SPINDUCK_OG_PORT=… SPINDUCK_OG_USER=… \
       SPINDUCK_OG_DB=… SPINDUCK_OG_PASSWORD=…          # 口令只经 env，不进 argv
./target/release/spinduck materialize --db run.duckdb --table par_sys_purchase \
    --where "dt='20250715'"
./target/release/spinduck call --db run.duckdb --file <routine.sql> <routine> <args…>
./target/release/spinduck sync --db run.duckdb --table <t> --cols a,b
# 三条子命令的参数形态以 spinduck/src/cli/args.rs:251 usage() 为准
# cargo 命令 cwd 必须是 spinduck/（crate 在那儿）；`verify` 则相反——它从 cwd 向上找
# spinduck.toml，在仓库根执行得到 [ENV-0030] 未找到项目声明（实测 rc=1），
# 所以要么 cd 到项目目录，要么把目录作位置参数传：spinduck verify golden/case_corpus_purchase

# —— A5 本机半边（V34 已落地；发版产物即可，cli ⊇ native，不碰实例通道）——
# 夹具：t_a(id INT) + prc_ins(p_i IN NUMBER) 插一行；下面四条命令即 V34.5 那张表的输入
./target/release/spinduck plan  --dir /tmp/a5smoke prc_ins                     # → .spinduck/plan/prc_ins.toml
./target/release/spinduck deploy /tmp/a5smoke/.spinduck/plan/prc_ins.toml --db work.duckdb
./target/release/spinduck call --db work.duckdb --file sql/routines/10_ins.sql prc_ins 1   # 再跑一次实参 2 ⇒ 库里两行
./target/release/spinduck query work.duckdb --sql-file q.sql --out a.json      # query: rows=2 cols=1 out=a.json
./target/release/spinduck compare a.json b.json                               # b 写 ["1.0"] / ["+2"]：diffs=0        rc=0
./target/release/spinduck compare a.json c.json                               # c 第二行 ["3"]：DIFF value row=2 col=id a=2 b=3  rc=1
./target/release/spinduck compare k1.json k2.json --key k                     # 两侧到达序相反：compare: key=k rows=2/2 diffs=0
# 一次只接受一条语句（内核不报多语句，见 §8 那条一手发现）：
#   [ENV-0031] query: SQL 文件里有 2 条语句，一次只跑一条（拆开写）            rc=1，且 --out 一个字节都不写

# —— 门禁（AGENTS §8）——
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings
cargo test --features native

# —— §3.1「verify 已具备部署 + 缺件红」的复现（发版二进制即可，cli ⊇ native）——
cd golden/case_corpus_purchase && ../../spinduck/target/release/spinduck verify .
#   → entries=1 checks=11 failed=0 skipped=1，rc=0（表/视图/桩/例程/种子全下发到内存 DuckDB）

# —— §3.3 三形态复现（同一二进制，只差 --file 传什么）——
# 现状（D5/A2-4 已落地，V31.1）：A/B/C 三条同形，红在 eng.call 之前——
#   MISS relation:par_sys_purchase <- fnc_get_purchase_js_days
#   call: target=fnc_get_purchase_js_days missing=1 未执行（design D5 缺件即红）      rc=1
# 下面逐条标注的是 V26.1 的缺陷快照（当时的输出/退出码），A/B 的 KERNEL-0010 现已不可达。
BIN=../../spinduck/target/release/spinduck      # cwd 仍是 golden/case_corpus_purchase
$BIN call --db /tmp/sd_A.duckdb --file sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql \
      fnc_get_purchase_js_days "'20250715'" "'0130000001'"                  # A → KERNEL-0010 rc=1
$BIN call --db /tmp/sd_B.duckdb --file sql/ddl/tables.sql --file cases/datasets/seed.sql \
      --file sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql \
      fnc_get_purchase_js_days "'20250715'" "'0130000001'"                  # B → 同 A，rc=1
$BIN call --db /tmp/sd_C.duckdb \
      --file sql/routines/0_SIMPLIFIED_fnc_com_getday.sql \
      --file sql/routines/0_SIMPLIFIED_fnc_com_if_deal_date.sql \
      --file sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql \
      fnc_get_purchase_js_days "'20250715'" "'0130000001'"                  # C → 快照 RETURN 0 rc=0，现 MISS + rc=1

# —— §3.5 编码入口复现（自造两份同内容、不同编码的最小夹具，勿用交付件）——
# 现状（D9② 已落地，V32.3；夹具 = fnc_flag 体比较实参与 '已成交'，两份只差编码）：
#   call   GBK 源  + UTF-8 参数 → RETURN MATCH                                        rc=0
#   call   UTF-8 源 + UTF-8 参数 → RETURN MATCH                                       rc=0（与上一行逐字相同）
#   explain GBK 源 → 0\tassign\tp_o := '中文'，与 UTF-8 twin 同一行 IR                 rc=0
#   verify       GBK 包体 → PASS Entry + entries=1 checks=2 failed=0 skipped=1        rc=0
#   compile      GBK 源 → OK … routines=1  (non-utf8)                                 rc=0（体检面刻意不变）
#   call/explain/verify  两把判定都不成立（0xFF 0xFE）→ [ENV-0030] 读取 <path>:
#                stream is neither valid UTF-8 nor valid GBK                          rc=1
# 前两代形态（留作对照，均已不存在）：
#   改动前（V26.4）：GBK 源 + UTF-8 参数 → RETURN NO_MATCH rc=0 / UTF-8 源 → MATCH rc=0
#   D9①（V28.3）  ：GBK 源一律 [ENV-0030] … stream did not contain valid UTF-8 rc=1

# —— 交付件画像复现（codeweb 0.9.7，外部工具；本机会话 scratch 不入库）——
codeweb init && codeweb analyze            # 22.2 s / 43,432 节点 / 286,311 边 / 0 errors
codeweb stats                              # 对象构成（18,433 proc / 2,148 func / 9,315 table …）
codeweb nodes -s <关键字>                   # 定位节点全名（trace 需要带 schema/包的完整键）
codeweb trace proc:<schema>.<pkg>.<proc> --exact     # 单过程闭包
codeweb conflicts                          # 292 条 high（JSON，schema_version 1）

# —— 体量与编码口径（§4 数字来源；注意 wc 的分批 total 陷阱）——
find . -name '*.sql' | wc -l                                   # 文件 14,611
find . -name '*.sql' -print0 | xargs -0 wc -l \
  | awk '/[0-9]+ total$/ {for(i=1;i<=NF;i++) if($i=="total") s+=$(i-1)} END {print s}'   # 行数 5,628,370
python3 - <<'PY'   # 编码分解：utf8=11,462 / 需 gbk=3,149 / 两者都不通=0
import os
u=g=x=0
for dp,_,fns in os.walk('.'):
    for fn in fns:
        if not fn.lower().endswith('.sql'): continue
        b=open(os.path.join(dp,fn),'rb').read()
        try: b.decode('utf-8'); u+=1; continue
        except UnicodeDecodeError: pass
        try: b.decode('gbk'); g+=1
        except UnicodeDecodeError: x+=1
print(u,g,x)
PY

# —— `(+)` 形态统计口径（本文 §4 数字的来源，全部可复现）——
LC_ALL=C grep -roF '(+)' --include='*.sql' . | wc -l                    # 出现处 7,906
LC_ALL=C grep -rlF '(+)' --include='*.sql' . | wc -l                    # 命中文件 574
LC_ALL=C grep -rcF '(+)' --include='*.sql' . | awk -F: '{s+=$2} END {print s}'   # 命中行 7,695（grep -c 计行不计次）
LC_ALL=C grep -roE '=[[:space:]]*[A-Za-z0-9_.$]+[[:space:]]*\(\+\)' --include='*.sql' . | wc -l  # 右侧 4,876
LC_ALL=C grep -roE '\(\+\)[[:space:]]*=' --include='*.sql' . | wc -l                                  # 左侧 1,921
for d in 012_VIEW sequence sql 数据拆分PROGRAM--GAUSS; do               # 按目录分解 293/0/0/7,613
  printf '%s %s %s\n' "$d" "$(LC_ALL=C grep -roF '(+)' --include='*.sql' "$d" | wc -l)" \
                       "$(LC_ALL=C grep -rlF '(+)' --include='*.sql' "$d" | wc -l)"; done
```

## 附录 B 待确认的五个实例事实（A0b 之前不进入实现）

1. 实例版本与 `sslmode` 实际取值（`SELECT version()`、`SHOW ssl` / 服务端 `pg_hba.conf` 口径）。
2. `datcompatibility`（A / PG / ORA）——决定函数语义与 `norm()` 的 NULL 口径是否成立。
3. 目标表的真实行数与列宽（决定 A3 的批量与内存上界取值）。
4. 回写目标是否与源表同名同构（决定 D6 影子表 DDL 是新建还是复用）。
5. 服务端/库级编码（`client_encoding`、`SHOW server_encoding`）——交付件源码**混合编码**
   （UTF-8 11,462 / 需 GBK 3,149，§4），COPY OUT 字节编码若非 UTF-8，A3 装载路径直接乱码；
   与 §3.5 的源码侧编码问题是**两条独立链路**，都要实测，不能互相推断。
