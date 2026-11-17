# Runbook：在真 GaussDB 实例上完整跑一个案例

> 适用对象：拿到一份语料级项目（`golden/case_*` 同构的目录）后，要在**真实例**
> （生产/准生产 GaussDB，或本仓 `golden/gaussdb-docker` 的 openGauss 5.0.0 容器）上
> 完整执行"参照侧采集 → 本地执行 → 逐值裁决"全流程的工程师。
>
> 本文件是**操作手册**，不是设计文档：机制见 `docs/architecture.md` 与
> `docs/design-production-gaussdb.md`，实证登记见 `docs/validation.md`。

---

## 0. 现状边界（先读，避免误期待）

截至本文件撰写时（PR #8 后的 `feat/a2-a5-local-half`），**CLI 能自动化的部分**与
**仍需手工的部分**如下：

| 环节 | 自动化程度 | 载体 |
|---|---|---|
| 真 GaussDB 连接 | ✅ CLI | `materialize` / `sync`（`--features gauss`） |
| 本地执行（部署→调用→查证） | ✅ CLI | `deploy` / `call` / `query` / `compare` |
| 参照侧行结果采集 | ⚠️ **手工** | 用 `gsql`/任意 SQL 客户端在实例上跑，结果誊成 `cases/baselines/expected.json` |
| 两侧自动比对（黄金层） | ✅ 测试层 | `cargo test --features gauss`（对四个 golden 用例自动跑两侧） |
| 多步编排（deploy→物化→call→query→sync） | ✅ CLI | `batch <script.sdsh>`（`.sdsh` 脚本，同进程顺序执行，见 §5） |
| `run <plan>` 一条龙编排 | ❌ 未落地 | 属 design A5/A6 待实例项，见 §7 |

口令与连接信息**只走环境变量**（`SPINDUCK_OG_*`），不进 argv、不进计划文件、不入库
（architecture §10.4）。今天代码实际读取的变量只有五个：

```
SPINDUCK_OG_HOST      必填
SPINDUCK_OG_PORT      必填（数字）
SPINDUCK_OG_PASSWORD  必填
SPINDUCK_OG_USER      缺省 gaussdb
SPINDUCK_OG_DB        缺省 postgres
```

> `design-production-gaussdb.md` §6.4 规划的 `SPINDUCK_OG_ROLE` /
> `ALLOW_SCHEMA_DROP` / `ALLOW_SHADOW_CREATE` 破坏性门禁**尚未落地**（属 A1 待实例项）。
> 在门禁落地之前，请把"对真实例只读"当作**人的纪律**执行：SpinDuck 的
> `materialize` 只发 `SELECT`，`sync` 只向显式白名单表 `INSERT`；除此之外不要手工
> 在真实例上执行任何 DDL/DML。

---

## 1. 准备二进制与环境

```bash
# gauss 蕴含 native 与 cli 契约层；--locked 防依赖漂移
cargo build --release --locked --features gauss
```

**用容器做参照**（推荐先走通这条路，再换真实例）：

```bash
cd golden/gaussdb-docker
cp .env.example .env && $EDITOR .env        # GS_PASSWORD=…（四类字符 + ≥8 位）
docker compose -f docker-compose.yml up -d  # 首次初始化约 40–60s
docker compose -f docker-compose.yml ps     # 等 healthy；容器名 spinduck-gaussdb
```

连接参数：`127.0.0.1:5433`，库 `postgres`，用户 `gaussdb`，口令即 `.env` 里的
`GS_PASSWORD`。容器是 openGauss 5.0.0 **Oracle A 兼容模式**（`datcompatibility=A`，
healthcheck 强制校验；md5 + `password_encryption_type=1` 专为纯 Rust 驱动，仅限开发环境）。

**连真实例**：只换环境变量，命令完全相同。

```bash
export SPINDUCK_OG_HOST=<实例地址>
export SPINDUCK_OG_PORT=<端口>
export SPINDUCK_OG_USER=<账号>
export SPINDUCK_OG_PASSWORD=…   # 口令只经 env，不要写进任何文件
export SPINDUCK_OG_DB=<库名>
```

连通性冒烟（最小表、只读）。`materialize` 会先校验**本地库已存在目标表**
（`ENV-0015`，物化只灌数据不建表），所以冒烟分两步——先建一张本地占位表：

```bash
printf 'CREATE TABLE smoke_t0(a INT);' > /tmp/smoke_t0.sql
target/release/spinduck query /tmp/smoke.duckdb --sql-file /tmp/smoke_t0.sql --out /tmp/ddl.out
target/release/spinduck materialize --db /tmp/smoke.duckdb --table smoke_t0 --where "1=0"
# 成功（rows=0）= 通道可达 + 认证通过；失败先看错误域
# （ENV-0014 = env 未配 / ENV-0015 = 本地目标表未建 / KERNEL = 连接层）
```

> openGauss 官方镜像在 **arm64 宿主**上会解析到 amd64 userland，起容器即
> `gosu: cannot execute binary file`——这条路被 CI 实测堵死（validation V21.12），
> 容器方式需要 amd64 环境。连**真实例**没有此限制（走网络协议）。

---

## 2. 采集参照侧基线（当前为手工环节）

参照侧 = 真 GaussDB 内核血统对同一份源码 + 同一份数据的输出。流程：

1. **确保实例上有项目资产**：表 DDL、视图、例程。golden 用例的参照侧行为由黄金
   harness 在测试层自动重建（对容器）；对手上的新项目，先用 `gsql` 把
   `sql/ddl/tables.sql`、`sql/ddl/views/*.sql`、`sql/routines/*.sql` 在实例上执行一遍，
   再灌入 `cases/datasets/seed.sql` 样例数据。
2. **对每个场景跑一次参照查询**：按 `cases/*.case.toml` 里 `[[case.scenarios]]`
   的 `args` 在实例上执行例程（`SELECT fnc_x('20250715','0130000001');` 或
   `CALL prc_x(...)` 后查输出表）。
3. **誊成基线**：写入 `cases/baselines/expected.json`。格式与本地快照一致（见 §4），
   单元格存实例给回的文本，数据库 NULL 写 `null`；数字写成文本或数字皆可
   （`compare` 对手写基线宽容：`"600000"` 与 `600000` 同值）。

基线提交入库（黄金层纪律：防基线漂移），改期望值必须单独评审并写清 diff 含义。

---

## 3. 本地侧：生成计划并核对缺件

```bash
cd <项目目录>          # 即含 spinduck.toml 的目录
target/release/spinduck verify .            # 链路核验 + 运行态快照
target/release/spinduck plan <目标例程>     # 落盘到 .spinduck/plan/<目标例程>.toml
```

计划落盘路径是固定约定（无 `--out` 类参数）：`<项目>/.spinduck/plan/<目标例程>.toml`。
`plan` 从 `[[entry]]` 出发求闭包，落盘**字节可复现**的运行计划。计划里
`[[missing]]` 非空即红（列出 `relation/sequence/routine` 缺件与引用方）——
**先补齐缺件再往下走**；`call --plan` 与 `deploy` 都会在执行前再判一次，缺件门
红在**开库之前**，不会留下半成品库文件。

---

## 4. 本地侧：部署 → 物化 → 执行 → 快照

> 顺序是硬依赖：**先 deploy 后 materialize**。schema 的唯一来源是计划
> `ddl_tables`（deploy 逐文件下发建表）；`materialize` 只灌数据、不建表，
> 本地表缺失会以 `ENV-0015` 显式红并指路 deploy（零行 `--where "1=0"` 同样红，
> 无"成功假象"）。

第一步：部署——把计划清单（表/视图/例程 DDL）装进本地 DuckDB：

```bash
PLAN=.spinduck/plan/<目标例程>.toml
target/release/spinduck deploy "$PLAN" --db run/local.duckdb
```

第二步：物化——按计划 `[sources].materialize` 清单把输入表从实例拉进本地库
（只读实例，`SELECT` 进本地库）。V27 起支持多表并行（`--table` 重复/逗号分隔
取并集，**重复表名 = 用法错**；`--jobs` 默认 4；`--where` 仅限单表形态；单表失败
不中断其余表，任一失败退出码 1）。快路径默认走 COPY STREAM；若 stderr 出现
`WARN materialize … 已回落 INSERT 批量路径`，说明该表降级到了慢速路径（大表上
会整表驻留内存），需按失败原因排查后重跑：

```bash
TABLES=$(sed -n '/^\[sources\]/,/^\[/p' "$PLAN" | grep '^"' | tr -d '"' | paste -sd, -)
target/release/spinduck materialize --db run/local.duckdb --table "$TABLES" --jobs 8
# 多表形态每表完成即输出 [k/总数] 前缀行；deploy 会在部署时核验清单内
# 每张表已被项目 DDL 建好（缺表 = MISS + exit 1，先补 sql/ddl 再重跑）
# 或沿用逐表形态：
# for t in …; do target/release/spinduck materialize --db run/local.duckdb --table "$t"; done
```

物化失败后残留的临时 CSV（`.spinduck-mat-<表名>-<pid>.csv`，点开头隐藏文件，
与目标库同目录）保留供诊断，**不会自动清理**；反复失败的宽表会在 run 目录堆积，
重跑前按前缀清点、确认无用后删除：

```bash
ls -la "$(dirname run/local.duckdb)"/.spinduck-mat-*.csv
rm "$(dirname run/local.duckdb)"/.spinduck-mat-*.csv
```

**重跑前的强制清表口径（V27.1）**：`materialize` 是纯追加（全路径无
`DELETE`/`TRUNCATE`），失败分两类处理——

- stderr 出现 `WARN … 已回落 INSERT 批量路径` 且最终 `rows=N` 正常：数据只有
  一份（快路径在入库前失败，0 行入库），排查原因后**直接重跑**；
- 命令报 `[ENV-0020] 行数交叉核对失败`：此时 `COPY FROM` **已成功入库**，数据
  已在本地表中（错误消息里的「CSV 已保留供诊断」即此含义）。**直接重跑 =
  两倍行数**——必须先清空该表再重跑：

```bash
# query 的契约是「跑一条 SQL 把结果集落成快照」：DELETE 无结果集，但会回
# 受影响行数（rows），可安全用作清表门面；--out 必填（快照文件，可丢弃）
printf 'DELETE FROM <表名>;\n' > /tmp/spinduck-clean.sql
target/release/spinduck query --db run/local.duckdb \
  --sql-file /tmp/spinduck-clean.sql --out /tmp/spinduck-clean.json
# 也可用 DuckDB 客户端等价清表：
#   duckdb run/local.duckdb -c "DELETE FROM <表名>"
```

> architecture §13.1 的「幂等覆盖」（物化前按快照清表）在代码面上尚未实现，
> 在此之前清表是手工步骤。

第三步：执行（两条路等价，按需选）：

```bash
# 路线 A：对已部署的落盘库执行（部署在第一步已完成，便于插桩、复查）
target/release/spinduck call --db run/local.duckdb --plan "$PLAN" '20250715' '0130000001'

# 路线 B：一步直呼（内存库，部署在进程内完成）
target/release/spinduck call --plan "$PLAN" '20250715' '0130000001'
```

执行面同样先过缺件门（参照面是**内核目录**而非项目声明），缺件即 `MISS` 行 +
退出码 1，例程本体不会执行。大业务（10 万级游标循环）默认预算不够时放大入口：

```bash
target/release/spinduck call --db run/local.duckdb --plan "$PLAN" \
  --batch-commit --step-budget 200000 …
```

排障面：这条调用到底跑了哪些 SQL、绑定值怎么变，加 `--trace`（stdout 契约不受影响；
缺件即红时日志与 sidecar 都不产出，因为一条都没执行；**日志目标已存在即 `ENV-0016`
拒绝执行**——覆盖旧日志等于把上一轮"跑过什么"的证据清掉，所以宁可一条都不跑）：

```bash
TRACE=run/trace-$(date +%Y%m%dT%H%M%S).log   # 一次调用一份，旧证据不会被碰
target/release/spinduck call --db run/local.duckdb --plan "$PLAN" \
  --batch-commit --step-budget 200000 --trace "$TRACE" …
tail -5 "$TRACE"                          # 每条 SQL 一行：sqlid + 前 96 字符 + 绑定值
grep '^sq-978f03531652' "$TRACE.sql"      # 全文反查（sidecar 与日志同前缀 + .sql）
# 日志 ↔ IR 对齐：哪条静态 SQL 一次都没跑（分支没走到 / 循环体没进）
target/release/spinduck explain --file sql/routines/PKG_X.sql \
  --trace "$TRACE" <包>.<过程>
```

`--trace-format jsonl` 给机器消费（§11.2 字段契约），`--trace-level stmt|sql|bind|all`
控制详细度，默认 `bind`。日志是**逐条全量**（同形语句不合并），体量与执行语句数同阶——
游标循环每取一行就多至少一条，`all` 级还另加每条 IR 指令；请先按 `run/` 的磁盘余量选级别，
`run/trace-*.log` 攒多了自己归档或删（引擎侧不做轮转）。
落地口径与已知缺口见 `architecture.md` §11.2「落地口径」、`validation.md` V39 / V40。

把裁决对象落成快照（一条 SQL；文件里多语句会被拒绝，红在发出内核之前）：

```bash
target/release/spinduck query run/local.duckdb \
  --sql-file cases/queries/q.sql --out run/local.json
```

## 5. `batch`：`.sdsh` 脚本同进程编排（可选）

§3–§4 与 §6 的每一步都是独立进程：每条 `call` 都重开库、重读源码重编译、重建维表索引，
参数写错要到前几步落盘之后才暴露。`spinduck batch <script.sdsh>` 把多步指令收进
**同一个进程**按序执行，并把"先全量预检、后逐步执行"做成硬契约。设计与取舍见
`docs/spinduck-batch-sdsh-design.md`；本节只写操作口径。

### 5.1 命令面与退出码

```bash
target/release/spinduck batch cases/daily_purchase.sdsh
target/release/spinduck batch run.sdsh --dry-run             # 只预检，零副作用
target/release/spinduck batch run.sdsh --continue-on-error  # 步失败继续跑，批级仍退 1
```

命令行只有三个面：脚本路径 + `--continue-on-error` + `--dry-run`。**库默认值不在
命令行给**——在脚本里用 `use` / 顶格 `--db` 行声明（§5.2）。

退出码：`0` 全部成功（`--dry-run` 预检通过也是 0）；`1` 任一步失败（含
`--continue-on-error` 下仅一失败）；`batch` 自身用法错（缺脚本、重复旗标、未知
旗标）走全局契约退 `2`。**脚本内部**的词法/预检/执行错误一律 `[CODE] msg` 打
stdout、退 `1`，stderr 保持为空。

### 5.2 `.sdsh` 脚本语法

封闭指令集，不是 Bash 子集：

- **指令行** = 单步 CLI 去掉程序名：`deploy` / `materialize` / `sync` / `query` /
  `call` / `compile` / `explain` / `rules probe` / `compare` / `version` 原样可用；
  `batch`、`help`（含 `--help`/`-h` 首词）与裸词 Shell 算符
  （`| > >> < << && || ; &`）一律拒。算符判定在 token 层——`--where "a;b"`、
  `--slice 'k=a|b'` 的引号内容是数据不是算符。
- **注释**：trim 后行首 `#` 的整行；**没有行内注释**——参数里的 `#` 是数据。
- **续行**：行尾引号外**奇数个** `\` 紧贴换行才续行（`\` 与换行之间夹空白不续行；
  `\\` 是字面反斜杠）；注释/空行会终结进行中的续行；EOF 处孤立 `\` 报 `ENG-0010`。
- **引号**：`"..."` 剥引号并展开 `$VAR`/`${VAR}`（支持 `\"` `\\` `\$` 转义）；
  `'...'` 剥引号且**完全字面**——`'bcrq=$BIZ_DATE'` 原样透传给 `sync --slice`
  在运行期按 env 解析。裸词同样展开变量。
- **变量**：`set KEY [=] VALUE`（`=` 可选；值 = 其后全部 token 经引号处理后以
  单空格连接）写入脚本局部表，**不污染进程环境**。展开顺序 `vars` → 进程环境，
  两者皆无即 `ENV-0017` 预检拒跑——不会把 `$VAR` 字面量漏进 SQL 或实参。
  要传字面 `$` 用 `\$VAR`（裸词转义）或单引号包裹。
- **默认库**：`use <库>` 或顶格 `--db <库>` 行按序更新后续指令的默认库；
  `deploy`/`materialize`/`sync`/`query`/`call` 缺 `--db` 时自动注入当前默认库
  （行内显式 `--db` 优先）。**全程未设默认库**：`call` 省略 `--db` = 每步独立
  匿名 `:memory:`（与单步 CLI 一致）；其余四指令缺 `--db` 报 `ENG-0010`。
  `use :memory:` 是**显式共享**内存库——前步 `query` 建的表后续 `call` 可见。
- **`str:` 语法糖**：只在 `call` 行的位置实参上生效，`str:0130000001` ≡
  `'0130000001'`（`parse_literal` 保真前导零）；旗标值不转换，其他子命令不转换。
  裸写 `0130000001` 会被推断成整数、丢前导零——与 §4 `call` 实参口径相同。

### 5.3 端到端示例

```sdsh
# daily_purchase.sdsh —— §2–§6 全流程收进一个进程
set BIZ_DATE = 20250715

use run/local.duckdb
# （顶格 --db run/local.duckdb 写法等价；注意 .sdsh 没有行内注释，# 必须独占一行）

deploy .spinduck/plan/prc_trd_deal_purchase.toml
materialize --table dat_trd_purchase_src,par_fund_partner --jobs 8

call --plan .spinduck/plan/prc_trd_deal_purchase.toml \
     --batch-commit --step-budget 200000 "str:${BIZ_DATE}" str:0130000001
call --plan .spinduck/plan/prc_trd_hz_byfund.toml "'${BIZ_DATE}'"

query --sql-file cases/queries/check_repurchase.sql --out run/local.json
compare cases/baselines/expected.json run/local.json --key purchase_no
sync --table dat_trd_repurchase --cols purchase_no,fund_code,bcrq \
     --mode slice --slice "bcrq=${BIZ_DATE}"
```

脚本含 `materialize`/`sync` 需要 `gauss` feature 构建（`--features gauss`，
§1 同款）；含 `deploy`/`query`/`call` 需要 `native`（`cli` 蕴含）。缺 feature
时**预检即拒**（`ENV-0012`），一条都不执行。

### 5.4 两阶段契约与缓存语义

- **Phase 1 预检（不碰库）**：词法/变量展开 → 逐行 `cli::args::parse`（`--db`
  注入在 parse **之前**）→ feature 门禁 → `call --trace` 冲突检查（同一路径
  出现两次、或目标文件已存在 → `ENV-0016`）。预检失败 = **零副作用**：不建
  `.duckdb`、不建 trace 日志、不写 `--out`。`--dry-run` 走到这里返回 0 并打印
  `batch: dry-run steps=N default_db=<...>`。
- **Phase 2 执行**：同一库只开一条 canonical 连接，引擎各持克隆连接；`deploy`
  与后续同闭包 `call --plan` 命中引擎缓存（跳过重复编译）；`deploy` /
  `materialize` / `query` / `sync` 任一写步后，该库的引擎 memo/lookup 缓存自动
  失效（`--continue-on-error` 下失败也失效）。
- **trace**：脚本里给 `call` 显式 `--trace <f>` 与单步契约相同——路径必须全批
  唯一且不存在；审计事件的进程字段自动带 `stepNN:` 前缀，同名例程可按步区分。

### 5.5 与手工逐步的取舍

- 适合：固定流水线（deploy→物化→多 entry call→快照裁决→回写）、想把参数错误
  挡在第一步执行之前。
- 注意：`materialize`/`sync` 每步仍各自建 GaussDB 短连接（V1 不跨步池化）；
  `deploy` 非幂等——重跑 batch 前先删本地库（§7）；没有 `--trace-dir` 自动
  分配，trace 路径要在脚本里逐条写死。

## 6. 逐值裁决

```bash
target/release/spinduck compare cases/baselines/expected.json run/local.json --key <键列,...>
# 绿：compare: key=… rows=n/m diffs=0，退出码 0
# 红：逐行 DIFF value/arity/only-in-a/only-in-b + 汇总行，退出码 1
```

裁决口径（与黄金层同一份 `norm` 实现）：

- **判值不判文本**：尾零、`+` 前缀、`.5` 补零、空串即 NULL 都先归一再比；
  列名/别名不参与判定，**列数不同才是差异**（arity）。
- **行序不算差异**：千万级实例上 `SELECT` 不保证行序，**务必带 `--key`**；
  键列必须两侧都存在，找不到即 `ENV-0031`，绝不退化成位置序。
- **不早停**：一次跑完所有行，报告是差异清单而非第一个差异。
- 归一不解析数字：`00123` ≠ `123`（文本保真承诺）；`NUMBER` 的 scale 渲染差
  （`0.0000` vs `0`）不算差异。

## 7. 收尾与已知边界

- **一次只执行一条 SQL**：`query` 按 `'…'`/`"…"`/`--`/`/* */` 安全切句，多于一条
  即 `ENV-0031` 且一个 `--out` 字节都不写——这是对"DuckDB prepare 会把多句都执行、
  只回最后一句结果集"这一内核事实的防线。
- **快照整体驻留内存**：`query` 面向结果裁决（聚合输出、小表全量）；千万级取数
  的流式出口是 A3 的 COPY OUT 改造，本轮前请先在 SQL 侧收敛结果集。
- **`deploy` 非幂等、例程不落盘、`plan.schema` 不下发**：重跑前删本地库文件即可
  （`run/local.duckdb*`），真实例不受影响。
- **待实例项**（落地后本文相应小节会改为自动命令）：`run <plan>` 端到端编排、
  参照侧自动采集、§6.4 破坏性门禁（role 开关）、A3 COPY OUT 流式、A4 回写门禁。
  多步 `.sdsh` 编排已由 `batch` 承接（§5），但它不替代 `run <plan>` 的单计划
  一条龙语义。当前口径登记见 `docs/validation.md` V27–V36 与
  `docs/design-production-gaussdb.md` §6.2 各阶段判定。

## 8. 故障速查

| 症状 | 域/码 | 第一排查点 |
|---|---|---|
| `SPINDUCK_OG_HOST is required` | ENV-0014 | §1 环境变量没导出（口令同理） |
| 连接被拒 / `connection closed` | KERNEL | 口令错误；或自建实例没做 md5 + `password_encryption_type=1` 改造（容器口径见 `golden/gaussdb-docker/`） |
| 容器起不来 `gosu: cannot execute binary file` | — | arm64 宿主跑 amd64 镜像，换 amd64 环境或直连真实例 |
| `MISS <obj> <- <routine>` 后退出 1 | 缺件门 | 按 `plan.toml` 的 `[[missing]]` 补 DDL/例程，重跑 `plan` |
| `stream is neither valid UTF-8 nor valid GBK` | ENV-0030 | 源文件/基线编码两种判定都不成立，修文件而不是改判定 |
| `快照 JSON …` | ENV-0031 | 基线手写格式错：顶层必须 `{schema, rows}`，单元格只能是文本/数字/布尔/null |
| `--key 列 x 不在快照 schema 里` | ENV-0031 | 键名写错或参照侧缺该列；先修基线/查询，不要去掉 `--key` |
| `审计日志 … 已存在：--trace 拒绝覆盖` | ENV-0016 | 上一轮的日志还在原地，本次一条都没跑：换新路径（`run/trace-<时间戳>.log`）再跑，别"先删后跑"——删掉的那份正是可比对的两轮证据 |
| `trace log …` 且不是"已存在" | ENV-0012 | `run/` 目录不存在或不可写；日志与 sidecar 都不该出现半成品，先修目录再重跑 |
| `batch: …未定义变量 $X` | ENV-0017 | `.sdsh` 引用了 vars 与进程环境都没有的变量：补 `set` 或导出环境变量；要字面 `$` 改用 `'…$X…'`/`\$X`（§5.2） |
| `sdsh: …` / `step N (…)` 且码是 ENG-0010 | ENG-0010 | 脚本词法/用法：引号未闭合、行尾孤立 `\`、裸词 shell 算符、嵌套 `batch`/`help`、未设默认库时 `deploy` 等缺 `--db`；报错在预检期、一条都没执行（§5.4） |
| `需要 feature 'gauss'` / `'native'` | ENV-0012 | batch 二进制没带对应 feature 编译：含 `materialize`/`sync` 要 `--features gauss`，执行类指令要 `native`（`cli` 蕴含）（§5.3） |
| `--trace 目标已存在/重复` | ENV-0016 | 脚本里两条 `call` 共用了 trace 路径，或目标文件已存在；换新路径重跑——预检期即拒，不会留半成品库（§5.4） |
