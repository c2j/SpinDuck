# SpinDuck

把 **GaussDB / openGauss 存储过程**跑在 **DuckDB** 上的独立执行引擎。

- **不是** SQL 方言翻译器，而是完整的过程执行引擎：PL 语法解析（复用
  `lib/ogsql-parser`）→ 内部 IR → 树遍历解释器；控制流、变量、游标、异常、
  事务标记全部在 Rust 内实现，SQL 语句下发 DuckDB 执行。
- **内核锚定**：DuckDB **1.5.5**（`duckdb` crate `=1.10505.0`，bundled 静态链接）。
- **数据通路**：GaussDB 只读物化进 DuckDB → 引擎处理 → 结果回写 GaussDB
  （PG 线协议，`postgres` crate）。

设计文档：[`docs/architecture.md`](docs/architecture.md)（机制）；
实证记录：[`docs/validation.md`](docs/validation.md)（V1–V10）；
工程规范：[`AGENTS.md`](AGENTS.md)。

## 快速开始

```bash
cd spinduck
export CARGO_HOME=../.cargo-home            # 本仓库自带依赖缓存

cargo build --features cli                  # 默认 native（DuckDB bundled）
cargo build --features gauss                # 追加 GaussDB 连接器

./target/debug/spinduck version
./target/debug/spinduck help
```

### 1. 语料编译体检

```bash
./target/debug/spinduck compile ../lib/fastaas/exam/清算拆分优化考题/基线代码
# OK   …/PKG_SPLIT_QQ.sql  routines=3
# summary: files=136 ok=136 failed=0 routines=54
```

逐文件独立编译、输出 `OK/FAIL` 与失败原因直方图；`FAIL` 时退出码为 1。
非 UTF-8（GBK）源码按有损 UTF-8 解码并标注 `(non-utf8)`。

### 2. 注册并调用例程

```bash
./target/debug/spinduck call --db /tmp/w.duckdb \
  --file ../golden/case_split_agg/routine.sql \
  prc_agg_fund "'20240105'"
# OUT … / RETURN …
```

参数按字面量推断：`42`（整数）、`1.25`（小数）、`'abc'`（字符串）、`null`、`true`。

### 3. GaussDB 物化 / 回写

连接信息走环境变量（口令不进命令行）：

```bash
# 先启动参照容器（复现包见 golden/README.md）
docker compose -f golden/gaussdb-docker/docker-compose.yml up -d
export SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
       SPINDUCK_OG_USER=gaussdb SPINDUCK_OG_PASSWORD='Enmo@123' SPINDUCK_OG_DB=postgres

./target/debug/spinduck materialize --db /tmp/w.duckdb --table mid_yjqs_detail
./target/debug/spinduck materialize --db /tmp/w.duckdb --table mid_yjqs_detail \
  --where "bcrq = '20240105'"                       # 按分区键增量
./target/debug/spinduck sync --db /tmp/w.duckdb --table dat_inst_fund_cjqs \
  --cols bcrq,fund_code,scdm,cjsl,cjje,fee
```

### 4. 规则探针

```bash
./target/debug/spinduck rules probe ../config/rewrite-rules.toml
# rules: … target_duckdb=1.5.5 probe_duckdb=1.5.5 match=true
```

## 能力矩阵

| 能力 | 状态 |
|---|---|
| PL 编译：赋值/IF/ELSIF/CASE/LOOP/WHILE/FOR(range)/GOTO/标签/嵌套块 | ✅ |
| PL 表达式：算术/比较/布尔三值/`BETWEEN`/`IN`/一元负/括号/函数调用 | ✅ |
| 声明段：变量（含初值）、静态游标、`%TYPE`/`%ROWTYPE` 类型名 | ✅（类型名仅解析，运行期按值） |
| 游标：`OPEN`/`FETCH INTO`/`%FOUND`/`%NOTFOUND`/`%ROWCOUNT`/`%ISOPEN`/`CLOSE` | ✅ |
| `FOR rec IN <query>` / `FOR rec IN <cursor>` | ✅（脱糖为隐式游标） |
| 异常：`EXCEPTION WHEN <cond>` 匹配、未捕获上抛、`SQLSTATE` 匹配 | ✅ |
| 例程：`CREATE PROCEDURE`/`FUNCTION`/`PACKAGE BODY`、IN/OUT/INOUT、过程间调用 | ✅ |
| SQL 上下文函数分派 | ✅ 表达式级→内核宏；②内联/①UDF 见 `architecture.md` §8.3 |
| `EXECUTE IMMEDIATE '<静态文本>'` | ✅（运行期拼接显式拒绝 `ENG-0062`） |
| 事务标记 `COMMIT`/`ROLLBACK`（延迟事务） | ✅ 顶层执行 |
| GaussDB 物化/回写 | ✅（文本承载，回写为 append） |
| `rownum = 1` 首行谓词 | ✅ `[patterns] rownum_first_row` 规则改写（AST 级，语料 42 处） |
| 记录变量限定字段下发 SQL（`INSERT … VALUES (r.field)`） | ✅ `BindSrc::Field`（`$v_<rec>__<field>` 绑定） |
| `%ROWTYPE` 整行记录 + 显式 `FETCH c INTO rec` | ✅ |
| PL 上下文 `@` 标记函数（`TO_CHAR`/`TO_DATE`/`INTERVALTONUM`） | ✅ SQL-first 下沉（`SqlFn`，保留日期中间类型；`日期±n` 自动 `to_days`） |
| 黄金对比 | ✅ `golden/`：toy 用例 + 语料级逐字函数（stock_kind / min_tax）+ **核心过程 prc_trd_deal_purchase** |
| `RETURN NEXT` / `PIPE ROW` / 集合类型（嵌套表/数组元素赋值）/ 带参游标 | ⛔ 待做（语料仅 3+1 例，编译期显式拒绝） |
| 例程目录持久化 | ⛔ 待做（当前进程内内存目录） |

语料实证：考题基线 **136/136 文件编译通过（54 例程）**；`exam` 全树 183/187
（余 4 例为集合元素赋值与带参游标，显式拒绝）。详见 `docs/validation.md` V9。

## 测试与门禁

```bash
cd spinduck
cargo fmt --all
cargo clippy --all-features -- -D warnings
cargo test --all-features                     # 单元 + 集成 + 内核探针 + CLI 契约

# GaussDB 相关（openGauss 容器）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD='Enmo@123' \
  cargo test --features gauss --test gauss_e2e --test golden_gauss --test golden_corpus_gauss
```

测试分层（`AGENTS.md` §4）：单元（`src/**`）、集成（`tests/`，假执行器）、
内核探针（真实 DuckDB）、CLI 契约、黄金对比（openGauss 基线）。

## 关键约束

1. **接缝纪律**：DuckDB API 只出现在 `sql/exec_native.rs`；编译器/解释器只依赖
   `SqlExecutor`/`TraceSink` 两个 trait。
2. **变量替换**：PL 变量在 SQL 文本内改写为命名占位符 `$v_<name>`（字符串字面量
   与引号标识符不替换），按名绑定，避免 `?` 位置错配。
3. **错误域**：`ENG-####`（引擎缺陷）/`KERNEL-####`（内核）/`BIZ-####`（业务，
   含 `NO_DATA_FOUND`/`TOO_MANY_ROWS`）/`ENV-####`（环境），在产生点标注。
4. **规则数据化**：方言差异（函数映射、类型映射、格式串、拒绝清单）在
   `config/rewrite-rules.toml`，引擎只实现规则应用；规则变更随探针测试同 PR。
5. **平台**：Linux glibc ≥ 2.28 / macOS 12+ / Windows 10+；Win7 仅发版交叉编译
   （见 `AGENTS.md` §10）。

## 已知限制

- `RETURN NEXT`/`PIPE ROW`/集合类型/带参游标未实现（编译期显式拒绝，不静默错译）。
- SQL 上下文的**过程级**函数（含控制流）只能 PL 上下文调用；SQL 内调用会得到
  `KERNEL-0010` Catalog Error（宏路径只覆盖纯表达式函数）。
- `materialize` 逐行 INSERT：万级可用，百万级需批量通道。
- GaussDB 侧若为 sha256 口令认证，纯 Rust `postgres` 驱动不支持（v1 需 md5 或
  自建 TLS 通道）；golden 复现包的容器经镜像环境变量启用 md5
  （`GS_HOST_AUTH_METHOD=md5` + `password_encryption_type=1`，见 golden/README.md）。
- 黄金基线当前由 openGauss 5.0.0 产出（GaussDB 实例不可达），差异见 V10。
