# 黄金对比（golden）——复现说明

本目录是 SpinDuck 与 GaussDB 基线**逐行比对**的完整复现包：容器环境 + 用例夹具 +
期望基线 + 运行命令。所有对比的语义承诺只有一条：

> **同一份源码/数据，参照实现（GaussDB 内核血统）与 SpinDuck 产出逐行一致。**

## 目录结构

```
golden/
├── README.md                      ← 本文件（复现说明）
├── gaussdb-docker/                ← 参照数据库容器（自包含，见下）
│   ├── docker-compose.yml
│   └── spinduck-entrypoint.sh
├── case_split_agg/                ← 用例 1：聚合管线（toy，自建例程）
├── case_corpus_stock_kind/        ← 用例 2：语料级（考题基线函数逐字）
├── case_corpus_min_tax/           ← 用例 3：语料级（考题基线函数逐字）
└── case_corpus_purchase/          ← 用例 4：语料级重量（核心过程 prc_trd_deal_purchase）
```

每个用例目录包含：

| 文件 | 作用 |
|---|---|
| `schema.sql` | 建表（字段类型取自考题基线 `1_表结构`，保持类型语义） |
| `seed.sql` | 种子数据 |
| `routine.sql` | **被测侧**例程（Oracle/GaussDB PL 语法，SpinDuck 编译执行） |
| `routine_gauss.sql` | **参照侧**等价例程（仅 toy 用例需要；语料用例被测/参照共用同一份逐字源码） |
| `expected.json` | 参照侧产出的期望结果（提交入库，防基线漂移） |

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

连接信息：`127.0.0.1:5433`，db `postgres`，user `gaussdb`，password `Enmo@123`
（口令仅开发容器；测试/CLI 经环境变量 `SPINDUCK_OG_PASSWORD` 注入，不进命令行）。

## 第 2 步：构建并运行对比

```bash
cd spinduck
export CARGO_HOME=../.cargo-home

# 语料级黄金对比（考题基线函数逐字，最有说服力）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD='Enmo@123' \
cargo test --features gauss --test golden_corpus_gauss -- --nocapture

# 语料级重量用例：核心过程 pkg_split_trade_step3_sh.prc_trd_deal_purchase
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD='Enmo@123' \
cargo test --features gauss --test golden_corpus_purchase -- --nocapture

# toy 聚合管线用例（含物化 → CALL → 回写 → 读回全链路）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD='Enmo@123' \
cargo test --features gauss --test golden_gauss

# GaussDB 物化/回写通道（V7）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
SPINDUCK_OG_PASSWORD='Enmo@123' \
cargo test --features gauss --test gauss_e2e
```

未设置 `SPINDUCK_OG_HOST` 时用例打印 `SKIP` 并返回——**跳过不等于通过**，
CI/验收必须在设置了环境变量的前提下运行。

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

## 期望基线（expected.json）的维护规则

`expected.json` 锁住的是**参照实现的行为**，不是某次运行的偶然输出：

- 若与容器实时基线不一致而两侧源码未动 → 参照侧行为变了，必须查明原因后再更新；
- 若 SpinDuk 侧改动导致不一致 → 被测侧行为变了，属回归，修实现而不是改基线；
- 更新 `expected.json` 必须在提交说明中写清是哪一侧、为何变化（AGENTS §4.3）。

## 故障排查

| 症状 | 原因与处置 |
|---|---|
| 容器一直 `health: starting` 后 `unhealthy` | `datcompatibility != A`：卷是用 PG 模式初始化的旧卷，`down -v` 重建 |
| 连接报 `authentication failed` | 口令非 `Enmo@123`（或卷复用了其它口令初始化），`down -v` 重建 |
| 报 `sha256 authentication` 相关 | 容器环境变量被改动，须保留 `GS_HOST_AUTH_METHOD=md5` 与 `OTHER_PG_CONF` |
| 端口 5433 被占 | 修改 compose 端口映射，同步设置 `SPINDUCK_OG_PORT` |
| 参照侧函数部署报 `subprogram body is not ended correctly` | 参照侧 DDL 语法限制：`CREATE PROCEDURE` 不接受 PG `$$` 体、DECLARE 内游标声明受限（validation V10/V13）；语料用例统一用 `CREATE FUNCTION`，toy 用例参照侧走 `routine_gauss.sql` |

## 已知边界

- 参照实现是 openGauss 5.0.0（GaussDB 内核血统）。考核提供的 GaussDB 实例
  不可达（validation V10）；若后续能访问真 GaussDB，用同一夹具重跑即可替换参照，
  夹具与断言不变。
- 对比口径覆盖**值域逐行一致**；行序由用例内 `ORDER BY` 固定，不依赖物理顺序。
