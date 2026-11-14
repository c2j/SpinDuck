# SpinDuck 进程内嵌入 codeweb 设计文档

> 状态：**Implemented（2026-11-14）** — 替代 `docs/design-production-gaussdb.md` D4 进程外契约  
> 适用仓库：`c2j/codeweb`（`v0.9.9`，已发版）与 `c2j/SpinDuck`（`0.1.21`）  
> 前置假设：双方已完成 `ogsql-parser` 版本对齐（`v0.11.0` = 子模块 `13d9baa`）
>
> **落地记录**
> - codeweb `v0.9.9`（tag 打在 `f67b6c1`）：`d9be49c` ogsql-parser v0.11.0 升级 +
>   tree-sitter 0.25 对齐；`c113e17` LICENSE；`a224f50` lib/bin 拆分与 feature 矩阵
>   （`default = [cli, tui, jsp, java]`、`analyze` = 嵌入形态）；`7035a19` 结构化 store
>   错误 + `Project::diff(&self)`；`e202be1` `codeweb::embed` 契约（`EmbeddedProject` /
>   `DiffStatus` / `ThinGraph`）；Build workflow 全绿，Release 资产已发布。
> - SpinDuck `0.1.21`：`7183c51` 依赖 + `[patch]` 单 parser；`f6293cb` `CodewebSource`
>   接缝；`0727d89` `from_thin_graph` 映射 + `EmbeddedCodewebSource`；`5f9b525` 调用点
>   替换与进程外契约退役（`--codeweb-bin` / `SPINDUCK_CODEWEB` 删除、`--analyze` 新增）；
>   `cebd488` 版本 bump + generator 守护测试 + validation V46。
> - 与本文的偏差（以代码为准）：`jsp` feature 蕴含 `java`（JSP 管线复用 ogsql 的 Java
>   抽取）；`ogsql-parser` 与扫描类依赖（rayon/walkdir/globset/blake3/encoding_rs/
>   indicatif）保持非 optional（任何可用形态的核心），`analyze` 是形态标记而非裁剪这些；
>   `DiffStatus.stale_store_version == Some(0)` 表示版本头不可读（legacy/损坏），按最大
>   过期处理。

---

## 1. 背景与目标

### 1.1 现状
目前 SpinDuck 通过**进程外 CLI 子进程契约**调用 `codeweb`（实现位于 `spinduck/src/plan/codeweb/`）：
1. 按 `--codeweb-bin` → `SPINDUCK_CODEWEB` → `PATH` 查找外部 `codeweb` 可执行文件（`export.rs`）。
2. 执行 `codeweb --version`，硬编码校验版本词 `0.9.7`。
3. 执行 `codeweb diff --project <dir>`，通过 stderr 是否精确等于 `"Up to date. No changes detected."` 判断图是否最新。
4. 执行 `codeweb export --format json --project <dir>`，流式读取 stdout 并经 `serde_json` 解析为瘦图 `CodewebGraph`（`parse.rs`），丢弃 `columns`、`sql`、`body`、Java/JSP/mapper 等重字段。
5. 由 SpinDuck 在 `CodewebGraph` 上执行 BFS 闭包展开（`walk.rs::closure_from_graph`），结合本地 DDL 索引生成 `RunPlan`（`plans.rs`）。

### 1.2 嵌入目标
1. **消除外部二进制依赖**：`spinduck` 不再要求用户预先安装并匹配特定版本的 `codeweb` CLI，开箱可用。
2. **消除进程与序列化开销**：大项目（如 4.3 万节点 / 28.6 万边）不再通过管道序列化/反序列化全图 JSON。
3. **契约结构化**：以强类型 Rust API 替代 stderr 哨兵文本匹配和版本号字符串解析。
4. **保持边界清晰**：
   - `codeweb` 负责：项目管理、源码指纹 `diff`、增量/全量 `analyze`、图存储读取、瘦图投影（`ThinGraph`）与锁冲突查询。
   - `SpinDuck` 负责：目录键规范化、DDL 索引映射、`closure_from_graph` 闭包语义、`RunPlan` 落盘与 `ENG-*` IR 体检。

---

## 2. 架构与依赖边界

```text
+-------------------------------------------------------------------+
| SpinDuck (spinduck-core)                                          |
|  - cli / plan::closure / deploy / call / verify / engine          |
|  - plan::codeweb::CodewebSource (抽象接缝)                         |
|      ├── EmbeddedCodewebSource (生产默认，进程内调用)                |
|      └── JsonFixtureSource     (#[cfg(test)] 内存 JSON 夹具)       |
+-------------------------------------------------------------------+
                                  |
                                  | Cargo dep (default-features = false)
                                  v
+-------------------------------------------------------------------+
| codeweb (lib crate: codeweb::embed)                               |
|  - 裁剪掉 cli / tui / serve / mcp / jsp / java 重依赖              |
|  - 暴露 EmbeddedProject / DiffStatus / ThinGraph / LockConflict   |
|  - 与 SpinDuck 共享同一份 ogsql-parser (v0.11.x)                   |
+-------------------------------------------------------------------+
```

### 2.1 前置工程约束
1. **单一 `ogsql-parser` 版本**：在 `spinduck/Cargo.toml` 引入 `codeweb` 时，通过 `[patch]` 或统一 git/path 源保证进程内只链接一份 `ogsql-parser`（`v0.11.x`），防止规则探针语义分裂。
2. **License 补齐**：`codeweb` 仓库根目录补充 `LICENSE`（与 SpinDuck 的 `MIT OR Apache-2.0` 兼容），并在 `codeweb/Cargo.toml` 声明 `license` 字段，满足 SpinDuck CI 的 `cargo audit` / 合规门禁。
3. **保持单 crate 构建**：遵循 `SpinDuck/AGENTS.md §1`，SpinDuck 侧不引入 `[workspace]`。
4. **跨平台构建兼容**：`codeweb` 库模式不得引入会破坏 SpinDuck 发布矩阵（`x86_64-unknown-linux-gnu.2.28`、`aarch64-unknown-linux-gnu.2.28`、`x86_64-pc-windows-msvc`）的本地 C 依赖或高于 glibc 2.28 的符号。

---

## 3. `codeweb` 侧改造与开放接口设计

### 3.1 Crate 结构与 Feature 裁剪

在 `codeweb/Cargo.toml` 中拆分 `[lib]` 与 `[[bin]]`：

```toml
[package]
name = "codeweb"
version = "0.9.8" # 或下一发版号
edition = "2021"
license = "MIT OR Apache-2.0"

[lib]
name = "codeweb"
path = "src/lib.rs"

[[bin]]
name = "codeweb"
path = "src/main.rs"
required-features = ["cli"]

[features]
# 独立使用 codeweb CLI 时默认行为不变
default = ["cli", "tui", "jsp", "java", "analyze"]

cli = ["dep:clap", "dep:indicatif"]
tui = ["dep:ratatui", "dep:crossterm"]
serve = ["dep:axum", "dep:chrono", "dep:tokio", "dep:tower-http", "dep:rust-embed", "dep:mime_guess"]
mcp = ["dep:rmcp", "dep:schemars", "dep:tokio"]
jsp = []
java = ["dep:tree-sitter", "dep:tree-sitter-java"]
analyze = ["dep:ogsql-parser", "dep:rayon", "dep:walkdir", "dep:globset", "dep:encoding_rs", "dep:blake3"]
search-sql-v2 = []
```

依赖清理要求：
- 将 `tree-sitter`、`tree-sitter-java`、`indicatif` 设为 `optional = true`，纳入各自 feature；SpinDuck 嵌入时使用 `default-features = false, features = ["analyze"]`。
- 将 `rust_i18n`（`i18n!("locales", fallback = "en")`）或仅在 CLI/TUI 中使用的文本输出与核心图逻辑隔离；库路径（`src/lib.rs` 可达模块）**禁止**调用 `std::process::exit`、直接写 `stdout`/`stderr`，进度与诊断通过返回值或可选回调传递。
- 若后续有纯只读查询场景（不触发 `analyze`），甚至允许 SpinDuck 仅开启 `default-features = false`，进一步缩小编译依赖面。

### 3.2 新增模块 `codeweb::embed`（对外稳定契约）

不直接暴露内部 `petgraph::DiGraph<Node, Edge>`、`GraphStore`（当前 `STORE_VERSION = 15`）或 `ColumnSummary` / `body_sql` 等易变结构，而是在 `src/lib.rs` 导出 `pub mod embed;` 作为唯一对外稳定接口面。

#### 3.2.1 版本与错误类型

```rust
/// 返回 codeweb crate 版本字符串（例如 "0.9.8"），替代 CLI `--version` 输出解析
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
```

在 `codeweb/src/error.rs` 的 `CodeWebError` 中补充显式存储状态错误（当前 `load_store` 缺文件复用的是 `ExportError` 文本）：
```rust
pub enum CodeWebError {
    // ... 保留现有 FileRead / NoFilesFound / ExportError / ProjectNotFound / ConfigError ...
    #[error("store not found at {path}")]
    StoreNotFound { path: PathBuf },

    #[error("store version mismatch at {path}: found v{found}, expected v{expected}")]
    StoreVersionMismatch {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}
```

#### 3.2.2 项目句柄 `EmbeddedProject`

```rust
pub struct EmbeddedProject {
    inner: crate::project::Project,
}

impl EmbeddedProject {
    /// 从起始目录向上查找 codeweb.toml 并加载项目
    pub fn find(start_dir: &Path) -> Result<Self, CodeWebError>;

    /// 仅检查指定目录本身是否存在 codeweb.toml（供 SpinDuck 显式指定 --codeweb-project 时使用）
    pub fn open_exact(project_root: &Path) -> Result<Self, CodeWebError>;

    pub fn root(&self) -> &Path;
    pub fn name(&self) -> &str;

    /// 结构化源码新鲜度检查（替代 `codeweb diff` + stderr 哨兵匹配）
    pub fn diff(&self) -> Result<DiffStatus, CodeWebError>;

    /// 全量或增量分析并落盘 store（需开启 `analyze` feature）
    #[cfg(feature = "analyze")]
    pub fn analyze(&mut self) -> Result<AnalyzeReport, CodeWebError>;

    /// 加载 store（若未加载）并投影为瘦图
    /// 投影时仅保留 SQL/PLSQL 闭关所需的节点与边，不拷贝 columns / body_sql / Java / JSP
    pub fn export_thin_graph(&mut self) -> Result<ThinGraph, CodeWebError>;

    /// 二期预留：查询全局或指定严重度以上的表锁冲突
    pub fn query_conflicts(
        &mut self,
        min_severity: ConflictSeverity,
    ) -> Result<Vec<LockConflictView>, CodeWebError>;
}
```

> 实现注意：`Project::diff(&mut self)` 当前只需要读取配置与 manifest sidecar，建议将其方法签名收窄为 `&self`（或仅加载 manifest，不改动 `self.store`）。

#### 3.2.3 新鲜度状态 `DiffStatus`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStatus {
    /// `.codeweb/store.*` 是否存在
    pub store_exists: bool,
    /// 若 store 存在但版本过期，返回旧版本号；否则为 None
    pub stale_store_version: Option<u32>,
    pub added: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

impl DiffStatus {
    pub fn is_up_to_date(&self) -> bool {
        self.store_exists
            && self.stale_store_version.is_none()
            && self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
    }
}
```

#### 3.2.4 瘦图模型 `ThinGraph`

语义与 `codeweb/src/export/json.rs` 及 `SpinDuck/spinduck/src/plan/codeweb/parse.rs` 保持 **1:1 等价**：

```rust
#[derive(Debug, Clone)]
pub struct ThinGraph {
    pub nodes: Vec<ThinNodeEntry>,
    pub edges: Vec<ThinEdge>,
}

#[derive(Debug, Clone)]
pub struct ThinNodeEntry {
    pub id: usize, // NodeIndex::index()
    pub kind: ThinNodeKind,
}

#[derive(Debug, Clone)]
pub enum ThinNodeKind {
    Procedure {
        schema: Option<String>,
        package: Option<String>,
        name: String,
        file: PathBuf,
        line: usize,
        partial: bool,
    },
    Function {
        schema: Option<String>,
        package: Option<String>,
        name: String,
        file: PathBuf,
        line: usize,
        partial: bool,
    },
    Package {
        schema: Option<String>,
        name: String,
    },
    Table {
        schema: Option<String>,
        name: String,
        explicit: bool,
        system: bool,
    },
    View {
        schema: Option<String>,
        name: String,
        explicit: bool,
        system: bool,
    },
    MaterializedView {
        schema: Option<String>,
        name: String,
    },
    Sequence {
        schema: Option<String>,
        name: String,
        explicit: bool,
    },
    Unresolved {
        raw_expr: String,
    },
    BuiltinFunction,
}

#[derive(Debug, Clone)]
pub struct ThinEdge {
    pub source: usize,
    pub target: usize,
    pub kind: ThinEdgeKind,
}

#[derive(Debug, Clone)]
pub enum ThinEdgeKind {
    Direct { file: PathBuf, line: usize },
    Dynamic { raw_expr: String, file: PathBuf, line: usize },
    TableAccess {
        modes: Vec<AccessModeTag>,   // Read | Write | LockRead | AccessExclusive | ShareUpdateExclusive | Share | ShareRowExclusive | Exclusive
        write_kinds: Vec<String>,   // 与 write_kind_label 一致（"insert", "update", ...）
        file: PathBuf,
        line: usize,
    },
    DependsOn { file: PathBuf, line: usize },
    UsesSequence { file: PathBuf, line: usize },
    AnchorsOn { file: PathBuf, line: usize },
    ContainsRoutine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessModeTag {
    Read,
    Write,
    LockRead,
    AccessExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
}
```

投影过滤规则（在 `export_thin_graph` 内执行）：
1. **节点过滤**：跳过 `JavaClass`、`JavaMethod`、`JavaSql`、`MappedStatement`、`Trigger`、`Type`、`Index`、`Synonym`、`Event`、`Custom`、`JspPage`、`JspSql`。
2. **边过滤**：仅投影上述 7 类 `ThinEdgeKind`（对应原 `parse.rs` 保留的边集合）；`source`/`target` 不在保留节点集中的边直接跳过（但 `Dynamic` 边的 `target` 在 `codeweb` 中指向发出该动态调用的例程自身，保留）。
3. **`AccessMode` 分解**：将 `bitflags` 按位展开为 `Vec<AccessModeTag>`，使 SpinDuck 无需直接依赖 `bitflags` crate 或绑定 `codeweb` 内部位掩码定义。

#### 3.2.5 二期预留：锁冲突视图 `LockConflictView`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictSeverity {
    Medium,
    High,
}

#[derive(Debug, Clone)]
pub struct LockConflictView {
    pub table_key: String,
    pub proc_a_key: String,
    pub proc_b_key: String,
    pub modes_a: Vec<AccessModeTag>,
    pub modes_b: Vec<AccessModeTag>,
    pub severity: ConflictSeverity,
}
```

---

## 4. `SpinDuck` 侧改造设计

SpinDuck 的 `walk.rs`（闭包展开）、`plans.rs`（`RunPlan` 组装）、`plan::closure::build` 与后续的 `deploy` / `call` / `compile` 均无需重写核心算法，只需替换取图接缝与 CLI 入口。

### 4.1 `spinduck/Cargo.toml` 调整
- 引入 `codeweb`：
  ```toml
  codeweb = { git = "https://github.com/c2j/codeweb", tag = "v0.9.8", default-features = false, features = ["analyze"] }
  ```
- 若 `codeweb` 通过 `git` 引用 `ogsql-parser`，在 `spinduck/Cargo.toml` 增加 `[patch]`（仅用于局部覆盖，注意 `AGENTS.md §1` 禁止 `[workspace]`），或由 `codeweb` 发版 tag 直接与 `../lib/ogsql-parser` 保持相同提交/semver。
- 检查 `cargo audit` 与 `release.yml` 缓存/子模块步骤是否需要放行该 git 依赖。

### 4.2 抽象接缝替换：`CodewebExport` → `CodewebSource`

位置：`spinduck/src/plan/codeweb/mod.rs`、`export.rs`（重命名或重构为 `source.rs`）。

```rust
/// 从 codeweb 获取瘦图与生成器版本。
/// 生产实现走进程内嵌入式 codeweb；测试夹具仍可注入内存 JSON。
pub trait CodewebSource {
    fn generator(&self) -> String;
    fn load_graph(&self, codeweb_root: &Path) -> Result<CodewebGraph, EngineError>;
}
```

#### 实现 1：`EmbeddedCodewebSource`（生产默认）
```rust
pub struct EmbeddedCodewebSource {
    pub auto_analyze: bool,
}

impl CodewebSource for EmbeddedCodewebSource {
    fn generator(&self) -> String {
        format!("codeweb {}", codeweb::embed::version())
    }

    fn load_graph(&self, codeweb_root: &Path) -> Result<CodewebGraph, EngineError> {
        let mut proj = codeweb::embed::EmbeddedProject::open_exact(codeweb_root)
            .map_err(map_codeweb_err)?;

        let diff = proj.diff().map_err(map_codeweb_err)?;
        if !diff.is_up_to_date() {
            if self.auto_analyze {
                proj.analyze().map_err(map_codeweb_err)?;
            } else {
                return Err(EngineError::env(
                    30,
                    format_diff_err(&diff),
                ));
            }
        }

        let thin = proj.export_thin_graph().map_err(map_codeweb_err)?;
        Ok(super::parse::from_thin_graph(thin))
    }
}
```

#### 实现 2：`JsonCodewebSource`（`#[cfg(test)]` 夹具适配器）
用于无缝承接现有 `tests/cli.rs`、`tests/verify_chain.rs`、`plan/codeweb/plans.rs` 中的 `FakeExport` / `JsonExport` 测试：
```rust
#[cfg(test)]
pub struct JsonCodewebSource {
    pub generator: String,
    pub json: Vec<u8>,
}
```

### 4.3 `parse.rs` / `plans.rs` / `walk.rs` 调整细节

| 文件 | 改动内容 | 说明 |
|---|---|---|
| `plan/codeweb/parse.rs` | 新增 `pub(super) fn from_thin_graph(thin: ThinGraph) -> CodewebGraph` | 直接将 `ThinNodeKind`/`ThinEdgeKind` 转为 `parse::Node`/`EdgeKind`；路径 `PathBuf` 转 UTF-8 字符串供 `project_rel`/`at_text` 使用；`AccessModeTag::Write` 映射为 `"write"` 文本以完全复用 `walk.rs::access_roles` |
| `plan/codeweb/parse.rs` | 保留 `parse_codeweb_json` / `parse_codeweb_reader`（可仅限测试或 `#[cfg(test)]`） | 现有大量单元测试直接传入 JSON 字符串，无需重写夹具 |
| `plan/codeweb/plans.rs` | `plans_from_export` 重命名为 `plans_from_source(..., source: &impl CodewebSource)` | 删除硬编码 `const GENERATOR: &str = "codeweb 0.9.7"`，改为调用 `source.generator()` 写入 `RunPlan.generator` |
| `plan/codeweb/walk.rs` | **无改动** | `closure_from_graph`、`catalog_keys`、`match_entry`、`ensure_entry_under_root` 口径 100% 保留 |
| `plan/closure.rs` | `build(..., source: &impl CodewebSource)` | 参数类型由 `&impl CodewebExport` 切换为 `&impl CodewebSource` |
| `cli/run/call.rs` | **无改动** | 第 219 行已使用 `plan.generator.starts_with("codeweb ")`，天然兼容 `0.9.8+` |

> **关键不变量核对**：`walk.rs::catalog_keys` 目前通过 `Edge::ContainsRoutine` 还原包成员名（并在多包端点时报错 `"codeweb 图不自洽"`）。`ThinGraph` 保留 `ContainsRoutine` 边，因此该校验逻辑原样生效，不破坏已有守护测试。

### 4.4 错误码映射表

保持 SpinDuck 既有错误域（`ENV-0030` / `BIZ-0150`）与退出码（`1`）不变：

| `codeweb` 返回值 / 错误 | SpinDuck 映射 | 语义说明 |
|---|---|---|
| `CodeWebError::ProjectNotFound` | `EngineError::env(30, "未找到 codeweb 项目: ...")` | 与现行 `codeweb_project_root` 一致 |
| `DiffStatus::is_up_to_date() == false` 且未开 `auto_analyze` | `EngineError::env(30, "codeweb 图与源码不一致: ...")` | 提示增量变更文件数或提示运行分析 |
| `CodeWebError::StoreNotFound` / `StoreVersionMismatch` | `EngineError::env(30, "codeweb 图索引缺失或版本过期: ...")` | 禁止无图回退到词法 `walk_chain` |
| `CodeWebError::FileRead` / `ConfigError` / `ExportError` | `EngineError::env(30, "codeweb 加载失败: ...")` | 替换原 `codeweb export 失败:` |
| 目标例程不在 `CodewebGraph` 中 | `EngineError::business(150, ...)` | 仍由 `walk.rs::match_entry` 判定 |

### 4.5 CLI 参数与行为调整

涉及文件：`cli/args/mod.rs`、`cli/args/compile.rs`、`cli/run/mod.rs`、`cli/run/compile_project.rs`。

1. **保留 `--codeweb-project <dir>`**：仍用于覆盖 `spinduck.toml` `[paths].codeweb` 及向上查找 `codeweb.toml` 的默认行为。
2. **`--codeweb-bin` 与 `SPINDUCK_CODEWEB` 退场**：
   - **彻底收口（推荐）**：移除 `--codeweb-bin` 参数、`SPINDUCK_CODEWEB` 读取及 `resolve_codeweb_bin` / `CommandExport` 子进程模块，避免“内嵌/外调双实现长期并存漂移”。
   - 若外部脚本短期内仍会传入 `--codeweb-bin`，也可先保留 CLI 解析为一个版本的空操作弃用警告（printing deprecation notice to stderr），随后下个版本移除。
3. **新增 `--analyze` 参数（可选增强）**：
   - 对应 `EmbeddedCodewebSource { auto_analyze: true }`。
   - 默认仍保持 `auto_analyze = false`（仅只读校验 `diff`，保持与原有严格检查语义一致）；当用户显式传 `spinduck compile --analyze ...` 时，若 `diff` 过期或尚无 `.codeweb/store`，自动在进程内完成增量分析后再生成 `RunPlan`。

### 4.6 测试矩阵迁移

| 测试模块 | 现行测试内容 | 改造后方案 |
|---|---|---|
| `plan/codeweb/tests.rs`、`plan/codeweb/plans.rs` | 使用 `parse_codeweb_json` / `FakeExport` 验证闭包、缺件、排序、去重 | 改为 `JsonCodewebSource` 适配器，**100% 保留原有断言**；新增 `from_thin_graph` 映射单测 |
| `tests/cli.rs`、`tests/verify_chain.rs` | 使用 `JsonExport` 桩验证 `compile` / `call --plan` 链路 | 替换 trait 实现为 `CodewebSource`，断言不变 |
| `tests/cli/compile_project.rs` | 构造假 `codeweb` shell 脚本测 `--version` / `diff` / `export` 失败 | 移除假脚本，改为创建临时真实 `codeweb.toml` + `.sql` 项目：测无 store 报 `ENV-0030`、修改源码未 analyze 报 `ENV-0030`、完整 `analyze` 后正常生成 `generator = "codeweb <ver>"` 计划、带 `--analyze` 自动修复并成功 |
| `tests/cli/usage.rs`、`tests/cli/compile_entry.rs` | 检查 `--codeweb-bin` 用法文本与无 `codeweb.toml` 报错 | 更新 usage 断言（移除 `--codeweb-bin`，如有新增则校验 `--analyze`）；无 `codeweb.toml` 报 `"未找到 codeweb 项目"` 保持不变 |

---

## 5. 分阶段实施清单

### Phase 1：`codeweb` 仓库库化与接口暴露
1. 补齐 `LICENSE` 与 `Cargo.toml` `license` 字段；对齐 `ogsql-parser` 至 `v0.11.x`。
2. 拆分 `src/lib.rs` 与 `[[bin]]`，整理 feature 门控，验证 `cargo check --no-default-features --features analyze`。
3. 实现 `codeweb::embed`（`version`、`EmbeddedProject`、`DiffStatus`、`ThinGraph`、`LockConflictView`）及单元测试，验证 `ThinGraph` 与 `export::json::to_json` 语义等价。

### Phase 2：`SpinDuck` 仓库接入与替换
1. `spinduck/Cargo.toml` 引入 `codeweb`（`default-features = false, features = ["analyze"]`）。
2. 在 `spinduck/src/plan/codeweb/` 用 `CodewebSource` + `EmbeddedCodewebSource` + `from_thin_graph` 替换 `CodewebExport` / `CommandExport`。
3. 清理 `cli/args/` 与 `cli/run/compile_project.rs` 的外部二进制路径（按需加入 `--analyze`）。
4. 迁移 `tests/cli/compile_project.rs` 与 `tests/cli/usage.rs`，更新 `docs/design-production-gaussdb.md` D4 决策记录。
5. 全量门禁验证：
   - `cargo fmt --all -- --check`
   - `cargo clippy --all-features -- -D warnings`
   - `cargo test --features cli`
   - `cargo audit`
   - 平台交叉构建冒烟（Linux x86_64/aarch64 glibc 2.28、Windows x86_64）。

