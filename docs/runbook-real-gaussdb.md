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
| `run <plan>` 一条龙编排 | ❌ 未落地 | 属 design A5/A6 待实例项，见 §6 |

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

连通性冒烟（最小表、只读）：

```bash
target/release/spinduck materialize --db /tmp/smoke.duckdb --table <一张小表> --where "1=0"
# 成功 = 通道可达 + 认证通过；失败先看错误域（ENV-0014 = env 未配 / KERNEL = 连接层）
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

## 4. 本地侧：物化数据 → 部署 → 执行 → 快照

按计划 `[sources].materialize` 清单把输入表从实例物化到本地 DuckDB
（只读实例，`SELECT` 进本地库）：

```bash
PLAN=.spinduck/plan/<目标例程>.toml
for t in $(sed -n '/^\[sources\]/,/^\[/p' "$PLAN" | grep '^"' | tr -d '"'); do
  # 或直接逐条手敲清单里的表名
  target/release/spinduck materialize --db run/local.duckdb --table "$t"
done
```

部署与执行（两条路等价，按需选）：

```bash
# 路线 A：显式两步（便于在部署后插桩、复查）
target/release/spinduck deploy "$PLAN" --db run/local.duckdb
target/release/spinduck call --db run/local.duckdb --plan "$PLAN" '20250715' '0130000001'

# 路线 B：一步直呼（内存库）
target/release/spinduck call --plan "$PLAN" '20250715' '0130000001'
```

执行面同样先过缺件门（参照面是**内核目录**而非项目声明），缺件即 `MISS` 行 +
退出码 1，例程本体不会执行。大业务（10 万级游标循环）默认预算不够时放大入口：

```bash
target/release/spinduck call --db run/local.duckdb --plan "$PLAN" \
  --batch-commit --step-budget 200000 …
```

把裁决对象落成快照（一条 SQL；文件里多语句会被拒绝，红在发出内核之前）：

```bash
target/release/spinduck query run/local.duckdb \
  --sql-file cases/queries/q.sql --out run/local.json
```

## 5. 逐值裁决

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

## 6. 收尾与已知边界

- **一次只执行一条 SQL**：`query` 按 `'…'`/`"…"`/`--`/`/* */` 安全切句，多于一条
  即 `ENV-0031` 且一个 `--out` 字节都不写——这是对"DuckDB prepare 会把多句都执行、
  只回最后一句结果集"这一内核事实的防线。
- **快照整体驻留内存**：`query` 面向结果裁决（聚合输出、小表全量）；千万级取数
  的流式出口是 A3 的 COPY OUT 改造，本轮前请先在 SQL 侧收敛结果集。
- **`deploy` 非幂等、例程不落盘、`plan.schema` 不下发**：重跑前删本地库文件即可
  （`run/local.duckdb*`），真实例不受影响。
- **待实例项**（落地后本文相应小节会改为自动命令）：`run <plan>` 端到端编排、
  参照侧自动采集、§6.4 破坏性门禁（role 开关）、A3 COPY OUT 流式、A4 回写门禁。
  当前口径登记见 `docs/validation.md` V27–V36 与 `docs/design-production-gaussdb.md`
  §6.2 各阶段判定。

## 7. 故障速查

| 症状 | 域/码 | 第一排查点 |
|---|---|---|
| `SPINDUCK_OG_HOST is required` | ENV-0014 | §1 环境变量没导出（口令同理） |
| 连接被拒 / `connection closed` | KERNEL | 口令错误；或自建实例没做 md5 + `password_encryption_type=1` 改造（容器口径见 `golden/gaussdb-docker/`） |
| 容器起不来 `gosu: cannot execute binary file` | — | arm64 宿主跑 amd64 镜像，换 amd64 环境或直连真实例 |
| `MISS <obj> <- <routine>` 后退出 1 | 缺件门 | 按 `plan.toml` 的 `[[missing]]` 补 DDL/例程，重跑 `plan` |
| `stream is neither valid UTF-8 nor valid GBK` | ENV-0030 | 源文件/基线编码两种判定都不成立，修文件而不是改判定 |
| `快照 JSON …` | ENV-0031 | 基线手写格式错：顶层必须 `{schema, rows}`，单元格只能是文本/数字/布尔/null |
| `--key 列 x 不在快照 schema 里` | ENV-0031 | 键名写错或参照侧缺该列；先修基线/查询，不要去掉 `--key` |
