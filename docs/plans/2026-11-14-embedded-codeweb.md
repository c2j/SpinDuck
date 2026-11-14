# SpinDuck 嵌入 codeweb 实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 将 codeweb 从「进程外 CLI 契约」改为「进程内嵌入库」：① codeweb 的 ogsql-parser 升到 v0.11.0 与 SpinDuck 对齐；② codeweb 库化并暴露 `codeweb::embed` 稳定接口；③ SpinDuck 引入 codeweb v0.9.9 替换子进程实现，随后做嵌入后的优化。

**Architecture:** 三个阶段跨两个仓库。codeweb 侧先做依赖升级（PR-A）再库化开接口（PR-B/C），合并后打 tag `v0.9.9`；SpinDuck 侧用 `[patch]` 把 git 源 ogsql-parser 重定向到本地子模块（保证单份 parser），将 `CodewebExport` trait 换成 `CodewebSource`，`walk.rs`/闭包语义零改动。

**Tech Stack:** Rust 2021 stable / ogsql-parser v0.11.0（git tag `v0.11.0` = commit `13d9baa`，与 SpinDuck 子模块 pin 完全一致）/ codeweb v0.9.8→v0.9.9 / petgraph / bincode store。

**设计蓝本:** `docs/embeded-codeweb.md`（本文档不重复其 §3.2 的完整类型定义，引用之）。

---

## 实地核查更新（写计划时确认的事实，执行者不必重查）

以下事实修正了设计文档 Draft 时的假设，直接影响任务量：

1. **ogsql-parser 已有 `v0.11.0` tag**（remote 实测 `refs/tags/v0.11.0 → 13d9baa`），且 `13d9baa` 正是 SpinDuck 主仓子模块 pin 的提交、Cargo.toml `version = "0.11.0"`。**对齐 = codeweb 把 git tag 从 `v0.10.0` 改成 `v0.11.0` 一行**，无需 ogsql-parser 发新 tag。
2. **`ibatis` / `java` feature 在 v0.11.0 仍存在**（`lib/ogsql-parser/Cargo.toml` features 段实测）。
3. **D4 阻塞④已基本解决**：`Edge::ContainsSql` 在 `graph/mod.rs:793`、`export/json.rs:902`、`store.rs:1933`、`mermaid.rs:156` 均已 `#[cfg(feature = "jsp")]` 门控。**仅剩一处**未门控：`src/graph/builder.rs:4687`。`cargo check --no-default-features --features cli` 当前只报这一个 `E0599`。
4. **codeweb 测试基线健康**：默认 features 下 795 + 17 + 7 + 44 + 3 个测试全绿（本机实测，<2s）。
5. `Project::diff(&mut self)`（`src/project/mod.rs:481`）实现只读 config + `load_manifest_only()`，不改 `self.store`——收窄为 `&self` 是纯签名收紧。
6. codeweb 仓库有自己的 `AGENTS.md`（TDD、`cargo test --features full` 带固定 skip、每加 feature 必须跑 `--features full` 门禁），codeweb 侧所有任务必须遵守它。
7. 本 SpinDuck worktree 的 ogsql-parser 子模块**未 checkout**（`git submodule status` 前缀 `-`，registered commit `13d9baa` = v0.11.0）。执行 Phase 3 前必须先 init。
8. `main.rs` 约 4,841 行，所有 `mod` 声明、`println_stdout!` 宏、`i18n!` 都在 bin 侧；`rust_i18n` 只被 `main.rs` 与 `tui/app.rs` 使用（实测 grep）。

## 仓库位置（执行者必读）

| 仓库 | 工作目录 | 分支 |
|---|---|---|
| codeweb | `/Users/c2j/Projects/Desktop_Projects/CODE/codeweb` | 新建 `embed-api`（基于 main `52ee052`） |
| SpinDuck（本计划文档所在） | `/Users/c2j/Projects/Desktop_Projects/DOTA/SpinDuck__worktrees/tuning-embeded-codeweb` | 当前 worktree 分支 |

## 需人类明示授权的点（AGENTS Ask first 清单）

- [ ] SpinDuck 新增运行时依赖 `codeweb`（git dep，不进 `cargo audit` 面，同 `gaussdb` 先例）
- [ ] SpinDuck `Cargo.toml` 新增 `[patch]` 段
- [ ] 删除 `--codeweb-bin` / `SPINDUCK_CODEWEB`（改 `tests/cli/usage.rs`、`tests/cli/compile_project.rs` 等人类测试断言）
- [ ] generator 字面量 `codeweb 0.9.7` → `codeweb 0.9.9`（改 `plans.rs` 等处人类测试断言；计划文件字节变化需在 validation.md 记 diff 含义）
- 用户在会话中已方向性批准（「将 spinduck 所引用的 codeweb 升级到 codeweb 最新版」）；执行时按上述逐条在 PR 描述中显式列出即可，无需再停。

---

# Phase 1：codeweb ogsql-parser 升级 v0.11.0（codeweb PR-A）

**工作目录：`/Users/c2j/Projects/Desktop_Projects/CODE/codeweb`**

> codeweb 的既有 866 个测试就是本次升级的守护面（它们大量锚定解析产出的图形态）。本 Phase 无新测试，验收 = 全量测试在升级后仍绿。

### Task 1.1: 建分支并记录升级前基线

**Step 1:** `git checkout main && git pull && git checkout -b parser-v0.11`

**Step 2:** 跑基线并留存输出：

```bash
cargo test --features full -- --skip test_path_mapping_applied --skip test_serve_ 2>&1 | tee /tmp/codeweb-baseline.txt
```

Expected: 全绿（与 AGENTS 记载的既有环境跳过项除外）。若 full 存在与本次无关的既有失败，**逐条记录到提交信息**，并确认后续未使其更糟。

### Task 1.2: 升级依赖 tag

**Files:**
- Modify: `Cargo.toml`（`ogsql-parser = { git = "...", tag = "v0.10.0", features = ["ibatis", "java"] }` → `tag = "v0.11.0"`，其余不动）

**Step 1:** 改 tag 后 `cargo check 2>&1 | tail -5`。
Expected: 编译通过或暴露 API 破坏。v0.10.0→v0.11.0 是 minor bump（semver 兼容），大概率直接过；若报错，逐条按 ogsql-parser CHANGELOG 修调用点（只允许动公共 API 适配，不改逻辑）。

**Step 2:** 全量测试（同 Task 1.1 命令）。
Expected: 全绿。**这是本次升级的语义门禁**——任何一个图形态测试红了，先判断是「v0.11 解析行为改进导致的合理变化」还是「回归」：合理变化需在提交信息逐条写 diff 含义；回归则报 ogsql-parser 侧 issue（AGENTS §1 纪律：不在其仓库动手，只记录）。

**Step 3:** `cargo fmt --all && cargo fmt --all -- --check && cargo clippy --features full -- -D warnings`。

**Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build(deps): bump ogsql-parser v0.10.0 -> v0.11.0 for SpinDuck alignment"
```

**验收:** PR-A 可独立合入；合入前把 `/tmp/codeweb-baseline.txt` 与升级后输出 diff 一次，凡测试数变化逐条解释。

---

# Phase 2：codeweb 库化与 `codeweb::embed` 接口（codeweb PR-B、PR-C → tag v0.9.9）

**工作目录同 Phase 1。** PR-B（license + lib/bin + feature 收紧）与 PR-C（embed 模块）分开提交、顺序合入，最后一个 PR 顺带 bump 版本并打 tag。

## PR-B：LICENSE + 库化 + feature 收紧

### Task 2.1: LICENSE 与 license 字段

**Files:**
- Create: `LICENSE-MIT`、`LICENSE-APACHE`（从 ogsql-parser 仓库复制同款文本：`git -C <spinduck>/../lib/ogsql-parser` 或 GitHub 原文）
- Modify: `Cargo.toml`：`license = "MIT OR Apache-2.0"`

**Step 1:** 复制两个 license 文本，加字段。
**Step 2:** `cargo check && git commit -m "chore(legal): add MIT OR Apache-2.0 license"`。

### Task 2.2: 修最后一个 feature 门控缺口

**Files:**
- Modify: `src/graph/builder.rs:4687`

**Step 1（Red）:** 现状已红：`cargo check --no-default-features --features cli` 报 `E0599: ContainsSql` 于 `builder.rs:4687`。确认该行处于 JSP 相关构建路径（读上下文确认它只在 `jsp` feature 语义下可达）。

**Step 2（Green）:** 与其余四处同款门控：把该调用点包进 `#[cfg(feature = "jsp")]`（若是 `match` 臂，同 `graph/mod.rs:793` 的写法给臂加 cfg；若是独立语句，用块级 cfg）。

**Step 3:** `cargo check --no-default-features --features cli` → 通过；`cargo check --no-default-features --features jsp` → 通过（门控两态都要绿）。

### Task 2.3: 依赖 optional 化 + 新 `analyze` feature

**Files:**
- Modify: `Cargo.toml`

目标 feature 面（在现有基础上增量，`default`/`full` 行为不变）：

```toml
[features]
default = ["cli", "tui", "jsp"]
full = ["cli", "tui", "serve", "mcp", "jsp", "java", "analyze", "search-sql-v2"]
# ……现有 cli/tui/serve/mcp/jsp/search-sql-v2 保持不变，另加：
java = ["dep:tree-sitter", "dep:tree-sitter-java"]
analyze = ["dep:ogsql-parser", "dep:rayon", "dep:walkdir", "dep:globset", "dep:encoding_rs", "dep:blake3"]
```

依赖改造（每条都要有对应代码门控，见 Task 2.4）：
- `ogsql-parser`、`rayon`、`walkdir`、`globset`、`encoding_rs`、`blake3` → `optional = true`，进 `analyze`。**注意**：`diff()` 依赖 `scan_with_fingerprints`（blake3/walkdir/globset/encoding_rs），所以「diff 能力」要求 `analyze`；这是设计文档 §3.1 的既定口径，在 `embed` 模块文档注释里写明。
- `tree-sitter`、`tree-sitter-java` → `optional = true`，进新 `java` feature（当前它们是非 optional 直依赖）。
- `indicatif` → `optional = true`，进 `cli`（`cli = ["dep:clap", "dep:indicatif"]`）。
- **`ogsql-parser` 不加 `ibatis`/`java` features 进 `analyze`**（设计文档未明说，此处是本计划的决策）：iBatis/Java/JSP 解析代码路径全部挂到 codeweb 自己的 `jsp`/`java` feature 上。这样 SpinDuck 只开 `analyze` 时，统一后的 ogsql-parser feature 面 = default（`[]`），**与 SpinDuck 今天用它的 feature 面完全一致**，规则探针语义零变化，省掉一轮 V6 式复核。代价是 codeweb 侧门控工作量；备选方案（analyze 带 ibatis+java，然后 SpinDuck 补探针）记入 PR 描述被否理由。

**Step 1:** 改 Cargo.toml。
**Step 2:** `cargo check` （default）→ 报缺依赖错误清单，即为下一步门控任务的需求清单。
**Step 3:** Commit 门控骨架（可与 Task 2.4 合并提交，但 commit message 分开写清两部分）。

### Task 2.4: 代码路径 feature 门控

**Files（预估，按编译错误清单收敛）:**
- `src/parser/extractor.rs`、`src/parser/loader.rs`、`src/parser/ibatis_loader.rs`、`src/parser/java_loader.rs`、`src/parser/jsp_*.rs`、`src/graph/builder.rs`、`src/sql_match.rs`、`src/graph/mod.rs`、`src/export/*`

规则：
1. `parser::ibatis_loader`、`parser::java_loader`、`parser::jsp_*` 整模块挂各自 feature（`jsp` / `java`）；import/mapper 入口调用点同门控。
2. 依赖 `ogsql-parser` 类型或指纹扫描（`walkdir`/`globset`/`blake3`/`encoding_rs`/`rayon`）的代码——`graph/builder.rs`、`graph/store.rs` 扫描与重建索引路径、`export` 消费点——挂 `analyze`。**预期结论**：分析内核整体本来就属于 `analyze`，这里的门控是「让 analyze-only 能编译」，不是把分析功能做成可选。
3. **禁止**为编译省事把与上述依赖无关的核心代码（图类型、store 读写、embed API）顺手关进 feature。

**验收（每完成一批跑一次，两态都要绿）：**

```bash
cargo check --no-default-features --features analyze   # SpinDuck 嵌入形态：必须绿
cargo build --features full                             # 全开形态：必须绿
cargo test --features full -- --skip test_path_mapping_applied --skip test_serve_   # 全绿
```

（纯 `--no-default-features` 零依赖形态不在验收内：嵌入场景固定带 `analyze`，为它多切一层门控是 YAGNI。）

### Task 2.5: lib/bin 拆分

**Files:**
- Create: `src/lib.rs`
- Modify: `src/main.rs`、`Cargo.toml`（`[lib] name = "codeweb" path = "src/lib.rs"` + `[[bin]] required-features = ["cli"]`，见设计文档 §3.1）

**Step 1:** `src/lib.rs` 内容 = 现 main.rs 的 `mod` 声明集去掉 bin 专属项。依赖 ogsql-parser / 指纹扫描的模块必须挂 `analyze` 门控（否则 `--no-default-features --features analyze` 编不过）：

```rust
//! codeweb 库入口：图分析内核与 embed 稳定接口。CLI 逻辑在 bin crate。

pub mod error;
pub mod export;
pub mod graph;
pub mod import;
pub mod mark;
pub mod parse_log;
pub mod project;
pub mod embed;                 // Task 2.7 建，先留空实现避免编译错
#[cfg(feature = "analyze")]
pub mod parser;                // ogsql-parser 消费方
#[cfg(feature = "analyze")]
pub mod sql_match;             // ogsql-parser 消费方
```

（`graph::builder` 等 ogsql 依赖点在 Task 2.4 里按编译错误清单同样挂 `analyze`；`graph` 本体保持无条件可用。）

**Step 2:** `main.rs` 保留：`#[macro_use] extern crate rust_i18n; i18n!(...)`（bin 侧才能用 `t!`）、`mod mcp; mod server; mod tui;`、`println_stdout!` 等宏、`fn main()` 与 CLI 路由。bin 侧对 lib 类型的引用改为 `use codeweb::graph::…` 等路径。`mcp`/`server`/`tui` 若引用图类型，同样走 `codeweb::` 前缀。

**Step 3:** 处理宏外溢：grep `println_stdout!\|t!(\|tokio_main` 等 main.rs 宏在 `src/graph|project|export|parser` 下的使用（实测当前为 0，但拆分后以编译为准）；有则把该宏下沉到 lib.rs 并 `pub(crate)`。

**Step 4:** 移除 lib 模块上仅因 bin 视角而加的 `#[allow(dead_code)]`（`import`/`parser`/`project` 的 mod 声明处）；若个别项确实死代码，保留 allow 并注释理由。

**验收:** 三个 check 形态（Task 2.4 同款）+ `cargo run -- --version` 输出不变 + `cargo test --features full -- --skip test_path_mapping_applied --skip test_serve_` 全绿。

### Task 2.6: 库路径去 `eprintln!` 与 indicatif 进度条治理

**Files（grep 实测清单）:** `src/graph/traverse.rs:198`、`src/graph/store.rs:435,1149-1160,2421`、`src/graph/mod.rs:1642-1652`（stats 打印）、`src/graph/cluster.rs:2075`、`src/parser/jsp_loader.rs:93`；**另加 indicatif 进度条**：`src/project/mod.rs:145,223`（analyze / diff 路径）、`src/graph/store.rs:456,515`（索引重建）、`src/graph/cluster.rs` 透传。

处置原则（设计文档 §3.1「库路径禁止直接写 stdout/stderr，进度与诊断通过返回值或可选回调传递」）：
1. `eprintln!` 明确是诊断信息的：改为返回值携带（`Result<_, _>` 已有错误通道的）或加到现有结构化诊断输出；stats 类（`mod.rs:1642` 的 `graph_stats`）改为返回字符串/结构体，由 CLI 层打印。
2. `store.rs:1149-1160` 的「多匹配提示后取第一个」：这是**行为**，把「ambiguous」事实返回给调用方（返回值带 warning 字段或 `Result` 变体），由 CLI 层决定打印。
3. `jsp_loader.rs:93` 在 `jsp` feature 下，仍属库路径，同原则处理。
4. CLI 层（main.rs）补上原先由库侧打印的文案，`--features full` 全量测试与既有 CLI 契约测试兜底；文案变化若破坏已有断言 → Ask first，逐条列进 PR。
5. **indicatif 进度条（审核补漏项）**：嵌入后 `analyze()`/`diff()`/索引重建会在 spinduck 进程里向 stderr 画 spinner，污染 CLI 契约。最小方案（不改任何函数签名）：抽一个统一构造入口，如 `fn spinner(total: Option<u64>) -> indicatif::ProgressBar`，内部 `std::io::stderr().is_terminal()` 为假时返回 `ProgressBar::hidden()`；五处构造点全部改走它。验收：
   - TTY 行为不变（人工冒烟 + 既有 CLI 测试）；
   - 非进程 TTY（CI / 嵌入）stderr 零进度输出；
   - spinduck 侧 Task 3.4 的测试以「`load_graph` 不产生 stderr 输出」为隐含验收（CLI 测试断言 stderr 形态时自然覆盖）。

### Task 2.7（PR-C）: `codeweb::embed` 模块 —— 严格 TDD

**Files:**
- Create: `src/embed.rs`（类型定义照抄设计文档 §3.2.2–§3.2.4，不在此重复）
- Modify: `src/lib.rs`（`pub mod embed;`）、`src/error.rs`（新增两变体）、`src/project/mod.rs`（diff 收窄）

**T2.7.1 错误变体（Red→Green）**

测试（`src/error.rs` 的 tests mod 或 embed 测试内）：

```rust
#[test]
fn should_report_store_not_found_with_path() {
    let err = CodeWebError::StoreNotFound { path: PathBuf::from("/x/store.bincode") };
    assert_eq!(err.to_string(), "store not found at /x/store.bincode");
}

#[test]
fn should_report_store_version_mismatch_with_versions() {
    let err = CodeWebError::StoreVersionMismatch {
        path: PathBuf::from("/x/s"), found: 14, expected: 15,
    };
    let text = err.to_string();
    assert!(text.contains("v14") && text.contains("v15") && text.contains("/x/s"), "{text}");
}
```

实现：`error.rs` 增两变体；`store.rs::load_bincode/load_json` 的版本不匹配路径（现为文本 `unsupported cache version {found}`）改抛 `StoreVersionMismatch`；缺文件路径改抛 `StoreNotFound`（现复用 `ExportError` 文本，`project/mod.rs` `load_store` 处）。**改动现有错误形态 = 改人类行为**：先 grep 现有测试对这些文本的断言，逐条同步（codeweb 侧 Ask first 条目）。`stale_store_version` 的取值可复用既有 `GraphStore::peek_version`（`.sisyphus/plans/fix-analyze-stale-store-version.md` 那轮修复的产物，免全量反序列化探测版本头），不要另造探测逻辑。

**T2.7.2 `version()`**

```rust
pub fn version() -> &'static str { env!("CARGO_PKG_VERSION") }
```
测试：`assert!(version().split('.').count() == 3)`。

**T2.7.3 `Project::diff` 收窄为 `&self`**

签名改动 + 调用点适配 + 编译兜底；有现成 diff 测试，不新写。

**T2.7.4 `EmbeddedProject` + `DiffStatus`（fixture 驱动）**

fixture：`tests/fixtures/` 下已有小型项目可直接用（执行时 `ls tests/fixtures` 选最小的 SQL 项目；没有就按 `tests/regress` 现有建项目模式在测试内 tempfile 构造：写 `codeweb.toml` + 1 个 `CREATE PROCEDURE` 文件）。

测试序列（一个测试一个行为）：
```rust
// a) 未 analyze 的项目：diff 报 store_exists=false
// b) analyze 后：diff.is_up_to_date() == true
// c) 改源文件再 diff：modified 非空、is_up_to_date() == false
// d) 删 store 再 diff：store_exists == false
```
实现：`EmbeddedProject` 薄包装 `Project`；`diff()` 转发并组装 `DiffStatus`（`stale_store_version` 来自 `load_manifest_sidecar` 的版本字段或 load 时的 `StoreVersionMismatch::found`）。

**T2.7.5 `export_thin_graph` + 语义等价测试（本 Phase 的核心资产）**

等价测试设计（守卫 ThinGraph ↔ JSON 投影不漂移）：

```rust
// 同一 fixture 项目：先 codeweb export --format json（走现有 to_json），
// 在测试内用现有 JSON 模型取"保留类节点+7类边"；再 export_thin_graph()。
// 断言两集合 1:1：
//   - 节点 (id, kind, 关键字段) 全等
//   - 边 (source, target, kind, file, line) 全等
//   - TableAccess.modes 的 Vec<AccessModeTag> 与 JSON modes 数组语义一致
```

`ThinNodeKind` 的字段映射表（实现与测试共用此口径，写在 `embed.rs` 顶部注释）：

| graph::Node 变体 | ThinNodeKind | 字段来源 |
|---|---|---|
| `Procedure/Function` | 同名 | `RoutineId`→schema/package/name；`SourceLocation`→file/line；`partial` |
| `Table/View` | 同名 | `explicit`/`system` 透传 |
| `MaterializedView`、`Sequence`、`Unresolved`、`BuiltinFunction` | 同名 | — |
| `MappedStatement/JavaSql/JavaMethod/JavaClass/JspPage/JspSql/Trigger/Type/Index/Synonym/Event/Custom` | **跳过** | — |

`ThinEdgeKind` 映射：`DirectCall→Direct`、`DynamicCall→Dynamic`、`TableAccess→TableAccess`（`AccessMode` bitflags 按位展开为 `Vec<AccessModeTag>`，`write_kinds` HashSet 排序后转 Vec）、`DependsOn`、`UsesSequence`、`AnchorsOn`、`ContainsRoutine` 原样；其余边变体跳过。**边两端只要有一端落在被跳过节点集 → 整条边跳过**（`Dynamic` 边 target 指向发出调用的例程自身，天然保留）。

**T2.7.6（可选，允许 defer）**：`LockConflictView`/`query_conflicts`。建议本轮不做（YAGNI，SpinDuck 二期才消费）；若做，`conflict.rs` 已有 `ConflictSeverity/LockConflict` 类型，薄映射即可。

**PR-C 验收：** Task 2.4 四条 check/test 全绿 + embed 测试全绿。

### Task 2.8: 发版 v0.9.9

1. `Cargo.toml` version → `0.9.9`；`cargo build` 刷新 lock。
2. 按本仓惯例 `chore(release): bump version to 0.9.9`。
3. 合入 main 后打 tag `v0.9.9` 并 push。发版流程已核实：`.github/workflows/build.yml` 以 `tags: ["v*"]` 触发，`--features full` 矩阵构建 + `softprops/action-gh-release` 自动建 Release——无需额外操作，只需确认 Actions 跑绿、资产齐全。
4. **记录 SpinDuck 侧要用的契约常量**：`codeweb::embed::version() == "0.9.9"`。

---

# Phase 3：SpinDuck 接入嵌入 codeweb（本 worktree，单 PR 或按任务拆小 PR）

**工作目录：本 worktree。** 每个 Task 一个 TDD 循环，AGENTS §2/§4 全程适用；`walk.rs`、`mod.rs` 的闭包语义**零改动**。

### Task 3.0: 环境就位

```bash
git submodule update --init lib/ogsql-parser     # AGENTS §9：必须在本仓根执行
git -C lib/ogsql-parser rev-parse HEAD           # 断言 commit id（勿用 describe：本地 tag 数据可能陈旧）
```

Expected: HEAD = `13d9baa4358ca9537eb4f753fd2e5cd2d5a2517b`（= 远端 `v0.11.0` tag 指向的提交，`git ls-remote --tags https://github.com/c2j/ogsql-parser.git` 可复核）。本 worktree 此前子模块未 checkout，init 后 registered commit 正是它。若不一致：先同步子模块 pin，不继续。

```bash
cargo test --features cli 2>&1 | tail -3          # 改动前基线全绿
```

> 执行顺序原则（审核修正）：Task 3.1–3.4 全部是**纯新增**（加依赖、加 trait 与两个实现、加映射函数），每步结束时全量测试必须与基线同样绿，不碰任何调用点；Task 3.5 一次性替换调用点并删除旧路径；Task 3.6 起 CLI 与文档。禁止「先改调用点再补实现」的中间红态提交。

### Task 3.1: 引入 codeweb 依赖 + 单份 parser 证明（Ask first 已获授权）

**Files:**
- Modify: `spinduck/Cargo.toml`

```toml
[dependencies]
# ……现有依赖不动，追加：
# 嵌入 codeweb（docs/embeded-codeweb.md）。git dep 不进 cargo audit 面，
# 豁免登记同 gaussdb 先例（见 CI workflow 注释与 docs/architecture.md 审计豁免）。
codeweb = { git = "https://github.com/c2j/codeweb", tag = "v0.9.9", default-features = false, features = ["analyze"] }

[patch."https://github.com/c2j/ogsql-parser"]
# 把 codeweb 的 git ogsql-parser 重定向到本地子模块，保证进程内只链接一份
# （version 两边同为 0.11.0，patch 合法；AGENTS §1 版本锁定纪律）。
ogsql-parser = { path = "../lib/ogsql-parser" }
```

**Step 1:** 加依赖后 `cargo check --features cli 2>&1 | tail -3` → 编译通过。

**Step 2（单一 parser 断言，本次嵌入的硬门禁）：**

```bash
cargo tree -i ogsql-parser --features cli
```
Expected: **恰好一个节点**，来源为 path `../lib/ogsql-parser`。若出现 git 源副本 = patch 未生效，停。

**Step 3:** `cargo test --features cli` 全绿（此步不改任何行为，纯依赖面验证）。

**Step 4:** commit：`build(deps): embed codeweb v0.9.9 via git tag, patch ogsql-parser to path dep`。

### Task 3.2: 新增 `CodewebSource` trait 与 `JsonCodewebSource`（纯新增，不碰调用点）

**Files:**
- Create: `spinduck/src/plan/codeweb/source.rs`
- Modify: `mod.rs`（声明 + 导出）

**Step 1:** 定义 trait（放 `source.rs` 顶部）：

```rust
/// 从 codeweb 获取瘦图与产出方版本。
/// 生产实现走进程内嵌入式 codeweb（EmbeddedCodewebSource）；
/// 测试夹具注入内存 JSON（JsonCodewebSource）。
pub trait CodewebSource {
    /// 写入 `RunPlan.generator` 的字面量，如 "codeweb 0.9.9"。
    fn generator(&self) -> String;
    fn load_graph(&self, codeweb_root: &Path) -> Result<CodewebGraph, EngineError>;
}
```

**Step 2:** `JsonCodewebSource`（放 `pub`：现有 `tests/cli.rs`、`tests/verify_chain.rs`、`plans.rs` 测试都用 JSON 夹具）：

```rust
pub struct JsonCodewebSource {
    pub generator: String,
    pub json: Vec<u8>,
}
impl CodewebSource for JsonCodewebSource {
    fn generator(&self) -> String { self.generator.clone() }
    fn load_graph(&self, _root: &Path) -> Result<CodewebGraph, EngineError> {
        parse_codeweb_json(&String::from_utf8_lossy(&self.json))
            .map_err(|e| EngineError::env(30, format!("codeweb export 失败: {}", e.message)))
    }
}
```

**Step 3:** 单测：`JsonCodewebSource::load_graph` 喂一份最小 JSON，断言 `load_graph` 返回的图能被 `closure_from_graph` 消费；坏 JSON → `ENV-0030` 且消息以 `codeweb export 失败:` 开头（`plans.rs::export_failure` 的匹配前缀依赖它）。

**验收:** `cargo test --features cli` 与基线等绿（旧 `CodewebExport` 路径原样共存，本 Task 不删不改）。commit：`feat(plan): add CodewebSource seam with a JSON fixture implementation`。

> 旧 `export.rs`（`CodewebExport`/`CommandExport` 等）在本 Task **原样保留**，Task 3.5 才删除——中间态双接缝共存只存在于一个 Task 内部，不提交。

### Task 3.5: 调用点替换与旧路径删除（含 `--codeweb-bin` 收口与 `--analyze`）

**Files:**
- Modify: `spinduck/src/plan/codeweb/mod.rs`、`plans.rs`、`plan/closure.rs`、`cli/run/mod.rs`、`cli/run/compile_project.rs`、`cli/args/mod.rs`、`cli/args/compile.rs`、`tests/cli/usage.rs`、`tests/cli/compile_project.rs`
- Delete: `spinduck/src/plan/codeweb/export.rs`（git rm）

**Step 1:** `plans.rs`：`plans_from_export` → `plans_from_source(..., source: &impl CodewebSource)`；`const GENERATOR` 删除，`plan_from_closure` 增加 `generator: String` 参数，取 `source.generator()`；`drain` 随 `Box<dyn Read>` 接缝一起删除。

**Step 2:** 全仓把 `impl CodewebExport` 调用点换成 `CodewebSource`：`plan/closure.rs::build`、`cli/run/compile_project.rs`、`cli/run/mod.rs::compile_with_export`；生产装配点固定为 `EmbeddedCodewebSource { auto_analyze: <见 Step 4> }`。

**Step 3:** `git rm src/plan/codeweb/export.rs`：删除 `CodewebExport`/`CommandExport`/`AmbientCommandExport`/`resolve_codeweb_bin`/`OwnedExport`/`check_version`/`check_diff`/`spawn_export` 及其测试。`--codeweb-bin` 参数与 `SPINDUCK_CODEWEB` 读取同步删除（彻底收口，设计文档 §4.5 推荐项；**改人类测试断言已列 Ask first 授权清单**）。

**Step 4:** 加 `--analyze`：`compile` 子命令布尔 flag → `EmbeddedCodewebSource { auto_analyze: <flag> }`；`spinduck.toml` 不加配置项（保持 CLI 显式语义，YAGNI）。

**Step 5:** `tests/cli/compile_project.rs` 迁移（设计文档 §4.6 表）：假 shell 脚本删除，改为 tempfile 真实 codeweb 项目四场景——无 store 报 `ENV-0030`；analyze 后改源不 `--analyze` 报 `ENV-0030`；全流程 `--analyze` 成功且计划 `generator = "codeweb 0.9.9"`；`--help` 文本无 `--codeweb-bin` 有 `--analyze`。`tests/cli/usage.rs` 断言同步更新。

**验收:** `cargo test --features cli` 全绿；`grep -rn "CodewebExport\|codeweb-bin\|SPINDUCK_CODEWEB" src/ tests/` 为空。commit：`refactor(plan): switch codeweb seam to embedded source, retire out-of-process export`。

### Task 3.3: `from_thin_graph` 映射（新行为，TDD）

**Files:**
- Modify: `spinduck/src/plan/codeweb/parse.rs`

**Step 1（Red）:** 在 `parse.rs` tests（或 `tests.rs`）写映射单测。构造 `codeweb::embed::ThinGraph`（直接手工构造值，不依赖 codeweb fixture），断言：

```rust
#[test]
fn should_map_thin_graph_to_the_same_closure_as_the_equivalent_json() {
    // 同构数据一份：JSON 字符串（走既有 parse_codeweb_json）与 ThinGraph 值
    // 必须产出可逐字段比较的 CodewebGraph 闭包结果。
    // 覆盖：procedure(+partial)、function、package、table(+explicit/system)、
    // view、mview、sequence、unresolved、builtin；边：direct、dynamic、
    // table_access(modes 全 8 种 + write_kinds)、depends_on、uses_sequence、
    // anchors_on、contains_routine；以及"两端有 Java 节点的边被丢"。
    let json = r#"{...等价 JSON...}"#;
    let thin  = /* 等价 ThinGraph 值 */;
    let a = closure_of(parse_codeweb_json(json).unwrap(), ...);
    let b = closure_of(from_thin_graph(thin), ...);
    assert_eq!(a, b);
}
```

（多条小测试优于一条大测试：每种 kind/edge 一条 `should_map_<x>`。）

**Step 2（Green）:** 实现 `pub(super) fn from_thin_graph(thin: ThinGraph) -> CodewebGraph`：
- `ThinNodeKind` → `parse::Node`：routine 的 `file: PathBuf` → `to_string_lossy` 字符串（`project_rel`/`at_text` 的既有口径吃字符串）；`Package/Table/View/MView/Sequence/Unresolved/Builtin` 直映射。
- **`explicit` 字段处置（审核已定死）**：`walk.rs` 只消费 `system`（`walk.rs:196-197`，过滤 system 与引擎自建对象），**不消费 `explicit`**——映射时丢弃 explicit，注释写明；`ThinGraph` 侧仍保留 explicit（codeweb 语义完整）。
- `ThinEdgeKind` → `EdgeKind`：`AccessModeTag::Write` → `"write"` 文本数组（`walk.rs::access_roles` 口径），其余 tag 同名小写；`write_kinds` 直传。
- **JSON id 语义对齐**：`parse.rs` 用 JSON `id`（非数组下标）做节点键；ThinGraph `id` = `NodeIndex::index()`，二者在投影后集合内一致，映射时直接用 `ThinNodeEntry.id`。

**Step 3:** `cargo test --all-features -- plan::codeweb` → 绿。commit：`feat(plan): map embedded ThinGraph onto the internal codeweb graph model`。

### Task 3.4: `EmbeddedCodewebSource`（生产实现，TDD，进程内真实 codeweb）

**Files:**
- Modify: `spinduck/src/plan/codeweb/source.rs`

实现（照设计文档 §4.2，`auto_analyze: bool`；错误映射照 §4.4 表）：

```rust
pub struct EmbeddedCodewebSource { pub auto_analyze: bool }

impl CodewebSource for EmbeddedCodewebSource {
    fn generator(&self) -> String { format!("codeweb {}", codeweb::embed::version()) }
    fn load_graph(&self, root: &Path) -> Result<CodewebGraph, EngineError> {
        let mut proj = codeweb::embed::EmbeddedProject::open_exact(root).map_err(map_cw)?;
        let diff = proj.diff().map_err(map_cw)?;
        if !diff.is_up_to_date() {
            if !self.auto_analyze {
                return Err(EngineError::env(30, format_diff_detail(&diff)));
            }
            proj.analyze().map_err(map_cw)?;
        }
        let thin = proj.export_thin_graph().map_err(map_cw)?;
        Ok(super::parse::from_thin_graph(thin))
    }
}
```

**测试（新写，`tests/embedded_codeweb.rs` 或 source.rs 内；tempfile 造真实 codeweb 项目）：**

1. `should_fail_with_env_30_when_store_is_missing`：写 `codeweb.toml` + 1 个 SQL 文件，`auto_analyze=false` → `ENV-0030`，消息含「codeweb 图索引缺失」。
2. `should_fail_with_env_30_when_sources_changed_after_analyze`：先 `EmbeddedProject{auto_analyze:true}` 跑一遍（或测试内手动 `proj.analyze()`）建 store，再改 SQL 文件，`auto_analyze=false` → `ENV-0030` 含「codeweb 图与源码不一致」。
3. `should_generate_plan_with_auto_analyze_and_versioned_generator`：同场景 `auto_analyze=true` → `load_graph` 成功，且 `generator() == "codeweb 0.9.9"`。
4. `should_fail_with_env_30_when_project_toml_is_absent`：空目录 → 消息含「未找到 codeweb 项目」（与 `codeweb_project_root` 现文案一致）。
5. `should_report_business_150_when_entry_absent`（既有 plans.rs 测试已覆盖该语义，此处仅确认嵌入式路径同样走到 `match_entry`——可不新写，由既有测试迁移覆盖）。

注意：`open_exact` vs 向上查找——`codeweb_project_root`（plans.rs:45）已实现向上查找并把结果传进来，嵌入式实现用 `open_exact`；`EmbeddedProject::find` 本轮不用（避免两套查找逻辑漂移），在 `EmbeddedCodewebSource` 文档注释写明。

### Task 3.6: SpinDuck 版本号 bump（审核补漏项）

`spinduck/Cargo.toml` version `0.1.20 → 0.1.21` 随本 PR 落地（AGENTS §9 发版顺序硬约束：版本号 bump 随 PR，不随 tag 补）。若按 Task 3.8 的 zigbuild 冒烟自测发版，tag 只能打在本 PR 之后的 CI 已验证 main 上。

### Task 3.7: generator 断言与复现性证据迁移

- `grep -rn 'codeweb 0.9.7' src/ tests/` 逐处（`plans.rs:239` 等）改为 `codeweb 0.9.9`（Ask first 授权清单内；PR 描述逐条列 diff）。
- validation.md 新增条目：计划文件 generator 字段变化 `codeweb 0.9.7 → codeweb 0.9.9` 的 diff 含义（嵌入后 generator 由链接的 codeweb 版本决定，未来随 codeweb 发版自动变化——这正是消除版本漂移的机制本身）；同输入同字节的复现性口径不变（同一次构建内）。
- 新增守护测试：`should_write_a_semver_generator_from_the_embedded_source`（断言 `^codeweb \d+\.\d+\.\d+$`），防 generator 字面量倒退回硬编码。

### Task 3.8: 文档同步

**Files:**
- `docs/design-production-gaussdb.md`：D4 决策记录追加「嵌入替代」决议段（四条阻塞的解除证据逐条引用：①lib 拆分 PR、②LICENSE 提交、③v0.11.0 tag = 子模块 pin、④builder.rs:4687 门控修复；契约从 stderr 哨兵/版本词 → `codeweb::embed` 结构化 API）
- `docs/architecture.md` §13.5 口径表：generator、codeweb 接入形态
- `docs/embeded-codeweb.md`：状态 Draft → Implemented（附各 PR/tag 链接）
- `docs/validation.md`：V 编号条目记录本计划全部实测（基线、cargo tree 单副本、嵌入后测试、Phase 4 的内存/耗时实测）
- CI workflow 注释：codeweb git dep 的 audit 豁免登记（同 gaussdb 格式）

### Task 3.9: 全量门禁与发版矩阵冒烟

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings
cargo test --features cli
cargo audit   # codeweb 为 git dep，应不在报告面；若出现，按 gaussdb 豁免格式登记
# 发版冒烟（§8；需 zig 与 cargo-zigbuild ≥ 0.23.4）
cargo zigbuild --release --locked --features cli --target x86_64-unknown-linux-gnu.2.28
python3 scripts/ci/check_max_glibc.py target/x86_64-unknown-linux-gnu/release/spinduck
```

Windows x86_64 冒烟依赖 CI（release.yml 触发或 workflow_dispatch），记录运行链接。

---

# Phase 4：嵌入后的优化（独立批次，逐项带数据立项）

> 每项先测量、后决策，全部产出进 `docs/validation.md`。本 Phase 开始前 Phase 1–3 已合入。

### Task 4.1: 内存峰值实测与处置决策

**动机**：嵌入后 `load_store` 会把完整 GraphStore（真实语料 456 MB bincode，含 columns/body_sql/ddl_source）物化进 spinduck 进程再投影丢弃；JSON 路径下 serde 从不物化重字段（`parse.rs:3-4`）。峰值可能不降反升，必须实测。

**Step 1（测量）**：真实语料（或 synthetic 5 万节点 fixture）上跑：
```bash
/usr/bin/time -l cargo run --release --features cli -- compile --dir <语料> --entry <入口> 2>&1 | grep "maximum resident"
```
记录：嵌入前（JSON 管道）vs 嵌入后峰值 RSS、wall time。

**Step 2（决策门）**：
- 峰值增量 < 1 GB 且无 OOM 风险 → 接受，validation.md 记档收口。
- 否则立项 codeweb 侧「加载即投影」：`GraphStore::load_bincode` 增加 `load_bincode_filtered(filter)` 或 store 拆分（重字段独立文件，按需加载）——独立 PR，走两仓门禁。

### Task 4.2: 耗时实测与 `diff` 快路径确认

- 记录 compile 端到端耗时：嵌入 vs JSON 管道基线（同语料）。
- 确认 `diff()` 走 `load_manifest_only()` 快路径（codeweb 已有），spinduck 每个 compile 只调一次 `load_graph`（`plans_from_source` 单次调用，现状保持）。
- 若 diff 指纹扫描成为新瓶颈（14,611 文件），立项：`DiffStatus` 附带 manifest mtime 短路（store 与 manifest 未变且 mtime 未变 → 跳过扫描）。独立 PR。

### Task 4.3: 并发写守卫（文档化先行）

- `auto_analyze` 在 spinduck 进程内写 `.codeweb/store.*`；两个 spinduck 并发 = 双写竞争。
- 本轮处置：architecture.md 记单写者假设 + `--analyze` 文档注明；不加锁文件（新依赖/新行为都超范围）。
- 若生产出现并发场景，立项 advisory lock（无依赖实现：`create_new` 哨兵文件 + 过期检测），独立设计。

### Task 4.4: 依赖面与产物回归记录

- `cargo tree` diff：新增 petgraph/bincode/bitflags/csv/rayon/pathdiff/regex 传递面——确认全纯 Rust、glibc 2.28 上限不受影响（check_max_glibc 冒烟即证据）。
- 记录 release 二进制体积变化、构建时长变化（release.yml artifact 体积对比 v0.1.20）。

### Task 4.5: 锁冲突接入（二期，独立设计文档）

- `codeweb::embed::query_conflicts`（若 Phase 2 未做则此处连同 codeweb PR 一起）→ SpinDuck `deploy`/`plan` 消费 `LockConflictView`。**先写设计文档再动手**，不在本计划范围。

---

## 风险与回退

| 风险 | 缓解 | 回退 |
|---|---|---|
| v0.11.0 升级改变 codeweb 图抽取语义 | 866 个既有测试为守护面；语义变化逐条归因 ogsql-parser | PR-A 独立可 revert |
| ogsql-parser 双副本（patch 失效） | Task 3.1 `cargo tree -i` 硬断言 | 停在 3.1，不带病前进 |
| analyze-only 下 codeweb 编译失败面大 | Task 2.3/2.4 分批收敛，`full` 与 `analyze` 两态都进每批验收 | PR-B 按 commit 粒度回退 |
| generator 变化破坏计划复现性证据 | Task 3.6 显式迁移 + validation.md 记档 | — |
| 嵌入后内存峰值升高 | Task 4.1 先测后决策；JSON 夹具测试路径保留（`parse_codeweb_json` 不删） | spinduck PR 整体 revert 即回到进程外契约 |
| codeweb v0.9.9 tag 后又改动 embed API | SpinDuck 用 `tag = "v0.9.9"` 精确 pin（同 AGENTS 版本纪律） | — |

## 执行顺序总结

```
Phase 1 (codeweb PR-A)  ──┐
                          ├── codeweb tag v0.9.9
Phase 2 (codeweb PR-B/C) ─┘        │
                                   v
Phase 3 (spinduck, 8 tasks) ──> 合入 main ──> Phase 4 (4.1/4.2 实测 → 4.3/4.4 记档 → 4.5 另立设计)
```
