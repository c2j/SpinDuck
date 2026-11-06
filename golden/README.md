# 黄金对比（golden）——复现说明

本目录是 SpinDuck 与 GaussDB 基线**逐行比对**的完整复现包：容器环境 + 用例夹具 +
期望基线 + 运行命令。所有对比的语义承诺只有一条：

> **同一份源码/数据，参照实现（GaussDB 内核血统）与 SpinDuck 产出逐行一致。**

## 目录结构

```
golden/
├── README.md                      ← 本文件（复现说明）
├── gaussdb-docker/                ← 参照数据库容器（自包含，见下）
├── case_split_agg/                ← 用例 1：聚合管线（toy，自建例程）
├── case_corpus_stock_kind/        ← 用例 2：语料级（考题基线函数逐字）
├── case_corpus_min_tax/           ← 用例 3：语料级（考题基线函数逐字）
└── case_corpus_purchase/          ← 用例 4：语料级重量（核心过程 prc_trd_deal_purchase）
```

每个用例目录即一个 **§12.1 项目模式单元**（`spinduck.toml` 声明项目资产与可调用面
`[[entry]]`，`spinduck_core::project` 识别；黄金 harness 是它的首个消费者）：

```
case_corpus_purchase/
├── spinduck.toml                  # 项目声明（[project]/[paths]/[cases]/[engine]）
├── sql/
│   ├── ddl/tables.sql             # 表（被测侧与参照侧都建）
│   ├── ddl/views/*.sql            # 视图（仅参照侧建；被测侧建同名占位视图）
│   ├── routines/*.sql             # 过程/函数/PACKAGE 源码（两侧共用）
│   └── stubs/*.sql                # 外部依赖桩包
├── rules/spinduck-rules.toml      # 项目规则覆盖（本用例为空 = 全用种子规则）
├── probes/README.md               # 规则探针（项目级；本用例由引擎测试层覆盖）
├── cases/
│   ├── datasets/seed.sql          # 样例数据
│   ├── baselines/expected.json    # 参照侧黄金输出（提交入库，防基线漂移）
│   ├── baselines/reference_routine.sql   # 参照侧专用例程（仅 toy 用例）
│   └── *.case.toml                # 用例定义：例程 × 场景 × 输入表 × 期望输出 × `[[case.assert]]` 断言
├── reports/                       # 生成产物（评审用；当前无）
└── .spinduck/                     # 本地运行态（gitignore）
```

用例定义示例（`cases/purchase.case.toml`）：

```toml
[case]
name = "corpus_purchase"
kind = "procedure"                 # procedure=比对输出表；function=比对返回值
routine = "pkg_split_trade_step3_sh.prc_trd_deal_purchase"
baseline = "expected.json"
expect_succeed = "0"
reference_call_sql = "SELECT fn_agg_fund({args})"   # 可选：参照侧专用调用模板

[case.data]
materialize = ["mid_yjqs_detail", "par_fund_partner", ...]  # 需物化的输入表

[case.output]
sql = "SELECT ... FROM dat_trd_repurchase ORDER BY security_id"

[[case.scenarios]]
why = "回购/申购业务拆分（含异常行跳过）"
args = ["'20250715'", "null"]
```

约定：

- `sql/ddl/*.sql` 是**表**（两侧都建）；`sql/ddl/views/*.sql` 是**视图**
  （仅参照侧建——被测侧建同名占位视图满足 `%TYPE` 声明，视图体不参与求值）。
- 输出表**不**进 `[case.data].materialize` 清单：参照侧执行后该表有数据，
  被测侧必须保持为空，由过程自己写入。
- 基线统一为 `{"scenarios":[{"why","value"|"rows"}]}`；数值按值归一
  （scale/尾零差异不算失败）。
- 未设 `SPINDUCK_OG_HOST` 时用例打印 `SKIP` 并返回——**跳过不等于通过**。

## 第 1 步：启动参照数据库容器

```bash
docker compose -f golden/gaussdb-docker/docker-compose.yml up -d

# 等待 healthy（首次约 40–60s）
docker compose -f golden/gaussdb-docker/docker-compose.yml ps
docker logs spinduck-gaussdb | grep spinduck-init
# 预期输出：[spinduck-init] gaussdb ready (Oracle A-mode, md5)
```

容器要点（细节见 `gaussdb-docker/docker-compose.yml` 头注释）：

- openGauss 5.0.0，**Oracle A 兼容模式**（`datcompatibility=A`，healthcheck
  强制校验——A 模式在 initdb 固化，若校验失败须 `down -v` 重建卷）
- md5 认证经镜像环境变量原生配置（`GS_HOST_AUTH_METHOD=md5` +
  `password_encryption_type=1`），纯 Rust `postgres` 驱动可直接连接
- 入口脚本改编自 dota-adapter 交付脚本
  （`lib/fastaas/prototype-1.7/dota-scheduler/dota-adapter/docker/`），
  去除了 DOTA 业务 schema 自动应用（SpinDuck 测试自建全部对象）

连接信息：`127.0.0.1:5433`，db `postgres`，user `gaussdb`；口令由本目录
`.env` 注入（`cp .env.example .env` 后设置 `GS_PASSWORD`，`.env` 不入库），
测试/CLI 经环境变量 `SPINDUCK_OG_PASSWORD` 传入，不进命令行。

## 第 2 步：构建并运行对比

```bash
cd spinduck
export CARGO_HOME=../.cargo-home

# 语料级黄金对比（考题基线函数逐字，最有说服力）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD="$SPINDUCK_OG_PASSWORD" \
cargo test --features gauss --test golden_corpus_gauss -- --nocapture

# 语料级重量用例：核心过程 pkg_split_trade_step3_sh.prc_trd_deal_purchase
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD="$SPINDUCK_OG_PASSWORD" \
cargo test --features gauss --test golden_corpus_purchase -- --nocapture

# toy 聚合管线用例（含物化 → CALL → 回写 → 读回全链路）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD="$SPINDUCK_OG_PASSWORD" \
cargo test --features gauss --test golden_gauss

# GaussDB 物化/回写通道（V7）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD="$SPINDUCK_OG_PASSWORD" \
cargo test --features gauss --test gauss_e2e
```

未设置 `SPINDUCK_OG_HOST` 时用例打印 `SKIP` 并返回——**跳过不等于通过**，
CI/验收必须在设置了环境变量的前提下运行。用例根目录可用
`SPINDUCK_GOLDEN_DIR=<dir>` 覆盖（默认仓库内 `golden/`）。

### 预期输出

```
test golden_corpus_stock_kind_matches_a_mode_baseline ... ok
test golden_corpus_min_tax_matches_a_mode_baseline ... ok
```

每个用例内部做的事（以语料级为例）：

1. 参照侧：容器内建表/灌数/部署**逐字取自考题基线的函数** → 原生 `SELECT fn(...)`
2. 被测侧：DuckDB 建表 → 从容器**物化**种子 → 注册同一份源码 → `Engine::call`
3. 断言两侧输出逐行一致（数值文本按值归一：GaussDB `NUMBER` 按声明 scale 渲染
   `0.0000`，SpinDuck `0`，值相等不算差异）
4. 同时断言与 `expected.json` 一致（锁住参照行为，防基线漂移）

## 重量级用例（case_corpus_purchase）的特别约定

- 容器侧**整体重建应用 schema**（`DROP SCHEMA gaussdb CASCADE` + 重建）保证幂等：
  该容器是本复现包专用实例。
- `src/` 目录保存被测源码**逐字副本**（核心包 + 依赖函数 + 日志桩包），夹具自包含，
  不引用 `lib/fastaas` 路径。
- `sys_dummy` 在 A 模式是系统单行表；被测侧（DuckDB）由测试建 `CREATE TABLE
  sys_dummy AS SELECT 1` 等价物。
- `%TYPE` 锚定的视图（`v_par_fund_check_para`）容器侧部署真实视图；被测侧建单列
  占位视图（类型声明不参与求值）。
- 测试在 256MiB 栈的线程中运行（重量级语料函数的解析/编译递归超过测试默认 2MiB）。
- 已知脱敏件缺陷：`par_fund_seat_para.commision_fee_type` 为 varchar(1) 而费型码为
  2 字符——黄金口径取 `swh_all_kind.kind_id='0'` 确定性路径（purchase_yj 两侧同为
  NULL）；CASE 路径须先修基线列宽。

## 原生断言层（`[[case.assert]]`，不需要容器）

黄金层比对必须有 A 模式容器；**用例自身的结果断言不需要**。四个 `case_*` 项目在
`cases/*.case.toml` 里用 `[[case.assert]]` 显式声明断言集（`rows` 行集 / `call` 返回值 /
`out` OUT 参 / `error` 错误契约），部署口径与 `verify` 同源，跑在进程内 DuckDB 1.5.5 上：

```bash
cd spinduck
export CARGO_HOME=../.cargo-home
cargo test --features native --test cases_native

# 新增断言取实测值（不参与门禁语义；提交前必须以实测值填回 want）
CASES_PROBE=1 cargo test --features native --test cases_native -- --nocapture
```

- 项目里一条 `[[case.assert]]` 都没有 = 显式变红（空覆盖不接受）。
- 链路核验也不需要容器：`case_corpus_purchase/spinduck.toml` 声明了 1 条
  `[[entry]]`，`cargo run --features cli -- verify ../golden/case_corpus_purchase`
  在进程内内核上逐对象核验（`entries=1 checks=11 failed=0`），同时把运行态快照
  落到 `.spinduck/conf/spinduck.toml`（gitignore）。其余三个用例的主角是无包限定的
  独立例程，按"入口必须全名定位"不声明 `[[entry]]`，由断言层兜住。
- 断言期望值与容器基线（`expected.json`）互不替代：前者锁"本引擎在样例数据上的
  行为"，后者锁"参照实现的行为"；改动导致前者变化时按 AGENTS §4.3 说明差异含义。
- 已退役：原用例 5（`case_corpus_hz_byfund`，热点过程 `prc_trd_hz_byfund` 的容器逐行
  比对）随 V21 移出仓库；其 SQL 形态（`UPDATE … SET f(col)`、游标 WHERE 里的 PL 函数
  调用，即 `sqlfn` 参数元组缓存化下沉）由 `spinduck/tests/corpus_sql_forms.rs` 与
  `case_corpus_purchase` 的原生断言覆盖，机制说明见 `docs/architecture.md` §8.3。

## 期望基线（expected.json）的维护规则

`expected.json` 锁住的是**参照实现的行为**，不是某次运行的偶然输出：

- 若与容器实时基线不一致而两侧源码未动 → 参照侧行为变了，必须查明原因后再更新；
- 若 SpinDuk 侧改动导致不一致 → 被测侧行为变了，属回归，修实现而不是改基线；
- 更新 `expected.json` 必须在提交说明中写清是哪一侧、为何变化（AGENTS §4.3）。

## 故障排查

| 症状 | 原因与处置 |
|---|---|
| 容器一直 `health: starting` 后 `unhealthy` | `datcompatibility != A`：卷是用 PG 模式初始化的旧卷，`down -v` 重建 |
| 日志刷 `The supplied GS_PASSWORD is not meet requirements` | 口令没过 initdb 复杂度校验：须大写 + 小写 + 数字 + 特殊字符四类齐全、长度 ≥8（CI 的一次性口令生成器同口径，见 `.github/workflows/ci.yml` golden job 与 validation V21） |
| 连接报 `authentication failed` | 口令与 `.env` 的 `GS_PASSWORD` 不一致（或卷复用了旧口令初始化），`down -v` 重建 |
| 报 `sha256 authentication` 相关 | 容器环境变量被改动，须保留 `GS_HOST_AUTH_METHOD=md5` 与 `OTHER_PG_CONF` |
| 参照侧语句报 `[ENV-0020] … connection closed` | 参照实例的**后端进程**没了，不是被测引擎的问题。先看 `docker logs spinduck-gaussdb` 与 `data/pg_log/*.log` 里失败时刻那一段，并确认容器有没有原地重启过（`docker inspect -f '{{.RestartCount}} {{.State.StartedAt}}'`——compose 带 `restart: unless-stopped`，`status=running` 判不出死过）。内存口径：镜像默认按**宿主**内存推导 `max_process_memory`（实测开发机 12GB / shared_buffers 1GB），推导值随宿主漂移会让同一份语料参照侧行为漂移，故 compose 固定 `max_process_memory = 4GB` + `shared_buffers = 512MB` + 限额 6GB（实测全量 gauss 套件容器峰值 amd64 1.74GB、arm64 1.10GB）。**但"runner 上是 OOM 杀的"这条已经用实测否掉**：CI golden job 采到峰值 670MB/限额 6GB、`OOMKilled=false`、容器 `die exitCode=0`，且容器内 `ulimit -c = 0`（崩溃不可能落 core）——参照实例是被静默终止的（validation V21.9）|
| `SHOW max_process_memory` 与 compose 里写的不一致 | 正常，因为 `OTHER_PG_CONF` 只在**空数据目录首次 initdb** 时追加进 `postgresql.conf`（镜像 entrypoint 的 `opengauss_setup_postgresql_conf` 在 `DATABASE_ALREADY_EXISTS` 为空的那个分支里）。旧卷改口径有两条路：`docker compose down -v && up -d`（丢数据），或手工把三行追加到 `$PGDATA/postgresql.conf` 末尾再 `docker compose up -d` 重建容器（实测有效）。这两个都是 postmaster 级参数，`reload` 不够 |
| 启动日志 `Failed to initialize the memory protect for ... cstore_buffers (512 Mbytes)` | 口径写死（2GB/4GB）时的常态，跟随宿主推导（12GB）时不出现。含义是 openGauss 的实例级内存看护没起来：超限不会被拦成可读 SQL 错误，而是内核静默杀后端。处置方式不是消除这行，而是让容器限额明显高于 `max_process_memory`（本地/CI 实测行数同为非零，不能拿它当 runner 差异项） |
| 端口 5433 被占 | 修改 compose 端口映射，同步设置 `SPINDUCK_OG_PORT` |
| 参照侧函数部署报 `subprogram body is not ended correctly` | 参照侧 DDL 语法限制：`CREATE PROCEDURE` 不接受 PG `$$` 体、DECLARE 内游标声明受限（validation V10/V13）；语料用例统一用 `CREATE FUNCTION`，toy 用例参照侧走 `routine_gauss.sql` |

## 已知边界

- 参照实现是 openGauss 5.0.0（GaussDB 内核血统）。考核提供的 GaussDB 实例
  不可达（validation V10）；若后续能访问真 GaussDB，用同一夹具重跑即可替换参照，
  夹具与断言不变。
- 对比口径覆盖**值域逐行一致**；行序由用例内 `ORDER BY` 固定，不依赖物理顺序。
