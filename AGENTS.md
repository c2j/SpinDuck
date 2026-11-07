# SpinDuck 工程规范（AGENTS.md）

> 适用范围：本仓库根下的 `spinduck/` crate 及 `scripts/`、`config/`、`golden/`
> 资产。本文件自包含，是编码、测试、规则数据、提交的唯一规范来源。

---

## 1. 工程结构约束

- 单 crate 多 bin：`spinduck-core`（lib，全部引擎能力）+ `spinduck`（`[[bin]]`，
  CLI 仅壳，零引擎逻辑）；feature 切分：默认 / `cli` / `golden`。
- 不使用 `[workspace]`，保持独立构建。前端 SQL 解析能力经 path 依赖引入
  （`../lib/ogsql-parser`）：**只允许引用其公共 API（Tokenizer/Parser/AST）**，
  不读取、不遵循、不修改其内部规范与测试；遇到其行为问题只记录，不在其仓库内
  动手。**版本锁定**：依赖其 v0.11.x（Cargo.toml 按次版本 pin），升级走评估 PR
  （解析行为变化直接影响规则探针）。
- 无 `rust-toolchain.toml`，日常一律 `stable`；`+nightly` 仅用于 Win7 发版
  交叉编译（见 §10），与日常 TDD 门禁无关。
- **内核版本锚定**：duckdb-rs crate 版本精确 pin 至对应 DuckDB 1.5.5（禁止
  `^` 漂浮）。内核升级 = validation.md V6 门禁的独立 PR。
- 两个内核边界接缝：`SqlExecutor`（SQL 执行）与 `TraceSink`（可观测埋点）。
  DuckDB API 调用**只允许**出现在 `sql/exec.rs`；解释器/编译器只依赖这两个
  trait。接缝签名变更 = Ask first。

---

## 2. TDD 工作流（Red → Green → Refactor）

一次循环只锁定一个行为：先写会失败的测试（Red），再写最小实现让它通过
（Green），最后在测试全绿的前提下重构（Refactor）。探索草稿不得直接合入，
必须按本文件用 TDD 重写。

### 先读再改

1. 单 crate；`spinduck` 是同一 package 的 `[[bin]]` 目标，靠 feature 切分。
   先确认改动落在哪个模块、哪个 feature 下。
2. 只用 §8 列出的 cargo 命令；不要发明裸 `cargo update`。
3. 先跑与改动相关的最小测试；提交前再跑全量门禁（fmt + clippy + test +
   audit，见 §8/§9）。
4. 完成一个循环后按 §11「完成标准与汇报」汇报，不要只说"做完了"。

### Never（不必请示，直接禁止）

- 删除、注释、跳过已有测试：`#[ignore]`、注释掉 `#[test]`、把断言改成
  `is_ok()` / `unwrap()` 了事
- 修改人类已有测试的断言来迁就实现
- 先提交无测试的业务行为，再"回头补"
- 写永真测试：无断言、只检查 `is_some()`、只 verify 调用次数不查参数与状态
- 用全量端到端测试覆盖本可单测完成的改动
- 提交半成品；每次对人类可见的结果必须能构建且相关测试为绿
- 把探索草稿、临时脚本、调试 `dbg!`/`println!` 留在主代码（日志只经
  `TraceSink`，见 §5）
- 库代码（非 main/tests）用 `unwrap` / `expect` / `panic!` 做控制流
- 无必要 `unsafe`；有则必须 Ask first 并写 `// SAFETY:` 注释
- 一次性 `cargo update` 整个 lockfile（如必须，提交说明中逐条解释）
- 用 `#[allow(...)]` 静默应修复的 lint
- 为绿而改 golden/期望值却不解释行为是否应该变
- 在 `sql/exec.rs` 之外调用 DuckDB API（§1 接缝纪律）
- 在提交说明写"做完了/已测试"而不给出证据

### Ask first（先请示再动）

- 改人类已有测试（含断言、fixture、golden 期望值）
- 新增运行时依赖、`unsafe`、新的 feature flag 或 `[[bin]]` 目标、新的外部服务
- 为不可测代码做超出当前改动路径的重构
- 接受/更新 golden file 或固定 fixture 的期望值，且行为含义发生变化
- 关闭 clippy lint、新增 `#[allow(...)]`
- 内核版本升级（走 V6 门禁独立 PR）
- 规则 `[rejects]` 清单新增项（等于收缩兼容范围，需业务确认）
- `TraceSink` / `SqlExecutor` 接缝签名变更（破坏所有消费者）
- 引入 `proptest` 之外的工具类依赖

### Always

- 改遗留路径前：先写守护测试，锁定当前可观察行为（见 §4.3）
- 新行为：先有会失败的行为断言，再写最少实现
- 难以测试时：先造接缝，再写测试（见 §2.6 接缝）
- 测试名描述行为：`should_reject_negative_amount`
- 现有测试因你的改动失败：修实现，不修测试（除非人类明确要求）

### 测试权限

| 测试来源 | 权限 |
|---|---|
| 人类已有测试 | 只读 |
| 本任务新建测试 | 可改，直到该行为稳定 |
| 过时或环境偶发失败 | 只报告，不擅自跳过 |

### 工作流细则

**Red** — 测试必须能被收集且必须失败（断言失败，或因缺失 API 导致编译失败，
都算合法 Red）。一次只加一个行为的测试，禁止一批 20 个用例再一次性实现。
修改已有功能先写守护测试锁定当前输出。

**Green** — 只写让当前失败测试通过的最少代码。禁止删掉/改掉失败测试、一次
引入多个未验证变更、用更宽断言换绿。

**Refactor** — 相关测试全绿后才重构；重构后立刻跑同一组测试；范围限于当前
改动路径。

**探索 vs 实现** — 方案不清可写草稿验证；草稿不得合并；方案确定后必须走
TDD 重写。Rust 的合法 Red 允许是编译失败（引用尚不存在的 API）；可以留
`todo!()` 仅作为 Green 的最小占位，且下一步必须替换。

### 遗留代码与接缝

**守护测试** — 锁定现有行为，不是证明它正确。用固定 fixture + 显式断言，或
与 golden 文件逐字比对。更新期望值必须在汇报里写清 diff 含义；默认不接受
"看起来差不多"。

**接缝（优先顺序，靠后的更差）**

1. trait + 泛型或 `impl Trait`，测试用假类型
2. 用类型去掉非法状态（enum / newtype），而不是在测试里补分支
3. 时钟、ID、熵、文件系统做成可注入依赖；测试用 `tempfile` / 内存实现
4. `unsafe` 不是接缝

只给即将修改的代码路径补测试，不要一次性给整个模块"补全覆盖率"。

---

## 3. 代码规模与结构限制

| 项 | 上限 | 超限处理 |
|---|---|---|
| 单文件 | **500 行**（含 `#[cfg(test)]` 总行 ≤700） | 按职责拆分子模块（mod 目录） |
| 单函数 | **60 行** | 提取子函数；状态机分支考虑查表 |
| 圈复杂度 | 单函数 **≤15** | 拆分或多态/查表替代分支 |
| 嵌套深度 | **≤4 层** | 提前返回（guard clause） |
| 函数参数 | **≤5 个**；布尔参数 ≥2 或语义含糊一律改枚举/结构体 | 封装参数结构体 |
| match | IR/AST 消歧的 match 必须**穷尽**（禁 `_ =>` 兜底吞新变体；确需通配须注释理由） | — |

结构惯例：

- 每个模块文件头一行职责说明（`//!`）；公共类型/函数必须有 `///` 文档，
  文档示例必须可运行，`no_run` 需注释理由
- 边界类型一律 Newtype：`SlotId` / `Label` / `RoutineId` / `ErrorCode` 禁裸
  `usize` / `String`
- 注释语言中文为主；机制改 `docs/architecture.md`，实证改 `docs/validation.md`，
  规则值改 `config/rewrite-rules.toml`——三者不得互相越界
- 规模限制 CI 不做硬阻断（clippy 已覆盖部分），但 Code Review 必须指出；
  同一文件的规模问题连续两次评审豁免 = 必须拆分

---

## 4. 测试体系

### 4.1 分层

| 层 | 位置 | 测什么 | 禁止 |
|---|---|---|---|
| 单元 | `src/**` 内 `#[cfg(test)]` | IR 编译、coerce、名字解析、AssignTarget 消歧等纯函数 | 触碰 DuckDB |
| 集成 | `tests/` | `SqlExecutor` 假实现：改写产物、绑定表（按名）、游标状态机转移 | 依赖真实内核 |
| 探针 | `tests/probes/` + 项目 `probes/` | 每条规则"建表+绑定+取回"三环节，锚定规则 status | 无期望断言 |
| 内核集成 | `tests/`（真实 duckdb，按版本 gate） | 游标/异常/分派矩阵/UDF 重入 | 跳过即绿 |
| CLI 契约 | `tests/cli.rs` | 退出码 + stdout 契约（子命令逐个） | 全量 e2e 代替单测 |
| 黄金 | `golden/` + 项目 `cases/` | 与 GaussDB 基线输出全量比对 | 盲收 diff |
| 不变量 | `proptest`（引入需 Ask first） | SQL↔IR↔改写往返、coerce 单调性 | — |

### 4.2 单元测试写法

- **AAA 结构**（Arrange-Act-Assert），一个测试函数一个行为断言簇
- 测试命名 `should_<期望行为>`（如 `should_map_varchar2_to_varchar`），禁止
  `test_1`；禁止共享可变 fixture（用构造函数/builder 每测新建）
- 断言具体值而非宽泛匹配；错误断言同时校验错误码与消息关键片段
- 假执行器（FakeExecutor）记录"收到的 SQL 文本 + 按名绑定表"供断言——这是
  验证改写器产物的唯一可信方式

### 4.3 守护测试（Guard Tests）

两类，缺一不可：

1. **回归守护**：每个 bug 修复必须先有**复现失败的测试**（修复前跑它必须红），
   修复后转绿并永久保留。禁止只修代码不加守护。
2. **行为锁定**：重构/优化前对遗留路径写守护测试，锁定当前可观察行为（输入→
   输出逐字断言，允许丑，必须可重复）。守护测试的期望值在重构中**永不变更**——
   若重构改变了守护输出，说明行为变了，必须单独评审。

守护测试集中登记（`tests/guard/` 目录，命名 `guard_<模块>_<行为>.rs`），
删改守护测试 = Ask first。

### 4.4 变异测试（Mutation Testing）

- 工具：`cargo-mutants`；**范围**：`src/ir/`、`src/sql/rewrite/`、`src/sql/sqlfn/`、
  `src/runtime/value.rs`（coerce）——编译器与改写是正确性核心（路径随 V19 规模拆分更新）
- **节奏**：不在每次提交跑（太慢）；在 M3.5 / M6.5 / M10 三个门禁节点各跑
  一轮完整变异测试
- **目标**：核心模块 mutation score ≥ **70%**；未达标时逐个评审"幸存变异体"
  ——幸存于错误处理/边界条件的变异体 = 缺失断言，必须补测试或书面说明为何
  该变异等价
- 幸存变异体清单入档（`docs/validation.md` 追加小节），作为下轮测试补强的输入

### 4.5 覆盖率

- 工具 `cargo-llvm-cov`；核心模块（ir/sql/runtime）行覆盖目标 **≥85%**、
  分支覆盖 ≥75%（指导值，不作为 CI 硬门禁；评审看趋势）
- 覆盖率豁免：`#[cfg(test)]`、error 类型定义、CLI 渲染层

---

## 5. 错误与日志规范

- **错误码分段**（对应设计文档 §11.2.2 故障域）：
  `ENG-####`（引擎缺陷）/ `KERNEL-####`（内核）/ `BIZ-####`（业务，映射自
  GaussDB SQLSTATE 段与命名异常）/ `ENV-####`（环境）。错误码表是规则数据，
  新增错误码必须挂探针测试。
- 故障域标签在**产生点**标注（归域判定链），禁止在调用上层事后改判。
- 库代码禁止 `unwrap/expect/panic!` 做控制流；内核 API 返回一律 `?` + 域标注。
- 日志/trace 只经 `TraceSink`；事件命名与字段遵循设计文档 §11.2 JSONL 契约，
  字段增删属破坏性变更需评审。

---

## 6. 规则数据规范（config/rewrite-rules.toml）

- 结构与注释即规范（见文件头）；变更走 PR，**规则值变更与探针测试同 PR**。
- `status` 生命周期：`assumed`（新增默认）→ 探针通过 → `verified`；CI 输出
  assumed 清单并告警；内核升级后全部回 `assumed` 复核（V6 门禁）。
- 携带语义损失的映射（CLOB→VARCHAR、CHAR→VARCHAR、NUMBER 无精度→DECIMAL）
  必须在条目注释写明损失点，评审时逐条确认。
- 项目覆盖文件（spinduck.toml `[paths].rules`）只允许追加/覆盖值，禁止删除
  种子规则（引擎加载时校验并告警）。
- 模拟器资产（`scripts/sim_*.py`）是行为规格的一部分：修改解释器导致模拟器
  口径变化时，须同步更新并说明 diff 含义。

---

## 7. 代码风格补充

- 多行表达式操作符置于行首（方便 diff 阅读）；枚举变体/结构体字段左对齐
- 派生宏中多个不相关 trait 不合并同一行
- 小而 `Copy` 的参数（u64 以下）按值传入；函数体不用 `return`（提前返回除外）
- `iter / iter_mut / into_iter` 命名规范生成迭代器；避免保留字作标识符

---

## 8. 命令

```bash
# 单测（按测试名过滤）
cargo test --all-features <test_name>

# 全量测试
cargo test --all-features

# 提交前门禁（CI 顺序：fmt → clippy → test → audit）
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings
cargo test --features native        # 无容器环境；gauss 见 golden job
cargo audit

# 变异测试（仅 M3.5/M6.5/M10 门禁节点）
cargo mutants -f src/ir -f src/sql -f src/runtime -f src/value.rs
# （逐个 -f；coerce 位于 src/value.rs）

# 覆盖率（评审参考）
cargo llvm-cov --html

# 发版产物（本地复现 CI 的 Linux 目标；需 zig 与 cargo-zigbuild ≥ 0.23.4）
cargo zigbuild --release --locked --features cli --target x86_64-unknown-linux-gnu.2.28
cargo zigbuild --release --locked --features cli --target aarch64-unknown-linux-gnu.2.28
# glibc 上限反查与 tag 守卫（发版门禁的两条断言面）
python3 scripts/ci/check_max_glibc.py target/x86_64-unknown-linux-gnu/release/spinduck
python3 scripts/ci/check_release_tag.py --tag v<X.Y.Z>
# 两个脚本各自的自检（不需要构建产物）
python3 scripts/ci/check_max_glibc.py --self-test
python3 scripts/ci/check_release_tag.py --self-test
```

循环内用 `<test_name>` 过滤；提交前跑全量 `--all-features` 门禁。

---

## 9. CI 要求

CI 定义 `.github/workflows/ci.yml`。常驻门禁五个 job 全部必须通过：

- **Format**: `cargo fmt --all -- --check`
- **Clippy**: `cargo clippy --all-features -- -D warnings`
- **Test**: `cargo test --features native`（默认/内核通道；gauss 黄金层
  需 A 模式容器，缺容器必红——见 golden job）
- **Security Audit**: `cargo audit`（构建期 `zip` 豁免清单见 workflow 注释与
  `docs/architecture.md`「审计豁免」，内核升级 PR 必须复评）
- **Golden**: `cargo test --features gauss`（openGauss 5.0.0 服务容器，
  与 golden/gaussdb-docker compose 同口径；决策记录 2025-11-05 第二轮复审。
  runner 必须是 amd64：官方镜像在 arm64 hosted runner 上解析到 amd64 userland，
  起容器即 `gosu: cannot execute binary file: Exec format error`，换平台这条路被
  CI 实测堵死，见 validation V21.12。语料级用例的黄金层只比最简函数，考题业务过程
  的多维断言走进程内内核（`cargo test --features native --test cases_native`）；
  调整的是**参照侧被测面划分**，不是断言——用例、期望基线、判定门禁一条不减）

此外 **Platform Build**（发版触发，非常驻门禁）定义在 `.github/workflows/release.yml`：
`push` 形如 `vX.Y.Z` 的 tag 触发，按 §10 矩阵构建 + 冒烟（`call` 一个探针例程并断言
`OUT p_o = 42`），产物发布为 GitHub Release 并附 `SHA256SUMS.txt`。四条不可省的前提：

- **版本守卫**：tag 版本必须等于 `spinduck/Cargo.toml` 的 `version`
  （`scripts/ci/check_release_tag.py`，不一致即红——否则资产名与二进制内
  `spinduck version` 自相矛盾）；
- **glibc 由产物反查**：每个 Linux 目标构建后跑 `scripts/ci/check_max_glibc.py`，
  读版本化符号判定 `≤ 2.28`；不采信工具承诺，也不接受"锚定参数写了就算数"；
- **冒烟跑的是打包后的字节**：从 tar/zip 解出来再 `call`，顺带验证打包本身；
- **不允许部分产物静默缺失**：Release job 核对资产数量必须等于目标数。

`workflow_dispatch` 入口只构建并上传 artifact、**不发布 Release**，是在不产生真实
发布物的前提下复验整条矩阵的唯一入口。**当前落地范围 = 3 个目标**（Linux x86_64 /
Linux aarch64 / Windows x86_64）；§10 表里的 macOS 双架构与 Win7 行**尚无发版入口**，
属未完成项而非"矩阵已覆盖"（实测证据见 validation V25，方案取舍见 architecture 决策记录 ⑬）。

> 子模块初始化（`git submodule update --init lib/ogsql-parser`）必须在仓库根
> 执行；workflow 的 `defaults.working-directory` 是 `spinduck/`，故这些步骤逐条
> 显式 `working-directory: .`（否则 pathspec 不匹配，CI 必红）。

> `cargo fmt --all -- --check` 失败是最常见 CI 错误。提交前先运行
> `cargo fmt --all` 自动修复，再 `git add`。版本号在 `Cargo.toml` 维护。

---

## 10. 平台构建矩阵

运行平台要求（引擎与 CLI 同一目标矩阵）：

| 平台 | target | 最低系统 | 工具链 | 说明 |
|---|---|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | **glibc ≥ 2.28**（RHEL8 / Ubuntu 20.04+） | stable + `cargo-zigbuild --target x86_64-unknown-linux-gnu.2.28`（或在不高于 glibc 2.28 的环境构建） | glibc 兼容性是发布硬约束：**禁止在高于 2.28 的构建机直接产物发布**（符号版本会随构建机的 glibc 抬高）。发版已落地（V25 实测：rockylinux:8 内 `readelf -V` 上限 GLIBC_2.28、冒烟通过） |
| Linux aarch64 | `aarch64-unknown-linux-gnu` | glibc ≥ 2.28 | **`cargo-zigbuild ≥ 0.23.4`** + zig 0.15.2，`--target aarch64-unknown-linux-gnu.2.28` | 交叉构建；国产化环境（鲲鹏等）按此目标产物交付。0.22.1 在链接期失败（rustc 传 `-Wl,--fix-cortex-a53-843419`，zig cc 不认）；"Rocky8 容器原生构建"这条路被 Rocky 8.10 aarch64 镜像源 repodata 全镜像 404 堵住（V25） |
| macOS | `aarch64-apple-darwin` / `x86_64-apple-darwin` | macOS 12+（跟随内核支持面） | stable；设置 `MACOSX_DEPLOYMENT_TARGET` | 双架构分别构建；universal2 按需 lipo 合并。**尚无发版 job**（§9 范围外，待补） |
| Windows x86_64 | `x86_64-pc-windows-msvc` | Windows 10+ | stable | 日常发布目标。不加 `+crt-static`（静态 CRT 只属 Win7 行），代价是目标机需 **VC++ 2015-2022 x64 运行库**；发版 job 用 `dumpbin /dependents` 打印静态导入清单作为产物侧证据（只覆盖静态导入），前提说明入 README 已知限制 |
| Windows 7 | `x86_64-win7-windows-msvc`（32 位：`i686-…`） | Windows 7 | nightly + `-Zbuild-std`（见下） | 仅发版，与日常门禁无关。**尚无发版 job**（nightly + build-std + `crt-static` 组合未经探针，待补） |

平台纪律：

- duckdb-rs crate 对上述平台的可用性与静态链接行为（`bundled` feature）在
  M1 探针中逐平台验证一次，结果记录 validation.md；某一平台不可用 = Ask first
  再决策（降级动态链接或裁剪平台）。**现状口径**：M1 并未逐平台跑过；本轮发版
  实测覆盖 Linux x86_64/aarch64（含 duckdb bundled 的 C++ 交叉编译，V25），
  Windows x86_64 以 CI 首跑为证据，macOS/Win7 仍无证据
- CI 构建矩阵与上表一一对应（**当前 3/6**：Linux×2 + Windows x86_64 已入
  `release.yml`，macOS 双架构与 Win7 待补）；资产包命名
  `spinduck-<ver>-<os>-<arch>.tar.gz|.zip`，包内二进制为 `spinduck`/`spinduck.exe`，
  Release 层附覆盖全部资产的 `SHA256SUMS.txt`
- 禁止使用平台专属 API（条件编译 `#[cfg(target_os)]` 需注释理由并配跨平台
  回退路径）；文件路径处理一律走 `std::path`，禁止字符串拼接分隔符
- **Windows 7 专项**（Rust 1.78+ 移除 Win7 支持：stdlib 无条件调用
  `GetSystemTimePreciseAsFileTime`）：

  ```bash
  # 前置
  rustup toolchain install nightly
  rustup component add rust-src --toolchain nightly

  # 构建（32 位同理：i686-win7-windows-msvc）
  cargo +nightly build --release --target x86_64-win7-windows-msvc -Zbuild-std
  ```

  `.cargo/config.toml` 已配置 Win7 目标的 `+crt-static`，静态链接 CRT。
  **Win7 交叉编译与日常 TDD 门禁无关，仅发版时使用。** 文件/目录命名遵循
  docs/architecture.md §11 运行态布局约束（纯 ASCII、无符号链接、大小写安全）。

---

## 11. 完成标准与汇报

提交或交还人类前，确认：

- [ ] 新行为有失败→通过的测试
- [ ] bug 修复有复现守护测试
- [ ] 未删除、跳过、改写人类已有测试
- [ ] 已跑与改动匹配的门禁（fmt + clippy + test）
- [ ] `cargo fmt` 与 clippy 干净
- [ ] 没有把草稿、调试输出、无主 lockfile 大面积变更带上
- [ ] 规则值变更附探针证据与内核版本

每个 TDD 循环汇报：

1. 测试了什么行为（测试函数名）
2. 最小实现改了哪些文件
3. 是否重构、边界在哪
4. 实际执行的命令和结果（通过 / 失败原因；不要只写"测过了"）
