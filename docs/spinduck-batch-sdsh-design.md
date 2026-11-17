# SpinDuck `batch` 子命令与 `.sdsh` 多指令同进程脚本设计文档（审校定稿版）

> **适用基线**：`c2j/SpinDuck`（单 crate `spinduck-core` + `[[bin]] spinduck`，DuckDB 1.5.5、`ogsql-parser v0.11.0`、`feature = "native" / "gauss" / "cli"`）  
> **设计目标**：新增 `spinduck batch <script.sdsh>` 子命令，在**同一个操作系统进程**内按序编排并执行多个 SpinDuck 指令（如 `compile` → `deploy` → `materialize` → 多条 `call` → `query` / `compare` → `sync`），消除跨进程冷启动与重复编译/开库开销，支持受控 `:memory:` 内存库跨指令共享与统一的预检/缓存失效/审计契约。

---

## 0. 审校意见闭环与修订摘要

结合对 `c2j/SpinDuck` 源码与 `AGENTS.md` 工程边界的多轮审校，本版文档已完成以下关键收敛：

1. **词法引号与变量展开去二义化**：放弃“对所有 `'...'` 保留外层单引号”的脆弱规则，采用标准引号剥离（`"..."` 剥引号且展开 `$VAR`；`'...'` 剥引号且原样禁展开）。`call` 中诸如 `0130000001` 的前导零串实参统一走 `"'0130000001'"` 或 `str:0130000001` 语法糖送入 `cli::args::parse_literal` 保真；未定义 `$VAR` 在 Phase 1 直接报 **`ENV-0017`** Fail-Fast（新码：`ENV-0014` 已被 GaussDB 连接环境变量占用、`ENV-0016` 兼表 `sync --slice` 运行期缺 env，同类失败不得两种码）。
2. **`--db` 继承的注入式实现**：`deploy`/`materialize`/`sync`/`query` 的 `--db` 是 `cli::args::parse` 必填旗标，原始 token 过 parse 必 `ENG-0010`。Phase 1 按行顺序维护 `current_default_db`（全局 `--db` → `use` 动态更新 → 行内显式优先），对缺 `--db` 的指令**在 parse 前注入 `--db <current>`**；无默认库时 `call` 保持匿名 `:memory:`（A4），其余四指令自然落 `ENG-0010`。
3. **`EngineCache` 键收敛与运行期开关剥离**：`EngineKey` 收敛为 `(DbKey, rules_hash, sorted_source_hashes)`（`SourceHash` = 源文件**内容哈希**，非路径），将 `batch_commit`、`set_oriented` 与 `step_budget` 移出静态编译键，每次 `call` 前经 `reset_for_batch_step()` 重置。命中时**跳过 `register`**（catalog 已含全部例程）并幂等重装本闭包 `MACRO`；跨计划/跨 `--file` 切换经无状态 `CompiledRoutineCache` 复用 IR、新建干净 `RoutineCatalog`。**V1 不做跨闭包 `DROP MACRO`**（理由见 §3.2.1-d：文件库宏持久化、deploy 视图可能引用、与多进程现状同风险面）。
4. **不池化 `GaussConn`（对齐仓内实测非目标）**：依据 `docs/materialize-copystream-design.md §1.1`（单表建连+开库仅 `0.2–0.6s`，明确决策“不做连接池/复用”），V1 对 `materialize` / `sync` 维持按步独立短连接，消除 `--continue-on-error` 或异常中断下 PG 线协议状态毒化风险。
5. **`:memory:` 隔离语义与 `DbKey` 路径归一化**：未设默认 `--db` 时，省略 `--db` 的 `call` 保持单步 CLI 一致行为（各自独立匿名 `:memory:`，不跨步共享）；唯有显式指定 `--db :memory:`（CLI / `use` / 行内）才共享 `DbKey::Memory`。文件库采用 `(canonicalize(parent_dir), file_name)` 归一化，规避建库前 `.duckdb` 尚未落盘导致 `canonicalize` 失败。
6. **Phase 1 产物追踪与 `ENV-0016` 零残留纪律**：追踪 `produced_artifacts` 避免 `query --out a.json` → `compare ref.json a.json` 被静态文件校验误杀；`TraceRun::open` 严格滞后于 `refuse_on_kernel_gaps`，缺件拒跑绝不创建空 `.log`。
7. **全子命令行为矩阵与 `AGENTS.md` 边界对齐**：补全 10 个允许子命令的读写库/失效/stdout 契约；将 `Engine` 扩展钩子收容于 `spinduck/src/engine/batch_hooks.rs`（`engine.rs` 非测试部分约 454 行、含测试 718 行已超 ≤700 上限，拆分诉求成立），并抽出 `*_with_executor` 内部入口实现 `cli` 与 `batch` 同源执行。

---

## 1. 背景与现行架构痛点（基于源码实证）

对照 `docs/runbook-real-gaussdb.md` 与 `spinduck/src/cli/` 当前实现，一条完整的端到端批处理流程通常需要串行拉起多个独立进程：

```bash
spinduck deploy .spinduck/plan/prc_a.toml --db run/local.duckdb
spinduck materialize --db run/local.duckdb --table t1,t2,t3 --jobs 8
spinduck call --db run/local.duckdb --plan .spinduck/plan/prc_a.toml '20250715'
spinduck call --db run/local.duckdb --plan .spinduck/plan/prc_b.toml '20250715'
spinduck query --db run/local.duckdb --sql-file q.sql --out run/out.json
spinduck sync --db run/local.duckdb --table dat_result --cols a,b --mode slice --slice dt=$BIZ_DATE
```

在多进程串联模式下，存在四项核心工程与性能瓶颈：

| 瓶颈维度 | 现行代码位置 | 问题根因与实测影响 |
| :--- | :--- | :--- |
| **1. DuckDB 重复开库与 `:memory:` 无法跨步共享** | `cli/deploy.rs`、`cli/run/io.rs`、`cli/run/call.rs`、`cli/query.rs` | 每个子命令独立调用 `DuckExecutor::open(path)`，每次经历文件锁获取、WAL 回放与关闭刷盘；同时无法在纯内存库（`:memory:`）上完成 `deploy` → `materialize` → 多轮 `call` → `query`。 |
| **2. 多 entry `call` 重复词法/语法编译源码** | `cli/run/call.rs:58-72`、`engine.rs:72-94` | 连续调用多个入口（或同入口多组实参）时，每次重读规则、`read_source_strict`、`Tokenizer`/`Parser` 编译 `IrProg`，并从冷 `prepare_cached(512)` 起步。 |
| **3. `LookupExecutor` 维表索引无法跨只读 `call` 复用** | `engine.rs:348`（`self.lookup.clear()`） | 每次顶层 `call` 入口无条件清空 `LookupState` 与 `MemoCache`，V42 中 620k 行级参数表索引在同批次第二条 entry 时被迫全量重建。 |
| **4. 预检割裂与 `--trace` 路径管理繁琐** | `cli/run/call.rs`、`cli/trace.rs`（`ENV-0016`） | 第 4 步参数写错时前 3 步已执行落盘；多个 `call` 若未逐行手动拼接不同路径则触发 `ENV-0016` 拒绝覆盖。 |

---

## 2. CLI 接口与 `.sdsh` 脚本语言规范

### 2.1 CLI 命令签名与全局选项

遵循 `spinduck/src/cli/args/` 现有零依赖手写解析器契约（用法错返回 `ENG-0010`、退出码 `2`）：

```text
spinduck batch [--db <f>] [--continue-on-error] [--dry-run] [--trace-dir <dir>]
               [--keep-lookup-cache] <script.sdsh>
```

| 参数 / 旗标 | 语义说明 |
| :--- | :--- |
| `<script.sdsh>` | 必填位置参数（仅允许 1 个脚本文件，多给或重复旗标报 `ENG-0010`）。通过 `cli::read_source_strict` 读取（UTF-8 → GBK 双判，`ENV-0010` / `ENV-0030`）。 |
| `--db <f>` | 可选默认库路径（文件路径或 `:memory:`）。Phase 1 按行顺序维护 `current_default_db`：初值 = 全局 `--db`；`use --db` 指令更新之；行内显式 `--db` 优先级最高。对缺 `--db` 的 `deploy`/`materialize`/`sync`/`query`/`call`，**在交给 `cli::args::parse` 前注入 `--db <current_default_db>`**（这四个子命令的 `--db` 在解析器中是必填旗标，不注入必 `ENG-0010`）。**若全局与 `use` 均未设默认库**：`deploy`/`materialize`/`sync`/`query` 省略 `--db` 在 Phase 1 报 `ENG-0010`；`call` 省略 `--db` 不注入，保持单步 CLI 一致语义（各自独立新建匿名 `:memory:`，不跨步共享）。 |
| `--continue-on-error` | 默认关闭（Fail-Fast：任一步返回非 0 退出码或 `Err` 立即中止批次，退出码 `1`）；开启后继续执行后续指令（仍强制刷新脏缓存屏障），结束时汇总失败数并返回 `1`。 |
| `--dry-run` | 仅执行 **Phase 1 静态预检**（词法/变量展开、逐行旗标与互斥校验、禁用嵌套 `batch`、feature 门禁、显式 `--trace` 冲突与产出追踪静态核对），**不打开 DuckDB / GaussDB，也不创建任何日志文件**；通过即返回 `0`。 |
| `--trace-dir <dir>` | 为脚本中未显式指定 `--trace` 的 `call` 指令自动分配无冲突审计文件路径：`<dir>/step-<k:02>-<routine>.log`（及 `.sql` sidecar）。遵守 `cli/trace.rs` 纪律：Phase 1 校验目录存在可写与目标不冲突，**实际文件仅在运行时过完 `refuse_on_kernel_gaps` 后才以 `create_new` 创建**（缺件拒跑绝不留空日志）。 |
| `--keep-lookup-cache` | 允许同一 `Engine` 在连续 `call` 之间保留 `LookupState` 维表点查索引；遇 `deploy` / `materialize` / `query` / `sync` 或任何写库失败时自动全量失效。 |

### 2.2 `.sdsh`（SpinDuck Shell）语法与词法规则

`.sdsh` 不是通用 Bash 子集，而是**封闭、确定性、可静态审计的 SpinDuck 指令批处理脚本**：

1. **注释与空白行**：
   - 空行忽略；行首或词边界外以 `#` 或 `--` 引导至行尾的内容为注释。
2. **换行续行（`\`）**：
   - 行尾（非单/双引号内、且前方反斜杠为奇数个）紧跟 `\` 并换行时，与下一物理行拼接为同一条逻辑指令；报错行号报告该逻辑指令的**起始物理行号**。
3. **标准引号剥离与 `call` 字符串字面量明确口径**：
   - **双引号 `"..."`**：剥除两侧 `"`；内部支持 `\"`、`\\`、`\$` 转义，并展开 `$VAR` / `${VAR}`（适用于含空格路径、SQL `--where "bcrq = '20240105'"` 或 `"${PLAN_DIR}/p.toml"`）。
   - **单引号 `'...'`**：剥除两侧 `'`；内部**完全原样保留**，不展开 `$VAR`、不解释转义（如 `sync --slice 'bcrq=$ENV'` 或 `--where 'status = "OK"'` 绝对安全）。若需在单引号内表示字面单引号，与 Shell 一致在单引号外拼接 `\'`。
   - **与 `cli::args::parse_literal` 的协作（前导零实参保真）**：
     - 剥去外层引号后，`parse_literal("20250715")` 得出 `SpinValue::Int(20250715)`，在 `frame::install_params` 下当声明形参为 `VARCHAR` 且 `[implicit].assignment_loose = true`（默认）时可宽松转成 `"20250715"`；但若实参为 **`0130000001`**（前导零）或 `assignment_loose = false`，直接裸写会被转成 `Int(13000001)` 而丢失前导零。
     - 因此在 `.sdsh` 中传纯数字外形的强类型字符串给 `call`，采用以下两种显式写法之一：
       1. 双层引号透传：`"'0130000001'"` 或 `\'0130000001\'`（与在 Bash 中调用 `spinduck call` 100% 字节同形，`parse_literal("'0130000001'")` → `SpinValue::Str("0130000001")`）；
       2. `.sdsh` `call` 显式类型糖：`str:0130000001`（由 `batch` 解析器标准化为 `'0130000001'` 后写入 `Command::Call.args`）。
4. **批内变量（`set`）、库切换（`use`）与未定义变量 Fail-Fast**：
   - `set <KEY> = <VALUE>`（或 `set <KEY> <VALUE>`）：按逻辑行从上到下顺序求值并写入脚本局部 `vars`（`=` 为可选分隔符；`<VALUE>` 取其后全部 token、逐个走引号剥离与变量展开后拼接，裸词多 token 以单空格连接；绝不调用 `std::env::set_var` 污染进程环境）。
   - **未定义变量纪律**：在双引号或裸词中出现 `$KEY` / `${KEY}` 时，依次查 `vars` → 只读进程 `std::env::var(KEY)`；**两者皆无时直接在 Phase 1 报 `ENV-0017` Fail-Fast 中止**（新码：`ENV-0014` 已被 GaussDB 连接环境变量占用、`ENV-0016` 兼表 `sync --slice` 运行期缺 env，同类失败不得两种码），绝不静默保留 `$KEY` 字面量污染下游 SQL 或过程参数。若确需把 `$ENV` 字面传给 `sync --slice` 在运行期解析（或测试缺 env），使用单引号 `'bcrq=$ENV'` 或 `\$ENV`——此时缺 env 由 `io/sync.rs` 以 `ENV-0016` 在运行期拒绝，属既有契约不变。
   - `use --db <path|:memory:>`：切换后续省略 `--db` 指令的默认会话库。
5. **允许子命令全集与执行属性矩阵（禁止 `batch` / `help` 及 Shell 算符）**：

| 子命令 | DuckDB 访问 | GaussDB 访问 | 写库 / 清缓 | 静态产物登记（供下游预检） | 说明 |
| :--- | :---: | :---: | :---: | :---: | :--- |
| `compile` | 无（`Noop`） | 无 | — | 项目态 `--dir`/`--entry` 产出 `<项目根>/.spinduck/plan/<entry>.toml`（逐条登记实际路径） | 项目态编译产出计划后，后续 `deploy`/`call --plan` 放行（`parse_call` `F6` 对尚不存在的计划自动跳过 target 预检，读不到时由执行层 `ENV-0010` 拒） |
| `deploy` | 打开 / `try_clone` | 无 | **写库 + 登记宏/目录** | — | 非幂等（`runbook §6`）；登记本计划同闭包 `EngineCache` |
| `materialize` | 打开 / `try_clone` | 按步独立建连（`--jobs` 并发池） | **写库（含 `ENV-0020` 失败）** | — | 无论成败均清除同库 `memo` / `lookup` 并置 `schema_dirty` |
| `call` | 同闭包复用 / `try_clone` / 匿名 `:memory:` | 无 | **写库（DML/DDL）** | `--trace` 文件（仅 `kernel_gaps` 通过后落盘） | 运行期按 `step_budget`/`batch_commit`/`set_oriented` 重置后执行 |
| `query` | 打开 / `try_clone` | 无 | **视作潜在写库**（支持 `DELETE`/`CREATE`） | `--out <F.json>` | 无论查询只读还是清表写库，安全起见置 `schema_dirty` 并失效同库 `lookup`/`memo` |
| `compare` | 无 | 无 | — | — | 若消费前序 `query --out` 文件，由 `produced_artifacts` 放行 |
| `sync` | 打开 / `try_clone`（读本地） | 按步独立短连接写入 Gauss | 只读本地库（不改本地结构） | — | 绝不跨步复用 `GaussConn`，防异常事务/COPY 毒化 |
| `explain` | 无（`Noop`） | 无 | — | — | 静态解析并输出 IR 文本，不写库 |
| `rules probe` | 无 | 无 | — | — | 版本不匹配返回 `EXIT_FAIL`，按 Fail-Fast / `--continue-on-error` 处理 |
| `version` | 无 | 无 | — | — | 打印 `spinduck <version>` |
| `batch` / `help` / `\|` / `>` / `;` / `&&` | **禁止** | — | — | — | Phase 1 直接报 `ENG-0010`（退出码 `2`）拒绝 |

6. **词法判定细则（消除歧义条款）**：
   - **Shell 算符判定在 token 层**：`|`/`>`/`>>`/`&&`/`;`/`&` 仅当以**裸词 token**出现时判违规；引号剥离后的 token 内容（如 `--where "a;b"`、`--slice 'x|y'`）不参与判定。
   - **`str:` 类型糖作用域**：只作用于 `call` 的位置实参——Routine 形态为 routine 名之后的实参，Plan 形态为全部位置参数；**不作用于旗标值**（`--where str:x` 原样透传）与其他子命令。`str:` 后内容按字面处理（可含空格时须整体加引号：`"str:a b"` → `Str("a b")`）。
   - **`produced_artifacts` 登记口径**：`compile` 项目态逐条登记实际落盘计划路径；`query --out` 登记输出路径（同一路径重复产出允许，后者覆盖前者，与 Shell `>` 同语义）；`compare`/`rules probe`/`explain`/`version`/`deploy`/`materialize`/`sync`/`call` 无静态文件产物。下游消费检查 = `路径已存在于磁盘` ∨ `路径 ∈ produced_artifacts`，否则 `ENG-0010`。
   - **`--trace` 冲突检查仅对 `call` 生效**：`explain --trace` 的 `--trace` 是输入（读既有日志做命中注记），不参与"目标已存在 → ENV-0016"判定；`--trace-dir` 也只为 `call` 分配。
   - **trace 事件可区分性**：`TraceRun::open` 的 `sid = pid+epoch_millis`、`cid` 恒为 `call_id(1)`，同进程相邻两步同毫秒打开会产生相同 `(sid,cid)`。batch 为每个 `call` 步的 `proc` 字段追加步序号前缀（如 `step03:prc_x`），保证同名例程的审计事件可按步区分。

### 2.3 典型 `.sdsh` 脚本示例

```sdsh
# 交易批结算端到端批处理脚本：daily_purchase.sdsh
set BIZ_DATE = 20250715
set PLAN_DIR = .spinduck/plan

# 1. 部署表结构、视图与例程清单（未建库前先核对缺件）
deploy "${PLAN_DIR}/prc_trd_deal_purchase.toml" --db run/local.duckdb

# 2. 从 GaussDB 并行物化所需输入表
materialize --db run/local.duckdb \
  --table dat_trd_purchase_src,par_fund_partner,par_sys_purchase,par_fund_seat_para,swh_all_kind \
  --jobs 8

# 3. 同进程连续调用多个 entry（前导零串显式用 str: 或 "'...'" 保真；prc_trd_deal_purchase 直击步骤1 EngineCache）
call --db run/local.duckdb --plan "${PLAN_DIR}/prc_trd_deal_purchase.toml" --batch-commit --step-budget 200000 "str:${BIZ_DATE}" str:0130000001
call --db run/local.duckdb --plan "${PLAN_DIR}/prc_trd_hz_byfund.toml" --batch-commit --step-budget 200000 "'${BIZ_DATE}'"

# 4. 本地快照校验与回写 GaussDB（slice 过滤可写 "${BIZ_DATE}" 或透传 'bcrq=$BIZ_DATE'）
query --db run/local.duckdb --sql-file cases/queries/check_repurchase.sql --out run/local.json
compare cases/baselines/expected.json run/local.json --key purchase_no
sync --db run/local.duckdb --table dat_trd_repurchase --cols purchase_no,fund_code,yj,bcrq --mode slice --slice "bcrq=${BIZ_DATE}"
```

---

## 3. 同进程资源共享与核心架构设计

严格遵守 `AGENTS.md §1 / §3` 工程约束：
- **所有编排、词法预检与会话复用归属 `spinduck-core` 新模块 `spinduck/src/batch/`**（单文件严格 ≤ 500 行：`mod.rs`、`parser.rs`、`preflight.rs`、`session.rs`、`runner.rs`）；
- **`spinduck/src/cli/args/batch.rs` 与 `spinduck/src/cli/run/batch.rs` 仅作薄壳**，接入 `cli/args/mod.rs` 与 `cli/run/mod.rs`；
- **feature 归属**：`Command::Batch` 与 `cli::args::parse` 分支不设 feature 门（`cli` 模块在 lib 内无条件编译），`batch/` 编排核心同样不依赖 `native`——Phase 1 预检在任何 feature 组合下可运行并对含 `materialize`/`sync`（需 `gauss`）或执行类指令（需 `native`）的脚本给出 `ENV-0012`；runner 执行面各分支沿用对应子命令现有的 `#[cfg(feature)]` 门控；
- **零破坏既有接缝**：`SqlExecutor`、`TraceSink`、`Engine::call`、`Engine::call_with_sink` 签名与原有行为**逐字节保持不变**；扩展能力放在新建子模块 `spinduck/src/engine/batch_hooks.rs`（同 `engine/set_oriented.rs` 布局）。`engine.rs` 实测非测试部分约 454 行、含 `#[cfg(test)]` 共 **718 行**——已超 AGENTS §3「含测试总行 ≤700」上限，拆分子模块的诉求成立。DuckDB API 仍**仅限**出现在 `spinduck/src/sql/exec_native.rs`（新增 `DuckExecutor::try_clone(&self)`，并抽内部 `*_with_executor` 供 `cli` 与 `batch` 共用）。

### 3.1 `BatchSession` 分层复用模型

```text
spinduck batch script.sdsh
  │
  ├── Phase 1: parse_sdsh() + preflight_check()            [零开库、零创日志、Fail-Fast]
  │     ├── cli::read_source_strict (UTF-8 / GBK)
  │     ├── 引号剥离 + 线性 set/env 插值（未定义即 ENV-0017）+ produced_artifacts 追踪
  │     ├── current_default_db 顺序模拟 + 缺 --db 指令注入，再复用 cli::args::parse 逐行校验（ENG-0010）
  │     └── 检查禁用嵌套 batch / feature 门禁(ENV-0012) / call 的 --trace 冲突与目录存在性
  │
  └── Phase 2: BatchSession (单进程生命周期管理；Drop 逆序释放 Engine -> DbPool)
        ├── BatchDbPool (BTreeMap<DbKey, DuckExecutor>)    [canonical 连接工厂]
        │     ├── DbKey::Memory（显式 :memory:）/ DbKey::File(canonical_parent + file_name)
        │     ├── 首次访问 open / open_in_memory；池持有 canonical DuckExecutor
        │     ├── EngineCache 各条目经 try_clone() 自持独立连接（同 DB instance，独立 prepared cache）
        │     └── 非引擎步骤（query/sync/materialize/deploy-ddl）与匿名 call 用池连接或独立 open
        │
        ├── EngineCache (BTreeMap<EngineKey, CachedEngine>)          [同闭包精确复用]
        │     ├── EngineKey = (DbKey, rules_hash, sorted_source_hashes)
        │     │     rules_hash = 生效 RuleRegistry 内容哈希（种子 < 项目 rules_files < --rules overlay）
        │     │     SourceHash = 例程源文件内容哈希（deploy=deployment.routines；call --plan=deployment.routines+--file；Routine 形态=--file 列表）
        │     ├── 命中：跳过 register（catalog 已含全部例程），reset_for_batch_step(...) 后 refresh_sql_macros() 幂等重装本闭包 MACRO
        │     └── 未命中：经 CompiledRoutineCache 建干净 RoutineCatalog，install_sql_macro 正常登记
        │
        └── CompiledRoutineCache (定义于 engine/batch_hooks.rs；HashMap<(SourceHash, RulesHash, DownshiftSetHash), Vec<CompiledRoutine>>)
              ├── 跨不同 --plan / 不同 --file 复用已编译 IR，免重复 Tokenizer/Parser CPU 开销
              ├── 仅在 register_named 成功（含静默丢弃护栏通过）后写入；命中即直接 catalog.register + install_sql_macro
              └── 新建干净 RoutineCatalog；V1 不做跨闭包 DROP MACRO（残留风险见 §3.2.1-d）
```

> **关于 `GaussConn` 的明确非目标**：依据 `docs/materialize-copystream-design.md §1.1` 实测（单表建连+开库固定开销仅 `0.2–0.6s`，相对于数据流传输可忽略），`batch` **不在 V1 中跨指令复用 `GaussConn` TCP 会话**。每次 `materialize` / `sync` 独立 `open_gauss()` 并在步骤结束时关闭连接，彻底避免异常中断或 `--continue-on-error` 下将处于 `Aborted` 事务/半截 COPY 状态的 PG 线协议连接带入后续步骤。

### 3.2 关键边界与一致性护栏

1. **`EngineKey` 定义、命中语义与 `MACRO` 生命周期**：
   - **键的三个分量**：
     - `DbKey`：`Memory`（显式 `:memory:`）或 `File(canonicalize(parent), file_name)`；
     - `rules_hash`：**生效 `RuleRegistry` 的内容哈希**（内置种子 < 项目 `rules_files` < `--rules` overlay 全量叠加后的结果），而非文件路径——同一路径内容变更必须击穿缓存；
     - `sorted_source_hashes`：例程**源文件内容哈希**的有序集合。取数口径与 `call_target_and_sources`（`cli/run/call.rs:189-235`）逐字对齐：`deploy` = `deployment.routines`（DDL/视图不进 catalog，不计入）；`call --plan` = `deployment.routines` + `--file` 追加；`call` Routine 形态 = `--file` 列表（可为空集）。`deploy plan.toml` 与无 `--file` 的 `call --plan plan.toml` 源集相同 ⇒ 同键 ⇒ 命中成立。
   - **命中语义（零重编译的完整定义）**：`EngineCache` 命中时 **跳过 `register`/`register_named` 整个编译路径**——catalog 在首次写入时已含全部例程，源内容哈希相同 ⇒ 注册结果确定相同，`register_named` 的静默丢弃护栏（`engine.rs:84-92`）已在首次注册时通过、无需重放。随后 `reset_for_batch_step(step_budget, batch_commit, set_oriented)` 重置运行态字段，并 `refresh_sql_macros()` 对 catalog 内全部例程幂等重跑 `install_sql_macro`（`CREATE OR REPLACE`，修复见 d）。**编译开销的真正节省在未命中路径**：新 Engine 构建时逐源文件查 `CompiledRoutineCache`，命中则直接 `catalog.register` + `install_sql_macro`，跳过 Tokenizer/Parser/IR 编译。
   - **`CompiledRoutineCache` 接入点**：类型定义于 `engine/batch_hooks.rs`（依赖方向 engine ← batch，不得反向）。键 = `(SourceHash, RulesHash, DownshiftSetHash)`；`DownshiftSetHash` = 编译该源文件**前** `downshift_names()` 集合的哈希（新 catalog 按确定顺序逐文件注册，注册前快照可预计算）。**只在 `register_named` 成功后写入**（护栏失败的源不进缓存）；命中条目直接 `catalog.register` + `install_sql_macro`，绕过护栏是安全的——缓存只可能由已通过护栏的注册产生。
   - **a) `deploy` 与 `call` 的规则一致性**：`deploy` 用 `project.rules_files`（无 `--rules`）、`batch_commit=false, set_oriented=false` 构建 Engine 并登记进 `EngineCache`；后续 `call --plan` 若携带 `--rules`/`--batch-commit`/`set_oriented=true`，`rules_hash` 不同或运行态字段不同——前者自然 miss 重建（仍可命中 `CompiledRoutineCache` 共有部分），后者因运行态不入键而**命中同一 Engine** 再经 `reset_for_batch_step` 覆盖字段，与单步 CLI 语义逐字一致。
   - **b) 非幂等边界**：同一文件库重复 `deploy` 仍由 DuckDB 抛目标表已存在错误，遵从 Fail-Fast / `--continue-on-error`（`runbook §6`）。
   - **c) 连接所有权**：`BatchDbPool` 每 `DbKey` 持一条 canonical `DuckExecutor`（首访 `open`/`open_in_memory`）；`EngineCache` 每条目经 `DuckExecutor::try_clone()` 自持独立 `duckdb::Connection`（同 DB instance、独立 `prepare_cached(512)`——命中同闭包复用的是**该 Engine 自己那条**连接的缓存，跨 Engine 不共享，符合预期）；非引擎步骤（`query`/`sync`/`materialize` 单表路径/deploy 的 DDL 执行）经 `*_with_executor` 使用池 canonical 连接或其 `try_clone`（`materialize_multi` 已有多连接先例，`io.rs:159-163`）。DuckDB 同 instance 多连接由 MVCC 处理，batch 串行执行无并发写冲突；文件库第二条 `Connection::open` 会撞文件锁，故**同库绝不二次 `open`，一律 `try_clone`**。
   - **d) `MACRO` 生命周期决策（V1 不做跨闭包 `DROP`）**：DuckDB 非 `TEMP` `CREATE MACRO` 是**数据库编目对象**——同 instance 所有连接可见、文件库持久化到 `.duckdb`。跨闭包 `DROP MACRO` 有三个不可接受的代价：① `EngineCache` 命中后宏已被删，`catalog`/`downshift_names()` 仍假定其存在 ⇒ 需在每次命中重装（成本虽低但状态机复杂）；② deploy 安装的**视图**可能引用宏函数（视图 SQL 在查询期绑定），删宏会打破已落盘视图；③ 与"多进程逐次 `call`"现状风险面相同（文件库宏本来就跨进程存活），batch 并未恶化。**V1 决策**：宏命名空间视为 DB 级共享资源、不做跨闭包清理；`EngineCache` 命中时 `refresh_sql_macros()` 幂等重装兜底外部删除。残留风险（plan_a 宏名与 plan_b 未宏化同名函数冲突致静默错答）登记为**已知边界**，由 `should_reuse_engine_after_deploy...` 的跨计划用例锁定当前行为；schema 限定/`TEMP` 宏/命名空间化列为 V2 候选。
2. **跨连接写库后 `prepare_cached(512)` 与 `LookupState` 失效屏障**：
   - 新增 `spinduck/src/engine/batch_hooks.rs`，提供 `Engine::call_with_options(&mut self, name, args, sink, CallOptions { preserve_lookup: bool })`、`Engine::invalidate_caches(&mut self)` 与 `Engine::reset_for_batch_step(step_budget, batch_commit, set_oriented)`；原有 `Engine::call` / `call_with_sink` 原样委托给 `CallOptions { preserve_lookup: false }`。
   - 仅当 `--keep-lookup-cache` 开启、命中同一 `Engine` 且中间无写库指令时，`preserve_lookup = true`（过程内自身 DML 写入表已由 `LookupExecutor::invalidate` 按表精确失效）。
   - 一旦执行 `deploy`、`materialize`、`query` 或其他写库操作，**无论成功还是在 `--continue-on-error` 下失败**（如 `materialize` 在 `COPY FROM` 后报 `ENV-0020` 已部分入库），`BatchSession` 一律立即调用 `invalidate_db_caches(&db_key)`：清空关联 `Engine` 的 `memo` + `lookup`、置 `schema_dirty = true` 强制下一次 `call` 重新采集 `verify::kernel_objects`。**不重建连接句柄**：DuckDB 对 catalog 变更已有 execute 期 rebind 兜底（`exec_native.rs:139-148`），`prepare_cached` 无需刷新；`Engine.ex` 为 `Box<dyn SqlExecutor>` 独占包装栈，不提供换连接 API，也不为此新增。
3. **`:memory:` 下的 `materialize` 临时目录与 `DbKey` 规范化**：
   - 文件库 `DbKey::File` 使用 `(canonicalize(parent_dir), file_name)`（规避建库前库文件尚不存在导致 `canonicalize` 失败）；`:memory:` 统一归一化为 `DbKey::Memory`。
   - 对 `:memory:` 库执行 `materialize` 时，`materialize_workdir(Path::new(":memory:"))` 返回 `"."`（CWD，对齐 `cli/run/io.rs:95` 现行实现），临时 `.spinduck-mat-*.csv` 在加载后自动清理，仅在失败时保留现场供排查。

---

## 4. 两阶段执行契约与错误码矩阵

### 4.1 输出渲染与退出码契约

- **退出码**：
  - `0`（`EXIT_OK`）：全部指令成功（或 `--dry-run` 预检通过）；
  - `1`（`EXIT_FAIL`）：预检触发 `ENV-*`（如缺失脚本、`ENV-0012` feature 缺失、`ENV-0017` 未定义变量、`ENV-0016` 冲突），或运行期任一步返回非 0 退出码 / `Err`（包括 `--continue-on-error` 下哪怕仅有 1 步失败）；
  - `2`（`EXIT_USAGE`）：`batch` 自身用法错、`.sdsh` 词法错误（引号未闭合/末尾孤立 `\`）、禁用 `batch`/`help`/Shell 管道、行内子命令参数错误（`ENG-0010`）。
- **Stdout / Stderr 契约（对齐 `tests/cli/usage.rs`）**：
  - 所有正常输出、`ENG-0010` 诊断、步骤头 `[k/N] >>> <cmd>`、子命令原有输出行（含 `SET_ORIENTED ...`、`MISS ...`、`OUT ...` / `RETURN ...`、`MATERIALIZE ...` / `SYNC ...` / `compare ...`）、`--continue-on-error` 下的失败行 `[k/N] FAIL <cmd> [CODE] ...` 与末尾 `batch: steps=N ok=X failed=Y elapsed_ms=Z` **全部经由 `out: &mut dyn Write` 输出至 stdout**，`stderr` 在正常路径保持绝对为空（仅当 `materialize` 快路径降级至 `INSERT` 时沿用 `cli/run/io.rs::warn_degraded` 写 `stderr` 告警）。
  - Phase 1 若发生错字或预检拦截，打印诊断后立即退出（不建 `.duckdb`、不建 `--trace` 日志文件）。

### 4.2 新增与复用错误码对照

| 场景 | ErrorCode | FaultDomain | 退出码 | 触发阶段 |
| :--- | :--- | :--- | :---: | :--- |
| 未闭合引号 / 行尾孤立 `\` / 未知指令 / 嵌套 `batch` / `help` / Shell 管道 / 行内子命令错 / 未设默认库时 `deploy` 等缺 `--db` | `ENG-0010` | `Engine` | `2` | Phase 1 预检 |
| `.sdsh` 文件不可读或非 UTF-8/GBK 编码 | `ENV-0010` / `ENV-0030` | `Env` | `1` | Phase 1 读取 |
| 含 `materialize`/`sync` 但未启用 `gauss`，或含执行命令未启用 `native` | `ENV-0012` | `Env` | `1` | Phase 1 预检 |
| 脚本中引用了 `vars` 与进程环境变量均未定义的 `$VAR`（**新码 `ENV-0017`**；`ENV-0014` 已被 GaussDB 连接环境占用、`ENV-0016` 兼表 `sync --slice` 运行期缺 env，均不复用） | `ENV-0017` | `Env` | `1` | Phase 1 插值 |
| 多条 `call` 指定相同 `--trace` 路径、目标文件已存在、或 `--trace-dir` 目录不可写 | `ENV-0016` / `ENV-0012` | `Env` | `1` | Phase 1 预检 |
| 某条指令运行期返回 `EXIT_FAIL`（如 `MISS`、`compare` DIFF、`rules probe` 不匹配）或抛出业务/内核错 | 原错误码（如 `BIZ-0150` / `KERNEL-0010` / `ENV-0020`） | 原域 | `1` | Phase 2 执行 |

---

## 5. TDD 落地拆分与验证矩阵（遵循 `AGENTS.md §1 / §2 / §3`）

### 5.1 文件改动清单（严格单文件 ≤ 500 行）

1. `spinduck/src/sql/exec_native.rs`：新增 `DuckExecutor::try_clone(&self) -> Result<Self, EngineError>`（保持 `SqlExecutor` trait 不变，DuckDB API 仍严格隔离在此文件）。
2. `spinduck/src/engine/batch_hooks.rs`（新建子模块；`engine.rs` 实测非测试约 454 行、含测试 718 行已超 ≤700 上限，拆分诉求成立）：定义 `CallOptions`、`CompiledRoutineCache`，实现 `Engine::call_with_options`、`Engine::invalidate_caches`、`Engine::reset_for_batch_step`、`Engine::macro_names()`、`Engine::refresh_sql_macros()`（幂等重跑 `install_sql_macro`）；`engine.rs` 的 `call_with_sink` 薄委托至 `call_with_options(..., CallOptions::default())`。
3. `spinduck/src/cli/deploy.rs`、`cli/query.rs`、`cli/run/{call,io}.rs`：各自拆出接收 `&DuckExecutor` 的内部 `*_with_executor` 函数，原有 `*_cmd` 仅负责开库后调用该内部函数，彻底保证单步 CLI 与 `batch` 执行口径 100% 同源。
4. `spinduck/src/batch/{mod,parser,preflight,session,runner}.rs`（`spinduck-core` 新模块）：实现 `.sdsh` 词法剥引号、`current_default_db` 顺序模拟与缺 `--db` 注入、前向变量求值（未定义即 `ENV-0017`）、`produced_artifacts` 跟踪预检、`BatchDbPool` / `EngineCache` 与 `runner`（`CompiledRoutineCache` 类型本体在 `engine/batch_hooks.rs`，batch 只持有实例）。
5. `spinduck/src/cli/args/batch.rs` & `mod.rs`：新增 `Command::Batch` 并更新 `usage()`（同步复核 `tests/cli/usage.rs` 全部断言）。
6. `spinduck/src/cli/run/batch.rs` & `mod.rs`：薄壳接入 `batch_cmd`。
7. `spinduck/tests/cli/batch.rs`（接入 `tests/cli.rs`）：端到端契约测试。

### 5.2 核心回归测试清单

| 测试函数名 | 验证目标 |
| :--- | :--- |
| `should_strip_quotes_and_reject_undefined_var_in_sdsh_parser` | `"..."` 剥引号并展开 `${VAR}`；`'...'` 剥引号且原样禁展开（保留 `'k=$ENV'`）；`str:0130000001` 与 `"'0130000001'"` 均产生 `SpinValue::Str("0130000001")`；未定义 `$NO_SUCH` 在 Phase 1 报 `ENV-0017`；`--where "a;b"` 不误杀 Shell 算符 |
| `should_reject_nested_batch_and_duplicate_trace_before_opening_db` | Phase 1 拦截嵌套 `batch`（`ENG-0010`）、`help`（`ENG-0010`）、重复 `--trace`（`ENV-0016`）、未设默认库时 `deploy` 缺 `--db`（`ENG-0010`），断言磁盘未创建 `.duckdb` 与 `.log` |
| `should_track_produced_artifacts_and_not_precreate_trace_on_kernel_gap` | `query --out a.json` 后紧跟 `compare ref.json a.json` 通过 Phase 1；当 `call` 触发 `kernel_gaps` 拒跑时 `--trace-dir` 下不留空 `.log` 文件 |
| `should_reuse_engine_after_deploy_with_different_batch_commit_and_reset_budget` | `deploy p.toml` 后紧接 `call --plan p.toml --batch-commit --step-budget 200000` 直击 `EngineCache`（跳过 register、`refresh_sql_macros` 重装）；后续无预算 `call` 重置 `step_budget`；`plan_a`→`plan_b`→`plan_a` 交替执行宏共存不误伤、命中缓存后宏仍可用（锁定 §3.2.1-d 已知边界） |
| `should_inject_default_db_before_args_parse_and_honor_use` | 全局 `--db` / `use --db` / 行内 `--db` 三层优先级；`use` 切换后续指令默认库；无默认库时 `deploy` 缺 `--db` 报 `ENG-0010`、`call` 走匿名 `:memory:` |
| `should_continue_on_error_and_summarize_exit_code` | `--continue-on-error` 下中间步骤失败仍执行后续指令、末尾 `batch: steps=N ok=X failed=Y` 且退出码 1；默认 Fail-Fast 首个失败即中止 |
| `should_dry_run_pass_with_zero_side_effects` | `--dry-run` 预检通过返回 0，断言磁盘无 `.duckdb`、无 `.log`、无 `--out` 文件 |
| `should_isolate_routine_form_call_closure_without_plan` | `call --file a.sql --file b.sql r` 的 EngineKey 由 `--file` 内容哈希构成；同文件改内容后缓存击穿重建 |
| `should_share_in_memory_db_only_when_explicitly_selected` | `batch --db :memory:` 跨 `deploy`→`call`→`query` 共享内存库；无 `--db` 时两条 `call` 各自隔离匿名内存库 |
| `should_invalidate_lookup_cache_after_write_even_on_failure` | `--keep-lookup-cache` 下连续 `call` 暖命中；穿插 `materialize` 或 `--continue-on-error` 下写库步骤失败后强制失效 `LookupState` |
| `should_reject_gauss_commands_upfront_when_compiled_without_gauss` | 未开 `gauss` feature 运行含 `materialize`/`sync` 的 `.sdsh` 时，Phase 1 直接以 `ENV-0012` 拒绝且不执行前序步骤 |
