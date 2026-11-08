# SpinDuck

把 **GaussDB / openGauss 存储过程**跑在 **DuckDB** 上的独立执行引擎。

- **不是** SQL 方言翻译器，而是完整的过程执行引擎：PL 语法解析（复用
  `lib/ogsql-parser`）→ 内部 IR → 树遍历解释器；控制流、变量、游标、异常、
  事务标记全部在 Rust 内实现，SQL 语句下发 DuckDB 执行。
- **内核锚定**：DuckDB **1.5.5**（`duckdb` crate `=1.10505.0`，bundled 静态链接）。
- **数据通路**：GaussDB 只读物化进 DuckDB → 引擎处理 → 结果回写 GaussDB
  （PG 线协议，`gaussdb::sync`，见「已知限制」的认证条目）。

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

clone 后即可复现（语料在仓库内）：

```bash
./target/debug/spinduck compile ../golden/case_corpus_purchase/sql
# OK   …/PKG_SPLIT_TRADE_STEP3_SH.sql  routines=2
# summary: files=7 ok=7 failed=0 routines=6
```

全量基线体检跑的是考题语料，那棵树在 `lib/fastaas` 子模块里（私有仓库，需
`git submodule update --init lib/fastaas` 且有访问权限；不在本仓库的 clone 面内）：

```bash
./target/debug/spinduck compile ../lib/fastaas/exam/清算拆分优化考题/基线代码
# summary: files=136 ok=136 failed=0 routines=54
```

逐文件独立编译、输出 `OK/FAIL` 与失败原因直方图；`FAIL` 时退出码为 1。
非 UTF-8（GBK）源码按有损 UTF-8 解码并标注 `(non-utf8)`。

### 2. 注册并调用例程

```bash
./target/debug/spinduck call --db /tmp/w.duckdb \
  --file ../golden/case_split_agg/sql/routines/routine.sql \
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
       SPINDUCK_OG_USER=gaussdb SPINDUCK_OG_PASSWORD="$GS_PASSWORD" SPINDUCK_OG_DB=postgres

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

### 5. 链路校验（verify）

在项目根（含 `spinduck.toml`）声明本项目对外可调用的存过全名，`verify` 从这些入口
出发沿调用链逐对象核验——语料是否齐全、链路对象是否真在核内、声明的场景能否执行：

```toml
# spinduck.toml —— 示意声明形状：[[entry]] 允许多条，全名必须是 package.procedure 形态
# （下面的例程名不属于在库用例项目，只为展示多入口与 kind 的写法）
[[entry]]
routine = "pkg_split_trade_step2.prc_pre_deal_shjycf"   # 缺省 kind = procedure

[[entry]]
routine = "pkg_qq_split.prc_general_deal_inst_sh_buy"
kind = "function"
```

下面是**在库项目的真实输出**（clone 后即可复现；考题项目不在库内，不能当例子）：

```bash
./target/debug/spinduck verify ../golden/case_corpus_purchase
# manifest: ../golden/case_corpus_purchase/.spinduck/conf/spinduck.toml (updated|unchanged)
# == entry pkg_split_trade_step3_sh.prc_trd_deal_purchase
#   PASS Entry pkg_split_trade_step3_sh.prc_trd_deal_purchase — 入口在例程目录命中且种类一致
#   PASS Routine pkg_split_trade_log.inst_log — 由 pkg_split_trade_step3_sh.prc_trd_deal_purchase 调用，已登记
#   PASS Relation dat_trd_repurchase — 内核存在（main.dat_trd_repurchase）
#   PASS Relation mid_yjqs_detail — 内核存在（main.mid_yjqs_detail）
#   PASS Relation par_fund_partner — 内核存在（main.par_fund_partner）
#   PASS Relation par_fund_seat — 内核存在（main.par_fund_seat）
#   PASS Relation par_fund_seat_para — 内核存在（main.par_fund_seat_para）
#   PASS Relation par_sys_purchase — 内核存在（main.par_sys_purchase）
#   PASS Relation swh_all_kind — 内核存在（main.swh_all_kind）
#   PASS Relation sys_dummy — 内核存在（main.sys_dummy）
#   SKIP Scenario - — 无场景声明该入口；2 条 [[case.assert]] 已由用例 corpus_purchase 覆盖该例程，本命令不执行断言（断言层见 cases_native）
# verify: entries=1 checks=11 failed=0 skipped=1
```

链路断点/缺表时长这样（示意片段，非某项目完整输出）：

```
#   FAIL Routine pkg_x.prc_gone — 链路断点：被 pkg_y.prc_z 调用，但语料中未登记
#   FAIL Relation t_missing — 链路 SQL 引用，内核目录无此对象（缺 DDL/桩，或部署顺序不符）
```

出现 `FAIL` 退出码为 1。核验同时把可调用面与资产绝对路径写成运行态快照
`<home>/conf/spinduck.toml`（唯一写入者 = `verify`，禁止手改）；把该目录单独交给
运行侧即可脱离研发态目录复现同一核验结论。

## 能力矩阵

| 能力 | 状态 |
|---|---|
| PL 编译：赋值/IF/ELSIF/CASE/LOOP/WHILE/FOR(range)/GOTO/标签/嵌套块 | ✅ |
| PL 表达式：算术/比较/布尔三值/`BETWEEN`/`IN`/一元负/括号/函数调用 | ✅ |
| 声明段：变量（含初值）、静态游标、`%TYPE`/`%ROWTYPE` 类型名 | ✅（类型名仅解析，运行期按值） |
| 游标：`OPEN`/`FETCH INTO`/`%FOUND`/`%NOTFOUND`/`%ROWCOUNT`/`%ISOPEN`/`CLOSE` | ✅（带参游标 `CURSOR c(args)` 声明段编译期不报错，拒绝只发生在 `OPEN c(args)` 调用点，见下） |
| `FOR rec IN <query>` / `FOR rec IN <cursor>` | ✅（脱糖为隐式游标） |
| 异常：`EXCEPTION WHEN <cond>` 匹配、未捕获上抛、`SQLSTATE` 匹配 | ✅ |
| 例程：`CREATE PROCEDURE`/`FUNCTION`/`PACKAGE BODY`、IN/OUT/INOUT、过程间调用 | ✅ |
| SQL 上下文函数分派 | ✅ 表达式级→内核宏（路径③）；已注册例程调用→参数元组缓存化下沉（路径②，`sqlfn`）；路径① UDF 仍推迟（`architecture.md` §8.3） |
| `EXECUTE IMMEDIATE`（静态文本与运行期拼接同一管线） | ✅ 与 OPEN FOR 共用解析/改写管线；`USING` 绑定 ⛔ 待做（`ENG-0058`，变量须先拼进文本） |
| 事务标记 `COMMIT`/`ROLLBACK` | ✅ 统一事务边界：最外层调用包 `BEGIN…COMMIT`，过程内 `COMMIT`/`ROLLBACK` 即时生效并重开（`architecture.md` §3.5） |
| GaussDB 物化/回写 | ✅（文本承载；回写走驱动 `copy_in`（COPY FROM STDIN）单事务追加、物化 500 行/语句批量 INSERT，实测 100k/98k 分别 26s/1.2s，见 validation V26.4） |
| `rownum = 1` 首行谓词 | ✅ `[patterns] rownum_first_row`（AST 级，语料 42 处：顶层 SELECT 与 `INSERT … SELECT` 子查询；UPDATE/DELETE 无对应语法位，原样下发由内核报错） |
| 记录变量限定字段下发 SQL（`INSERT … VALUES (r.field)`） | ✅ `BindSrc::Field`（`$v_<rec>__<field>` 绑定） |
| `%ROWTYPE` 整行记录 + 显式 `FETCH c INTO rec` | ✅ |
| PL 上下文 `@` 标记函数（`TO_CHAR`/`TO_DATE`/`INTERVALTONUM`） | ✅ SQL-first 下沉（`SqlFn`，保留日期中间类型；`日期±n` 自动 `to_days`） |
| **SQL 上下文例程调用**（`UPDATE SET f(col)` / 游标 WHERE / INSERT VALUES 中的 PL 函数） | ✅ 参数元组缓存化下沉（`sqlfn`，确定性前提 + 10 万元组上界） |
| `%NOTFOUND` 空游标 / `SQLERRM` / `UPDATE` 去别名 / `SELECT INTO` 行数语义（0 行 `NO_DATA_FOUND`、>1 行 `TOO_MANY_ROWS`，静态与动态同口径） | ✅ |
| 黄金对比 | ✅ `golden/`：toy 用例 + 语料级逐字函数（stock_kind / min_tax）+ 核心过程（purchase） |
| 合并 COMMIT 开关（`Engine::batch_commit`） | ✅ 默认 OFF（§3.5 逐行即时提交不变）；ON 时过程内 INSERT 攒批合并 + COMMIT 吞并 + 出口统一提交（100k 稳态 938s→316s；失败行数据级跳过的偏差点见 writebuf.rs，validation V24.3） |
| 用例原生断言层（无容器） | ✅ `[[case.assert]]`（`rows`/`call`/`out`/`error` 四维，进程内内核逐值比对）+ 语料形态覆盖门禁 `config/corpus_forms.toml`（`architecture.md` §12.1、validation V21） |
| 入口链路核验 `spinduck verify` | ✅ `[[entry]]` 全名声明 → 例程断点 / 表·序列缺失 / 场景可执行逐条判定 + 运行态 `conf/` 快照（`architecture.md` §11.1、§12.1） |
| `RETURN NEXT` / `PIPE ROW` / 集合类型（嵌套表/数组）/ 带参游标 | ⛔ 待做（语料仅 3+1 例；`RETURN NEXT`/`PIPE ROW`/集合元素**赋值**/`OPEN c(args)` 编译期显式拒绝，集合元素**读取**不属拒绝面，见「已知限制」） |
| 例程目录持久化 | ✅ 内核内 `spinduck_routines` 表存源 DDL 文本，`reload_catalog` 按当前规则重放编译（`--db` 落盘后跨进程恢复） |

语料实证：考题基线 **136/136 文件编译通过（54 例程）**；`exam` 全树 182/187
（余 5 例：3 集合赋值 `v_info_tab := …`（同一形态在三份收集件里各计一次）+ 1 带参游标
`OPEN` + 1 声明了例程却未登记任何例程，均为显式拒绝）。详见 `docs/validation.md` V9、V23。

## 测试与门禁

```bash
cd spinduck
cargo fmt --all
cargo clippy --all-features -- -D warnings
cargo test --all-features                     # 单元 + 集成 + 内核探针 + CLI 契约

# GaussDB 相关（openGauss 容器）
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 SPINDUCK_OG_PASSWORD="$GS_PASSWORD" \
  cargo test --features gauss --test gauss_e2e --test golden_gauss --test golden_corpus_gauss
```

测试分层（`AGENTS.md` §4）：单元（`src/**`）、集成（`tests/`，假执行器）、
内核探针（真实 DuckDB）、CLI 契约、黄金对比（openGauss 基线）。

## 发版产物

`.github/workflows/release.yml` 在 push `vX.Y.Z` tag 时构建并发布 GitHub Release：

| 资产 | 目标 | 说明 |
|---|---|---|
| `spinduck-<ver>-linux-x86_64.tar.gz` | `x86_64-unknown-linux-gnu` | cargo-zigbuild 锚定 glibc 2.28 符号 |
| `spinduck-<ver>-linux-aarch64.tar.gz` | `aarch64-unknown-linux-gnu` | 同上（国产化环境按此交付） |
| `spinduck-<ver>-windows-x86_64.zip` | `x86_64-pc-windows-msvc` | 需目标机装 VC++ 2015-2022 x64 运行库 |
| `SHA256SUMS.txt` | — | 覆盖上述全部资产 |

发布前三道断言，任一不过即不发：tag 版本 == `spinduck/Cargo.toml` 的 `version`；
产物实际要求的 `GLIBC_*` 符号最大值 ≤ 2.28（`scripts/ci/check_max_glibc.py` 反查
ELF 版本化符号，不采信构建参数写了什么）；每个目标从**打包后的压缩包**里解出来跑
`call prc_smoke 14 null` 并断言 `OUT p_o = 42`。`workflow_dispatch` 入口只构建上传
artifact、不发布（实测与踩坑记录见 `docs/validation.md` V25）。

已发布版本：`v0.1.1`（首跑）与 `v0.1.2`（复跑，含发版矩阵与缓存缺陷修复的证据）——
各为三个目标的压缩包 + `SHA256SUMS.txt` 四个资产，断言原文（含 Windows 侧
`dumpbin /dependents` 的静态导入清单）与两轮差异登记在 V25。发版按
"bump 随 PR 落地 → 常驻五门禁绿 → 合并 → tag 打在 CI 已验证的 main head"顺序进行。

macOS 双架构与 Windows 7 在 `AGENTS.md` §10 矩阵内，但**当前没有发版 job**。

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

- `RETURN NEXT`/`PIPE ROW`（编译器兜底支 `ENG-0022`）、集合元素赋值 `t(i) := v`
  （`ENG-0022`）、带参游标 `OPEN c(args)`（`ENG-0048`）与 `FOR r IN c(args)`
  （`ENG-0059`）编译期显式拒绝，不静默错译；但**集合元素读取** `v := t(i)` 会编成
  标量函数调用，到运行期才报 `KERNEL-0010`——该形态语料真实存在（`PKG_IMPORT_EXCEL`
  的 `v_info_tab(1)`），拒绝面尚未覆盖，登记为待补（V23）。
- SQL 上下文的**过程级**函数（含控制流）调用：已在例程目录登记的走参数元组缓存化
  下沉（`sqlfn`，路径②；确定性前提 + 10 万元组上界；上下文互引显式拒绝 `BIZ-0160`），
  其余名字（宏路径③只覆盖纯表达式函数、UDF 路径①仍推迟）会得到 `KERNEL-0010`
  Catalog Error，属明确失败、不静默降级。
- `materialize` 逐行 INSERT / `sync` 逐行往返：**已落地批量通道**（V26）——物化 500 行/语句
  多值 INSERT、回写走驱动 `copy_in`（COPY FROM STDIN）单事务追加；100k/98k 实测 26s / 1.2s。
- GaussDB/openGauss 的 **sha256 口令认证已可用**（2026-11-07 换驱动）：数据通道由纯 Rust
  `postgres` 改为 `rust-opengauss` 的同步门面 `gaussdb::sync::*`（实现 sha256 /
  md5-sha256 / SM3），默认口令形态实例直连由 `tests/gauss_sha256.rs` 三条断言锁定
  （`cargo test --features gauss-sha256`，compose 内新增 `gaussdb-sha256` 服务，见 V24）。
  golden 参照实例**仍保持 md5 口径**不变——撤掉这个后门等于同时改参照实例与认证面，
  拆成独立决策（V24「未做的事」）。代价：`gauss` feature 变为 git 依赖（`rev` 钉住），
  `cargo audit` 对它没有公告面，见 `architecture.md` 决策记录 ⑫。
- Windows x86_64 产物**不是自包含**：`x86_64-pc-windows-msvc` 走默认动态 CRT（静态
  `+crt-static` 只属 `AGENTS.md` §10 的 Win7 行），目标机通常需要 VC++ 2015-2022 x64
  运行库。每次发版由 release job 的 `dumpbin /dependents` 打印该 exe 的静态导入 DLL
  清单作为产物侧证据（delay-load / 运行时 `LoadLibrary` 的依赖不在这张面上）；
  在装了 Visual Studio 的 CI runner 上冒烟通过**不等于**裸机可直接跑（V25）。
- macOS 与 Windows 7 尚无发版入口：§10 矩阵的这两类目标既无 job 也无构建证据。
(docs(validation): V24 三段引擎优化与数据搬运通道的实证记录；architecture/README 对应口径)
- 黄金基线当前由 openGauss 5.0.0 产出（GaussDB 实例不可达），差异见 V10。
