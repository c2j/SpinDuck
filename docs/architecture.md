# SpinDuck 架构设计：基于 DuckDB 内核的 GaussDB 存储过程引擎

> 定位：独立 Rust 引擎。ogsql-parser 做前端（GaussDB 方言解析），Rust 实现
> AST → 内部过程表示（IR）→ 树遍历解释器；逻辑/控制流/变量/存过内 SQL 改写全部由
> Rust 完成，具体 SQL 执行下沉给 DuckDB（`duckdb` Rust crate）。>
>
> **I/O 定位（V1）**：SpinDuck 处理"存储过程逻辑"，数据输入/输出锁定 GaussDB——
> 处理前将高斯数据表**物化**进本地 DuckDB，处理完成后将结果**回写**高斯
> （Extract → Materialize → Process → Sync）。V1 不接入其他数据源。
>
> 设计原则：**机制进本文档，规则与案例进数据/验证文档**。方言改写规则一律以
> 数据文件维护（[config/rewrite-rules.toml](../config/rewrite-rules.toml)），
> 实证过程与案例见 [validation.md](validation.md)。本文不出现目标语料专属的
> 过程名、字段名或具体规则值。

## 1. 总体架构

```
┌─────────────────────────────────────────────────────────────────┐
│                        SpinDuck Engine (Rust)                    │
│                                                                  │
│  ┌──────────────┐   ┌──────────────┐   ┌───────────────────┐   │
│  │ 前端 Frontend │   │ 编译 Compile │   │ 执行 Execute       │   │
│  │ ogsql-parser  │──▶│ AST → IR     │──▶│ 树遍历解释器        │   │
│  │ 解析 SQL/PL   │   │ 语义标注      │   │ 值/帧/游标/异常     │   │
│  └──────────────┘   └──────┬───────┘   └─────────┬─────────┘   │
│                            │                     │              │
│                     SQL 改写/变量绑定            │ 下发 SQL      │
│                            ▼                     ▼              │
│              ┌─────────────┴───────────────────────┴──────────┐ │
│              │ RuleRegistry (rewrite-rules.toml，数据驱动)      │ │
│              └────────────────┬────────────────────────────────┘ │
│                               ▼                                  │
│                     ┌──────────────────────────────────┐        │
│                     │   SQL Execution Layer             │        │
│                     │   duckdb crate (连接/事务封装)     │        │
│                     └──────────────────────────────────┘        │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ 目录/持久层 RoutineCatalog（过程、函数、PACKAGE 元数据）     │   │
│  │ 存储在 DuckDB 内部表（随数据库持久化，对应用透明）             │   │
│  └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

分层原则：

1. **解释器只知道 IR 和 Value，永远不直接触碰 DuckDB API**；SQL 执行层是唯一
   边界，便于测试（可注入假执行器）与未来替换内核。
2. **改写规则是数据，不是代码**（§9）：方言映射、格式串转换、隐式转换等全部
   外置于规则文件，引擎内只有"规则应用机制"。

## 2. 模块划分（新 crate `spinduck`，workspace 依赖 ogsql-parser）

```
spinduck/
├── src/
│   ├── lib.rs
│   ├── catalog.rs        # 例程目录：CREATE PROCEDURE/FUNCTION/PACKAGE 注册与解析缓存
│   ├── ir/               # 模块装配与对外表面在 mod.rs（§3 规模拆分，单文件 ≤500 行）
│   │   ├── mod.rs        # IR 类型 / 编译入口的再导出
│   │   ├── types.rs      # IrProg / IrStmt / IrExpr / AssignTarget / BindSrc 等 IR 类型
│   │   ├── entry.rs      # 编译入口（compile_*）与 SQL 改写管线
│   │   ├── control.rs    # Compiler 状态：标签、跳转、fixup、嵌套深度护栏
│   │   ├── stmt.rs       # 语句编译
│   │   ├── sql.rs        # SQL 语句 / 赋值目标 / 循环标签
│   │   └── expr.rs       # 表达式编译与原子量
│   ├── runtime/
│   │   ├── value.rs      # SpinValue（DuckDB Value 包装 + NULL + RECORD）
│   │   ├── frame.rs      # 执行帧（变量槽、游标表、OUT 参数、返回值）
│   │   ├── eval.rs       # PL 表达式求值（三值逻辑、NULL 传播、内建/例程分派）
│   │   │   └── numeric.rs# 数值定标与比较（unscaled i128 × 10^-scale）
│   │   ├── cursor.rs     # 游标状态机（open/fetch/close/scroll 有限支持）
│   │   ├── interp.rs     # 树遍历解释器主循环（trap 展开）；语句执行按类拆于同名目录
│   │   │   ├── flow.rs       # 事务/赋值/条件/RETURN/RAISE
│   │   │   ├── sqlstmts.rs   # SELECT INTO / DML / 动态执行
│   │   │   ├── cursors.rs    # 游标 OPEN/FETCH/CLOSE
│   │   │   ├── routine.rs    # CALL / FuncCall 例程直调
│   │   │   ├── dynamic.rs    # EXECUTE IMMEDIATE 语句准备
│   │   │   ├── binds.rs      # 绑定解析
│   │   │   └── trace.rs      # TraceSink 埋点事件构造
│   ├── sql/
│   │   ├── rewrite/      # 改写引擎（机制）：读 RuleRegistry 应用规则
│   │   │   ├── tokens.rs     # token 级变量替换 + [functions] 重命名
│   │   │   ├── scan.rs       # 调用点扫描（括号配平、引号感知）
│   │   │   ├── split.rs      # 顶层切分与字面量引号处理
│   │   │   └── ast.rs        # 解析级变换（DECODE/TO_*/INTERVALTONUM、UPDATE 去别名）
│   │   ├── sqlfn/        # SQL 上下文例程调用下沉（§8.3 路径②）
│   │   ├── render.rs     # IR 表达式 → SQL 文本/绑定（含 bind 收集）
│   │   ├── patterns.rs   # [patterns] 文本形态开关
│   │   └── exec_native.rs# 内核接缝的 DuckDB 实现（query/execute/事务/结果集）
│   ├── exec.rs           # SqlExecutor / TraceSink 接缝 trait（§1 两个接缝）
│   ├── error.rs          # EngineError + 故障域（ENG/KERNEL/BIZ/ENV）与异常匹配
│   ├── project/          # 研发态项目结构（spinduck.toml / *.case.toml 识别）
│   │   ├── mod.rs        # ProjectConfig（项目声明与目录定位）
│   │   ├── case.rs       # CaseConfig（用例声明）
│   │   └── fs.rs         # 配置读取与目录扫描
│   ├── io/
│   │   ├── mod.rs        # DataSource / DataSink trait（§13 接缝）
│   │   └── gauss.rs      # GaussDB 连接器：COPY 批量读（materialize）/ 批量写回（sync_append）
│   └── engine.rs         # 对外入口：Engine::connect / execute_batch / call
├── config/rewrite-rules.toml   # 规则数据（种子 + 项目覆盖）
├── src/bin/
│   └── spinduck.rs       # 研发 CLI（[[bin]]，§2.1；仅壳，零引擎逻辑）
└── tests/
```

**产物形态**：`spinduck-core`（lib，全部引擎能力）+ `spinduck`（CLI binary，
研发工具）。与 ogsql-parser 的"单 crate 多 bin、feature 切分"同构，不拆新 crate。

### 2.1 研发工具 spinduck-cli（运行态的轻量包装）

定位：**开发/调试/验证的命令行入口**，复用 core 公共 API，禁止包含任何引擎
逻辑。调试能力归属 core 的可观测钩子（`TraceSink` trait：解释器与 SQL 执行层
埋点输出结构化事件），cli 只负责渲染（文本表格 / `--json`）。

| 子命令 | 用途 | 复用的 core API |
|---|---|---|
| `init <db>` / `load <dir\|file>` | 建库；批量执行 DDL/脚本（建表/视图/序列，应用规则文件 `[ddl]`） | `Engine::connect` + 批量执行 |
| `register <file>` | 注册过程/函数/PACKAGE：解析→编译→catalog 入库，错误定位到源码行 | 编译器 + catalog |
| `call <proc> [args...]` | 调用过程；OUT 参数/返回值/RAISE 输出；`--trace` 显示解释器事件流 | `Engine::call` + TraceSink |
| `explain <proc>` | **不执行**，展示编译产物：IR 指令序列、每条 SQL 的改写结果与绑定槽位（变量替换是否命中一目了然） | 编译器（只读） |
| `run <sql\|file>` | 直接执行 SQL 脚本（开发期造数/验证） | SQL 执行层 |
| `materialize` | 从 GaussDB 物化数据表进本地 DuckDB（按项目 `[io]` 声明；支持增量） | io::DataSource |
| `sync` | 处理结果回写 GaussDB（append/upsert/分区替换，附校验计数） | io::DataSink |
| `repl` | 交互式会话：混合输入 SQL 与 `CALL`，保留 session/包状态 | Engine 会话 |
| `rules probe` | 跑规则探针测试，输出每条规则 verified/assumed 与内核版本（V6 升级门禁的执行器） | RuleRegistry + 探针套件 |
| `golden <case-config>` | 黄金比对（M10）：按项目配置声明语料/数据/期望输出执行比对 | Engine + 比对器 |
| `assess` | P0 评估：对项目声明语料跑 ogsql 批量解析，产出通过率/特性面/规则缺口报告 | ogsql-parser（委托） |
| `package` | P4 打包：按项目声明产出部署产物（引擎二进制 + conf + 项目规则 + case） | 配置装配 |
| `dump catalog / ir` | 例程目录与 IR 反汇编视图（诊断用） | catalog + IR |

**项目上下文**：所有子命令默认工作在"项目模式"——自当前目录向上搜索
`spinduck.toml`，加载其路径/规则/case 声明；`--db`、`--rules` 等选项为对项目
声明的覆盖。研发态零参数可用，部署态选项显式覆盖。

通用选项：`--db <path>`、`--rules <path>`、`--trace-level <stmt\|sql\|bind\|all>`、
`--timeout`、`--threads`、`--memory-limit`、`--json`、`--dry-run`（explain 语义）。

工程约束：
- CLI 验收走"退出码 + stdout 契约"测试（ogsql AGENTS.md 测试分层同款）
- `--trace` 事件流即 §10.5 可观测性的数据源，core 一次埋点、cli 与生产监控共用
- `golden`/`rules probe` 是 M3.5/M10 门禁的执行入口，验证语料经项目配置声明

TDD 要求（遵循 ogsql-parser AGENTS.md）：每条能力先写失败测试；解释器、改写器
以 trait `SqlExecutor` 为接缝注入假实现单测，DuckDB 集成另设 `tests/` 集成层。
每条规则值必须有探针测试锚定其 `status`（verified/assumed）。

## 3. 关键设计

### 3.1 前端复用（零新增解析工作）

直接使用 ogsql-parser 公共 API：

```rust
let tokens = Tokenizer::new(sql).tokenize()?;
let stmts = Parser::new(tokens).parse()?;   // Vec<Statement>
// Statement::CreateProcedure / CreateFunction / CreatePackage / CreatePackageBody
```

`ast::plpgsql::PlBlock` 已覆盖：变量/常量/游标/RECORD/%TYPE/%ROWTYPE 声明、
IF/CASE/LOOP/WHILE/FOR/FOREACH、EXIT/CONTINUE（带 WHEN 与标签）、GOTO、
RAISE、EXECUTE/PERFORM、OPEN/FETCH/CLOSE/MOVE、GET DIAGNOSTICS、
EXCEPTION WHEN 处理、事务控制、FORALL/PIPE ROW——解释器只需"消费"这套 AST。

### 3.2 IR：扁平化 + 显式栈

不在解释器里直接递归走 AST 控制流。两步：

1. **编译期**：`PlBlock` → `Vec<IrStmt>` 线性指令序列，控制流编译为
   `Jump`/`JumpIf`/`JumpIfNot` + 标签；`EXIT [label] [WHEN c]` 编译为条件跳转；
   循环体内向前跳转到循环顶标签视为 continue。编译产物缓存进 RoutineCatalog
   （连同 AST JSON），调用时免重解析。
2. **运行期**：解释器用**堆上显式栈**管理帧与循环状态，而不是 Rust 调用栈。
   这是支持"表值函数流式返回（RETURN NEXT 提前 LIMIT 终止）"的前提，
   也是异常处理可以 unwind 显式栈帧而非 panic 的前提。

```rust
enum IrStmt {
    DeclareLocal { slot: SlotId, init: Option<IrExpr> },
    Assign { target: AssignTarget, expr: IrExpr },
    Jump(Label), JumpIfNot(IrExpr, Label),
    Raise { level: Level, message: IrExpr },
    ExecSql { stmt: RewrittenSql, binds: BindMap },   // 命名占位符 $v_* + 按名绑定表
    OpenCursor { cursor: SlotId, params: Vec<IrExpr> },
    Fetch { cursor: SlotId, targets: Vec<AssignTarget>, is_for: FetchMode },
    CallRoutine { name: String, args: Vec<IrExpr>, out: Vec<SlotId> },
    Perform(IrExpr),
    Return(Option<IrExpr>), ReturnNext(IrExpr),
    PushFrame(PackageId, String), PopFrame,
    Trap(HandlerTable), Untrap,
    // ...
}
```

**AssignTarget 消歧**（通用规则）：赋值目标在 AST 中是 `ColumnRef` 与记录字段
引用**同形**（`rec.field := x` 与列引用无法靠形状区分）。编译期按帧作用域判定：
首段名为帧内记录变量 → `AssignTarget::RecordField`（就地写回）；否则视为
SQL 侧列引用。改写器与解释器必须共享同一判定，避免两套语义。

### 3.3 值系统

`SpinValue` 直接包装/映射 DuckDB `Value`，另加 PL 特有类型：

| PL 概念 | SpinDuck 表示 |
|---|---|
| 标量/NULL | `duckdb::types::Value`（LogicalType 与 SpinType 一一映射，映射值来自规则文件 `[types]`） |
| RECORD / %ROWTYPE | 有序 `(name, SpinValue)` 列表（来自结果集 schema） |
| 游标 | `CursorState`（见 §7） |
| 集合类型 TABLE OF / VARRAY | `Vec<SpinValue>`（本期不做 index-by 自定义类型） |

**隐式转换**由规则文件 `[implicit]` 驱动。第一阶段只覆盖声明为 `VARCHAR`
（含 `VARCHAR2` / `CHAR` / `CLOB` 等映射到 `VARCHAR` 的名字）的标量槽：
调用装帧与赋值、`SELECT INTO`、`FETCH` 写入时，`assignment_loose = true`
把 `Int` / `Decimal` / `Bool` 收成 `Str`；关闭时这些变体返回 `BIZ-0111`。
`NULL` 保持 `NULL`。`NUMBER` 等数字目标、`DATE` / `TIMESTAMP`、记录字段
与映射表里没有的类型名保持写入前的变体。SQL 绑定只看帧里的 `SpinValue`，
不在改写文本里插入 `CAST`。`coerce` 仍是数值字符串等转换的函数，成功路径
不另打 lint。

### 3.4 存过内 SQL 改写（`sql/rewrite/`，机制层）

对 PL 块内的每条 SQL 语句（`Statement` AST）在编译期做三件事：

1. **变量替换（命名占位符）**：AST Visitor 找出表达式中的 PL 变量引用（先查
   帧作用域，再查 DuckDB catalog 列；记录变量按 §3.2 消歧，`rec.field` 整体
   替换），替换为**命名占位符** `$v_<变量名>`；运行期按名绑定 `SpinValue`。
   选型依据（1.5.5 实测，validation.md V6）：命名参数原生支持、**同一变量多处
   引用只绑定一次**（`?` 则需逐处替换逐个绑定，计数脆弱）、`$v_` 前缀与列名/
   别名无冲突。可读性收益：`explain` 改写产物与 trace 绑定事件自描述，replay
   可按名引用。副作用同 `?`：避免"表达式包成 SELECT 逐次求值"的性能坑，天然
   规避 SQL 注入。
2. **方言降级**：**全部规则值来自 RuleRegistry**（§9），引擎只实现规则应用器：
   - `[functions]` 简单重命名与特殊变换（如 `@case` 型）；
   - `[dates]` 日期函数与格式串转换、日期算术模式；
   - `[patterns]` 语句模式（AST 识别 → 改写动作）；
   - `[rejects]` 不可降级特性 → 明确报错，禁止静默错误翻译。
3. **结果列绑定**：`SELECT ... INTO`、`FETCH INTO` 依据 DuckDB 结果 schema
   顺序写回帧槽位，做可空性与类型宽松转换并产生诊断。
   **SELECT INTO 行数语义**：探测至多 2 行——0 行 → NO_DATA_FOUND、
   ≥2 行 → TOO_MANY_ROWS、1 行 → 绑定；命名条件与 OTHERS 同表匹配。

动态 SQL（`EXECUTE IMMEDIATE`、游标 `OPEN FOR`）：运行期把求值后的 SQL 文本
送回**同一条**"解析→改写→绑定→执行"管线。动态与静态 SQL 禁止出现两套规则表。

**批处理容错语义（V1 必须定义）**：`load` / 批量 `register` / 脚本多语句执行
的失败策略二选一，逐命令显式声明——`fail-fast`（默认：首错即停，报错定位到
语句序号与源码行）/ `collect`（继续执行，汇总全部错误一次报告）。禁止隐式
"跳过失败继续"，防止半成品数据状态被误认为成功。

### 3.4.1 例程间调用（过程 ↔ 过程 / 过程 ↔ 函数）

过程间互相调用与函数调用共用同一帧机制（`CallRoutine` + 显式栈 PushFrame/
PopFrame）。语义要点：

1. **名字解析（编译期，fail-fast）**：不带限定名 → 当前包体 → 同 schema 独立
   例程 → catalog 全局；带 `pkg.proc` 限定 → 直接查 catalog。catalog 缺失即
   编译错误，禁止运行期隐式解析。
2. **OUT/INOUT 跨帧**：实参槽位在编译期登记，被调帧对 OUT 槽位的写回在返回时
   合并进调用帧（同型校验走 §3.3 隐式转换）。
3. **实参省略与形参默认值**：`DEFAULT` 只接受**字面量**（`NULL`/字符串/整数/
   小数/布尔），登记期编译进目录；调用侧省略的尾部实参按声明顺序取默认值，
   未声明 `DEFAULT` 的形参一律 `BIZ-0151` 显式拒绝（不静默补 NULL）。非字面量
   默认值（表达式、`sysdate` 一类）在 v1 登记期即 `BIZ-0203` 拒绝——语料 22 处
   DEFAULT 全为字面量，据此不引入表达式求值面。
4. **异常传播**：被调过程未捕获的异常沿显式栈上抛至调用者 handler；PL 栈回放
   （§10.5/§11.2.2）输出完整帧链。处理器内的裸 `RAISE;` 重抛**当前**异常，
   不因已进入处理器而失效（与 GaussDB 同口径）。
5. **事务标记跨帧**：被调过程体内的 COMMIT/ROLLBACK 同样只是边界标记，归调用
   链顶层统一执行（§3.5 延迟事务）。
6. **递归防护**：帧深度上限 + 编译期调用图检测（复用 ogsql call graph）；
   互相递归在编译期报错，运行期深度超限抛 `ENGINE` 域错误。

验证依据：语料中存在两种真实形态——跨包限定调用（`pkg_a.proc_b(...)`）与
未限定名解析调用（独立过程同名直调）；转发型过程（仅条件调用 + OUT 透传）
作为 golden 必测案例（§12 P3）。

### 3.5 异常处理与事务

- `PlExceptionBlock` 编译为 `Trap(HandlerTable)`：解释器在 SQL 执行或 RAISE
  出错时，将 `duckdb::Error`/自定义错误映射为 `PlError { code, message }`，
  沿显式栈向上找最近 handler 匹配。处理器条件统一为三类：命名条件
  （NO_DATA_FOUND / TOO_MANY_ROWS 等，错误码映射表属规则数据）、
  SQLSTATE、OTHERS 兜底。
- **事务语义**：GaussDB 存过中的 COMMIT/ROLLBACK 嵌在过程体内。设计决策：
  默认**延迟事务模式**——过程体内的 COMMIT/ROLLBACK 记录为事务边界标记，
  在顶层 `CALL` 返回后统一执行；`strict_transactions = false` 选项下允许
  真实提交（文档明示与 GaussDB 的差异）。该差异必须写入兼容性清单。
- **合并 COMMIT 开关**（`Engine::batch_commit`，默认 OFF；V26）：在"过程内
  COMMIT 即时生效"的实现口径上，ON 时过程内 COMMIT 被吞并、INSERT 攒批
  合并（同前缀连续攒入为一组，每组刷盘为一条多值 INSERT；缓冲行数上限
  10,000），缓冲在例程出口统一刷盘、随顶层调用 COMMIT 一起
  持久化。带 sqlfn 下沉（`fn_sites` 非空）或不可字面量化形态（INSERT … SELECT）
  的语句不攒批、回落即时下发（`take_insert` 显式准入条件）。两处显式偏差：
  ① 持久化粒度从行级变为调用级（连接中断不再留半截数据，亦不再逐行落盘）；
  ② 合并语句失败回退逐行重放，失败行按数据
  语义跳过、不进入 PL 异常处理器（inst_log 缺失）。机制细节见
  `runtime/interp/writebuf.rs` 头注释，实证见 validation V26.3/V26.7。

### 3.5.1 参数表点查缓存

循环里的点查（等值、日期 `BETWEEN`、`IN` 列表、同一列上的 `OR`、单列 `IN`
子查询、带 `ORDER BY` 的 `LIMIT`）第一次执行时，由 DuckDB 按不变谓词扫一遍，
结果按等值键放进进程内哈希表，随后的行只做探测。`SELECT INTO` 的 0 行 / 多行
语义不变，结果列名跟 SELECT 列表走。投影里的 `coalesce` 留在构建 SQL 中，由内核计算。

刷新点只有两处：写入都走当前连接的 `SqlExecutor::execute`，调用内没有第二写者。
顶层 `call` 入口清空；`execute` 的标识符命中已缓存表名时丢掉该表的索引，下一次
查询重建，因此能看见本事务刚写入的行。认不出的语句（`CASE`、不等谓词、连接）、
构建失败、排序键并列，或排序值无法在进程内比较，都原样下发。机制见 `src/lookup_cache/`，
实证见 validation V42。

### 3.6 目录与持久化

例程定义存 DuckDB 内部表 `spinduck_routines(name, kind, source, registered_at)`。
好处：随 `.duckdb` 文件持久化、免额外存储、可版本化；避免与 DuckDB 自身
catalog 钩子纠缠（扩展级 catalog 注册在非扩展形态下不可用）。

**实现口径（M7 落地）**：只持久化**可重放的源 DDL 文本**，不存 `ir_blob`/`ast_json`。
理由：IR 是编译产物（含解析后的跳转地址与标签表），一旦编译器演进或版本升级，
旧 IR 的语义不再是"当时的语义"，重放会得到难以诊断的行为；而"源文本 + 当前规则
重新编译"总是产生与当前版本一致的确定结果，且升级路径天然正确（升级 = 重新编译，
失败 = 明确的编译错误而不是运行期怪象）。

- 写入：`Engine::register_persistent(sql)` 在注册成功后按例程名 UPSERT 源文本
  （`ON CONFLICT (name) DO UPDATE`）；包体（PACKAGE BODY）内多个例程共享同一段
  源文本，恢复时按 `SELECT DISTINCT source` 去重，避免重复注册。
- 读取：`Engine::reload_catalog()` 在目录表缺失时返回 0（不是错误——空库首次运行），
  存在时逐条重新注册并返回例程数。
- 版本申报：`registered_at` 仅作审计，不参与一致性判断。

PACKAGE：spec+body 配对校验（ogsql 已有包一致性分析可复用），包级变量放在
`PackageState`（按 session 生命周期管理——与 GaussDB 会话级包状态语义一致，
但需文档说明不跨会话持久）。

## 4. 里程碑（每步一个 TDD 循环）

| M | 内容 | 验收 |
|---|---|---|
| M1 | SQL 执行层 + SpinValue/类型映射（读规则文件）+ 假执行器单测；**锁定 DuckDB 1.5.5 对应 crate** | `SELECT` 往返、规则文件 `[types]` 全部类型通过 |
| M2 | 编译器：PlBlock → IR（无 SQL） | 赋值/IF/循环/GOTO 解释执行，纯 Rust 单测 |
| M3 | 变量替换（命名占位符 + 同名消重）+ SELECT INTO（行数语义）+ PERFORM | 假执行器验证改写产物与按名绑定 |
| M3.5 | 改写规则应用器 + 规则文件加载/覆盖机制；以 V6 探针套件（已在 1.5.5 完成基线，validation.md）做实现回归 | 每条规则一条探针测试；1.5.5 上 assumed 规则清单归零 |
| M4 | DuckDB 集成：真实 SQL 下发、游标状态机（§7） | 集成测试：建表/查询/游标遍历/动态 OPEN |
| M4.5 | 数据接入与回写（§13）：物化（全量+按日期增量）、回写三模式、幂等重跑 | 与真实 GaussDB 环境联调通过（依赖 §12 P3 基线环境） |
| M5 | RAISE + EXCEPTION（含命名条件）+ GET DIAGNOSTICS | 异常表匹配、嵌套块 unwind |
| M6 | CALL 语法、OUT/INOUT、返回标量 | 顶层 API `engine.call("proc", args)` |
| M6.5 | SQL 上下文函数分派（§8）：三类路径各打通 ≥1 例程，UDF 重入与线程安全验证 | 分派矩阵全覆盖 |
| M7 | CREATE FUNCTION/PROCEDURE/PACKAGE 注册 + catalog 持久化 | 重启后 CALL 成功 |
| M8 | 动态 SQL EXECUTE IMMEDIATE + 目标语料批处理 | 验证语料（validation.md V1 语料）通过率报告 |
| M9 | 流式返回（RETURN NEXT / 表函数） | LIMIT 提前终止（FORALL 列二期，见 §6/validation V1） |
| M10 | 黄金比对终验：语料与样例数据由项目配置声明，引擎代码零硬编码；**附冒烟性能基线**（清算样例规模下的单过程耗时上限，数值在 P3 与业务共同确认） | 与 GaussDB 基线输出全量比对 + 性能冒烟达标 |

### 4.1 实施状态（截至本次交付）

| M | 状态 | 证据 |
|---|---|---|
| M1–M6 | ✅ | 逐里程碑提交；内核探针（真实 DuckDB 1.5.5）覆盖游标/异常/分派 |
| M3.5 | ✅ | 规则应用器 + 探针回归；validation V6 |
| M4.5 | ✅ | openGauss 5.0.0 物化→处理→回写→校验；validation V7 |
| M6.5 | ✅ ③ 宏路径 | 表达式级函数登记为内核宏，SQL 上下文可直调；② 内联与 ① UDF 主动推迟（§8.3）；validation V8。本行为 M6.5 当时口径：**② 已于 V15/V21.3 以参数元组下沉落地**，① 仍推迟（§8.3、V23） |
| M7 | ✅ 编译体检 + 目录持久化 | 基线语料 136/136 文件（54 例程）；重载后 CALL 成功；validation V9/V11 |
| M8 | ◐ 静态部分 | `EXECUTE IMMEDIATE '<静态文本>'` 下沉；运行期拼接显式拒绝（注入面）；语料批处理即 `compile` 子命令 |
| M9 | ⛔ 未做 | 语料实证：`RETURN NEXT`/`PIPE ROW` 出现 0 次，集合赋值 3 例（`v_info_tab := …`，同一形态在三份收集件里各计一次）、带参游标 `OPEN` 1 例（编译期显式拒绝）；优先级低于 M10。**拒绝面不含集合元素读取** `v := t(i)`，该形态留待编译期拒绝（V23） |
| M10 | ✅（替代基线） | 黄金用例与 **openGauss 5.0.0** 基线逐行一致；GaussDB 实例不可达，替代依据见 validation V10；**性能冒烟未做**（无生产量级数据） |

未完成项的处置原则：**宁可显式拒绝也不静默错译**（编译期 `ENG-####` 报错），
并在 README「已知限制」与 validation 对应小节登记。

## 5. 主要风险与对策

| 风险 | 对策 |
|---|---|
| duckdb crate API 层次（版本绑定、Arrow 结果提取） | **内核版本锚定 DuckDB 1.5.5**：M1 固定 duckdb-rs crate 版本与之对齐；结果统一走 `Statement::query` + 行迭代，避免依赖不稳定内部类型 |
| `%TYPE` 依赖表 schema，CREATE 顺序敏感 | catalog 提供 schema 解析延迟化：例程在首次调用时完成 %TYPE 解析（lazy bind） |
| GaussDB 语义细节（隐式转换、空串=NULL、NLS 排序） | 全部进规则文件 `[implicit]`，差异显式配置 + 文档化 + 测试 |
| 规则值凭经验假设出错 | 规则必须带探针测试；`assumed` 状态规则在 CI 中告警（validation.md R3 教训） |
| 存过内嵌套调用、递归 | 帧深度上限 + 调用图检测（编译期，可复用 ogsql 的 call graph 能力） |
| 与 DuckDB 原生宏/UDF 生态重叠 | 定位差异化：GaussDB 方言过程体兼容，不做通用过程语言 |

## 6. 明确不做（v1 范围外）

- 触发器（DuckDB 无触发器基础设施）
- AUTHID DEFINER 权限语义（DuckDB 无角色系统）
- 高级滚动游标（SCROLL/ABSOLUTE/FETCH PRIOR 深度功能）
- 数据库链接（DBLINK）、物化包状态跨会话持久
- DISTRIBUTE BY / 分区等分布式 DDL（遇之报错并给出迁移指引）

## 6.1 字符集与时区声明（V1 兼容边界）

- **编码统一 UTF-8（读入面判定，决策 2026-11-10 / D9②，实证 validation V33）**：引擎内部、
  结果、日志、golden 输出一律 UTF-8 不变。SQL **源文件**的读入走 `crate::source::decode`
  的两把严格判定：UTF-8 合法即用，否则按 GBK 解码（`encoding_rs`），两种都不合法即
  `ENV-0030` 判红——不猜第三种编码，也不退回有损解码。原条文"引擎不做运行时编码探测、
  由工具层 `--encoding` 预处理"在 A 阶段作废：交付件 14,611 个 `.sql` 里 3,149 个是 GBK
  （design §4），"先把语料转成 UTF-8"不在承诺范围（语料字节不动，AGENTS §6），故判定只能
  发生在读入时；顺序固定 UTF-8 优先（两把判定同时成立的 5,130 个文件按 UTF-8 解释，
  按 GBK 读必成乱码）。细节与残留风险面见 §13.8。
- **会话时区**：引擎启动/会话级 `timezone` 配置（默认 UTC），映射到 DuckDB
  `SET TimeZone`；`now()`/`CURRENT_DATE` 及 TIMESTAMP WITH TIME ZONE 行为
  依赖此配置。与 GaussDB 会话时区语义的差异（GaussDB 支持会话级动态切换）
  列入兼容性清单；V1 仅支持连接级设置。
- **排序与比较语义**：V1 默认二进制排序（DuckDB 默认），中文拼音/笔画排序、
  大小写不敏感比较（NLS 语义）**不做**（列 §10.9 边界）；涉及中文排序的查询
  由改写器 lint 提示差异。

## 7. 游标子系统设计

> 依据语料中普遍存在的两种形态（实证见 validation.md V2/V3）：静态游标
> （含大结果集 UNION ALL 查询 + 查询列内函数调用）与弱类型 REF CURSOR +
> 运行期拼 SQL 的动态打开。设计必须同时覆盖两类。

### 7.1 两类游标，一条编译管线

| 类型 | 编译期处理 |
|---|---|
| 静态游标 `CURSOR c IS SELECT...` | ogsql 已把查询解析为 `parsed_query: Statement`，走标准改写管线（变量替换→占位符、方言降级、语句模式），产出 `CachedQuery` |
| 弱 REF CURSOR `OPEN c FOR <表达式>` | **无法编译期解析**——`OPEN FOR` 是运行期指令：把求值后的 SQL 字符串送回同一条"解析→改写→绑定→执行"管线。ogsql 解析为微秒级，运行期解析成本可忽略 |

两类最终汇合到同一个运行时状态机，差别只在"查询文本何时确定"。

### 7.2 编译期：游标 → IR

```
PlCursorDecl { name, arguments, return_type, query, parsed_query, scrollable }
        │
        ▼ compile
CursorDef {
  slot: SlotId,                     // 每个游标名占一个帧槽
  query: QuerySource,               // Static(CachedQuery) | Dynamic(内联表达式求值)
  arg_slots: Vec<SlotId>,           // 形参；游标体直接引用的外层变量编译为自由变量捕获
  row_shape: RowShape,              // 以 OPEN 时结果 schema 为准（禁止 AST 静态推导）
  attrs: CursorAttrs,               // %ISOPEN/%FOUND/%NOTFOUND/%ROWCOUNT 访问点
}
```

`FOR rec IN (SELECT ...)`：编译器脱糖为"匿名游标 + OPEN + LOOP{FETCH INTO rec;
EXIT WHEN %NOTFOUND; 循环体} + CLOSE"，复用同一套 IR。

### 7.3 运行期：CursorState 状态机

```rust
enum CursorState {
    Closed,
    Open {
        stream: RowStream,               // DuckDB 行流（不整体物化，逐行拉取）
        schema: Vec<(String, SpinType)>, // OPEN 时锁定；FETCH INTO 依据它写槽
        row_buf: Option<Vec<SpinValue>>, // 当前行的物化副本
        row_count: u64,                  // %ROWCOUNT
        last_fetch_hit: Option<bool>,    // 最近一次 FETCH 是否命中 → %FOUND/%NOTFOUND
    },
}
```

关键决策：

1. **OPEN 即绑定、快照语义**。OPEN 时完成参数求值与查询执行（预编译语句按
   查询文本缓存复用），后续基表修改不影响已打开游标；游标查询引用的外层
   变量在 OPEN 时求值绑定，循环体内改变量不影响游标。
2. **FETCH = 单行拉取 + schema 对位写入**。INTO 目标是 `%ROWTYPE` 记录 →
   按 OPEN 时 schema 逐列构造 Record，**字段名取查询别名**（改写器不得改写
   SELECT 别名，别名即记录字段契约）；INTO 标量列表 → 位置对位宽松转换；
   流结束 → 目标槽位**不覆盖**，`%NOTFOUND` 为真。
3. **游标属性是表达式**：属性读取解析为对槽位 `CursorState` 元数据的访问，
   不需要特殊指令。
4. **流式，不物化**：大结果集逐行拉取，仅留当前行副本；批量优化在此基础上加。
5. **REF CURSOR 动态打开**：运行期求值 SQL → 解析→改写→prepare→execute，
   schema 在 OPEN 后才可知（`RowShape::Deferred`）。
6. **与异常处理的交互**：异常沿显式栈 unwind 到最近的内层 handler；
   "循环内 BEGIN..EXCEPTION GOTO <循环顶标签>"模式即 continue；游标状态挂在
   外层帧，不受内层 unwind 影响；帧销毁时 drop 残留行流（CLOSE 不强制）。
7. **范围限制（v1）**：仅 FORWARD-ONLY；SCROLL 明确报错；REF CURSOR 作
   OUT 参数跨过程传递为二期。

### 7.4 SELECT..FROM dual 表的折叠优化

形如"SELECT <纯 PL 表达式> INTO .. FROM <哑表>"的语句，若表达式无 SQL 侧
依赖（仅变量/记录字段/常量），编译期折叠为解释器内直接求值，省一次 SQL 下发。
判定条件与哑表名单由规则文件维护。

## 8. SQL 上下文 PL 函数的分派矩阵

过程体内函数调用的分派由两个维度决定：**调用上下文**（PL / SQL）×
**函数形态**（表达式级 / 单条 SQL 体 / 过程级）。

### 8.1 分派矩阵

| 形态 \ 上下文 | PL 上下文（赋值右值/条件） | SQL 上下文（出现在下发 SQL 中） |
|---|---|---|
| 表达式级 | 解释器直调 | ③ **宏**：`CREATE MACRO fn(args) AS <改写表达式>`（规则文件提供表达式），SQL 与回调共用 |
| 单条 SQL 体 | 解释器直调 | ② **内联**：函数体 AST → 相关标量子查询展开，进 DuckDB 优化器 |
| 过程级（控制流/多语句/异常） | 解释器直调（含嵌套调用） | ① **Rust 标量 UDF**：duckdb crate `create_scalar_function`，回调内转调解释器（同语言同进程，不引入第二语言） |

PL 上下文一律解释器直调，不经过 DuckDB。SQL 上下文按形态选择能下沉的最快
路径，UDF 仅兜底过程级函数。宏与内联产物随例程注册进 catalog；三类路径可
混合嵌套（UDF 回调内可经连接调用宏）。

### 8.2 UDF 路径的耦合约束

UDF 回调发生在 DuckDB 执行查询的过程中：

1. **连接隔离**：DuckDB 不允许同一连接在查询执行中再发起子查询——回调内的
   SQL 必须走**同一 Database 实例的第二连接**（连接池挂在实例上）。可见性
   约束：函数体内 SQL 只见已提交数据；"UDF 函数 + 同事务先写后读"模式由
   改写器 lint 告警。
2. **线程安全（按执行位置收窄）**：实证表明（validation.md V6）SELECT 列表
   位置的 UDF 在多线程下安全；**过滤/子查询位置的 UDF 回调在并发下失败**——
   该位置的语句下发前强制 threads=1（会话开关）；"Rust UDF 线程安全复测"
   列入 M6.5 验收。
3. **已知性能边界**：行级 UDF 调用为逐行跨边界；内联/宏路径优先以压缩其
   使用范围，长期方案为 UDF 结果缓存/去重。

### 8.3 落地状态（M6.5）

| 路径 | 状态 | 判定依据 |
|---|---|---|
| ③ 宏（表达式级） | **已落地** | 纯表达式函数（单 `RETURN <expr>`、无局部变量、纯 IN 参数、非包限定）在 `register` 时登记为 `CREATE OR REPLACE MACRO`；表达式经 `sql::render` 由 IR 渲染，函数名走 `[functions]` 规则映射，`@` 变换标记拒绝渲染 |
| ② SQL 上下文例程调用 | ✅ 已实现（参数元组缓存化下沉） | `sql/sqlfn/`：编译期按调用点构造 `SELECT DISTINCT <args> [FROM…][WHERE…]`（剔除自身谓词、上下文依赖拓扑、相同调用去重），调用点内联为相关子查询；运行期逐元组调用例程填 `sd_fnN` 临时表。**前提**：例程在语句执行期确定性；元组上界 10 万；上下文互引显式拒绝（BIZ-0160）。**保留命名空间**：下沉产出的表名与列名一律带 `sd_`
前缀（`sd_fnN` / `sd_v` / `sd_a0…`）——管线在调用点替换之后还要跑一次变量绑定，裸 `v`/`a0`
会命中同名 PL 变量并被替换成绑定占位符；`sd_` 前缀在语料里零命中，故不会与项目标识符相撞。
见 validation V15、V21 |
| ① UDF（过程级） | **主动推迟** | DuckDB 禁止同一连接在查询执行中发起子查询（§8.2-1），回调内必须走同 Database 的第二连接；`SqlExecutor` 接缝当前不暴露 Database 句柄，新增能力属接缝签名变更（AGENTS §1 需 Ask first）。口径修订（2026-11-07，validation V23）：SQL 上下文调用过程级函数已由路径② 承接，① 不再是这类调用的唯一出路；内核 Catalog Error 只剩"名字不在例程目录（且未宏化）"一种情形 |

实现纪律：

- 宏登记失败（内核拒绝 DDL）为硬错误，不吞异常；**形态不满足时跳过登记**，
  由内核在调用点报错，错误域归 KERNEL（§11.2.2）。
- 宏体不强制按声明返回类型 `CAST`：SQL 上下文的类型由实参的 DuckDB 类型主导，
  强转会在 `NUMBER` 函数上丢失整型语义并引入精度损失。该偏差登记为已知限制
  （validation.md V8），M8 随内联路径统一评估。

## 9. 兼容性规则的数据化机制

**设计只定义机制，规则值是数据。**

1. **规则文件**：`config/rewrite-rules.toml` 为种子数据（函数映射、日期格式
   与算术模式、类型映射、DDL load 规则、语句模式、隐式转换、拒绝清单），
   结构见该文件注释。
2. **加载与覆盖**：引擎启动加载种子规则；`--rules <path>` 允许项目级覆盖/
   追加。规则变更不改引擎代码。
3. **规则生命周期**：每条规则带 `status`（verified = 有 DuckDB 实测探针；
   assumed = 待验证）。CI 对 `assumed` 规则告警；规则值以实测为准，
   禁止凭其他数据库经验假设（教训：validation.md R3）。
4. **目标场景配置**：验证语料、样例数据、黄金基线输出均由项目配置声明
   （M10），引擎与测试代码零硬编码业务对象。

---

## 10. 企业级能力清单（单机形态，非分布式目标）

> 本节是 v1 之后进入企业环境的增量要求清单。原则不变：机制进设计，参数/策略
> 进配置（§9 规则文件同源管理）；与内核语义相关的项以探针验证为准。

### 10.1 事务与故障一致性（优先级最高）

| 项 | 设计要求 |
|---|---|
| 崩溃恢复语义 | 明确定义延迟事务模式下 CALL 中途崩溃的恢复边界（DuckDB WAL 重放到哪个事务边界）；"步骤级原子性"作为可选模式：过程编译期把语句分组成原子块，块间落延迟事务标记 |
| 长事务治理 | 长 CALL 持有写事务会阻塞 checkpoint → WAL 无界增长。过程级提交点设计 + WAL 大小监控指标；清算类长过程必须显式声明事务边界 |
| 幂等与重跑 | 批处理过程的断点重跑语义（依赖 v_step_no 类步骤锚点），失败恢复从最近提交点继续或全量重跑，二选一显式配置 |

### 10.2 并发与连接模型

- DuckDB 单写多读：并发 CALL 的排队策略（写互斥、读并发）、read_only 附加副本支撑报表负载
- **V1 并发语义收窄**：同一会话内严格串行（延迟事务标记按序合并）；跨会话并发 CALL 仅允许至多一个写会话（DuckDB 单写者约束），并发写 CALL 排队等待——延迟事务标记不做跨会话合并，杜绝标记交叉导致的边界混乱
- 多租户：每租户独立 Database 文件（进程内多实例）为默认；单文件多租户不承诺隔离
- **死锁防护**：UDF 第二连接在外层写事务活跃期间强制只读（补 §8.2 可见性约束），杜绝自锁

### 10.3 资源治理与可取消性

- 引擎配置：memory_limit / temp_directory / threads / 各类超时，全部进配置文件（§9 同源）
- 解释器循环埋设**协作式取消点**：CALL 超时取消、进度上报（语句计数 + 耗时）
- 帧深度/递归上限（已有）+ 单 CALL 内存配额（新增）

### 10.4 安全与审计

- 注入面收口：占位符绑定为主；**动态 OPEN FOR 拼接 SQL 是残余注入面**——改写器对运行期拼接来源做 lint（强制参数化改写），拒绝无法静态改写的拼接模式
- 鉴权：引擎级调用鉴权 + 对象访问 deny 清单（DuckDB 无角色系统，GaussDB AUTHID/RLS 不承诺，见 §6；企业差异全部文档化）
- **审计日志**：调用主体、CALL 参数摘要、捕获的 SQL 与错误码、会话标识；审计开关与输出通道可配置
- 静态加密：DuckDB database encryption（AES-GCM）；密钥来自外部 KMS/环境，不落盘明文

### 10.5 可观测性

- 结构化调用链：过程 → 语句 → 耗时/行数/错误；过程内步骤锚点（如业务代码自带的步骤号变量）自动识别为 trace span
- 指标：CALL 次数/时长分位/错误率、游标行数、SQL 下发计数、UDF 回调计数
- 慢过程报告 + PL 栈回放（异常时输出帧链）；OTel exporter 可选

### 10.6 版本升级与离线部署

- **存储格式兼容是硬约束**：锚定内核版本（§5）后，升级 = 数据导出/导入迁移，不支持原地跨格式升级；升级路径写入运维手册
- **平台矩阵**：Linux x86_64/aarch64（glibc ≥2.28，cargo-zigbuild 锚定符号版本 + 产物侧
  `scripts/ci/check_max_glibc.py` 反查）、macOS aarch64/x86_64、Windows x86_64 + Win7（nightly 专项）。
  发版入口 = `.github/workflows/release.yml`（tag `vX.Y.Z` 触发，见 AGENTS §9 / 决策记录 ⑬），
  **当前只覆盖前两项的 Linux 与 Windows x86_64 共 3 目标**；"duckdb-rs 各平台可用性在 M1 探针
  逐平台验证"是计划而非事实，Linux 两目标已由 V25 实测补上，macOS/Win7 仍无证据
- **离线/内网部署**：禁用扩展 autoinstall 网络，所需扩展静态链接进二进制（含 Win7 交叉编译目标）
- 规则文件与 routine catalog 均带 schema 版本号，升级迁移脚本随版本发布

### 10.7 备份恢复

- 定期 CHECKPOINT + 文件级备份（单写者模型使备份一致性简化）；在线备份走 DuckDB backup API；PITR 能力边界（WAL 保留窗口）显式文档化

### 10.8 工程质量门禁（企业级准入）

- 故障注入：CALL 中途 kill -9 → 验证 WAL 恢复与事务语义（覆盖 10.1）
- 长循环内存泄漏检测（万次游标迭代 RSS 曲线）
- 性能回归基准（模拟器口径扩展为基准集）与黄金比对进 CI
- 升级门禁：规则探针套件（validation.md V6 模式）作为每次内核升级的强制回归

### 10.9 明确的企业级边界（v1 不承诺，文档公示）

- 跨进程/跨机共享同一数据库文件；在线 schema 迁移；细粒度行级权限（RLS）
- 多写者并发；热备/故障自动切换（非分布式目标下以"备份 + 重启恢复"为可用性模型）

---

## 11. 运行态文件布局与日志结构

> 例程编译产物常驻内存解释执行，但持久状态必须落在**受管理的目录结构**内。原则：
> ① 引擎只写 `SPINDUCK_HOME` 内路径（全部可配置）；② 内核托管文件（数据库/WAL）
> 与引擎托管文件（日志/审计/spool）分区；③ 日志全部结构化（JSONL），审计与
> 运行日志分离；④ 命名兼容 Win7（无符号链接、大小写安全）。

### 11.1 目录布局

```
$SPINDUCK_HOME/                       # 根路径，配置项 home；默认 ./（CLI 开发态）
├── conf/
│   ├── engine.toml                   # 引擎配置（threads/memory/limits/路径/日志策略）
│   ├── rewrite-rules.toml            # 规则文件（种子或项目覆盖副本）
│   ├── spinduck.runtime.toml         # 项目声明的运行态快照（`compile --sync-manifest` 生成，见 §12.1）
│   └── conf.d/                       # 项目级覆盖片段（后加载覆盖先加载）
├── data/                             # 内核托管：数据库文件（每库一文件，多租户=多文件，§10.2）
│   ├── <dbname>.duckdb               #   例程 catalog 亦在其内（§3.6）
│   └── <dbname>.wal                  #   DuckDB WAL 由内核强制同目录落放——不可分离
├── spool/                            # 内核托管：temp_directory（内存溢出落盘，§10.3）
│   └── <dbname>/                     #   要求本地高速盘，禁止网络盘
├── logs/
│   ├── engine/                       # 运行日志：spinduck-YYYYMMDD.log(.gz)，按天+大小轮转
│   ├── error/                        # 错误日志：独立通道，含 PL 栈回放（§10.5）
│   ├── audit/                        # 审计日志（§10.4）：append-only JSONL，按天分文件，
│   │                                 #   写入即 fsync 策略可配（at-least-once，禁止静默丢弃）
│   ├── trace/                        # 调用链（§10.5，可选开关）：
│   │   └── <date>/<session_id>/<call_seq>-<proc>.jsonl   # 每次 CALL 一文件，便于定位
│   └── crash/                        # 崩溃现场：CALL 中途 kill 时落盘帧快照 + 步骤锚点
│                                     #   （§10.1 故障恢复诊断的输入，重启后上报）
├── backup/                           # §10.7：checkpoint 导出与备份产物，按 <dbname>/<ts> 分层
└── tmp/                              # 引擎临时文件（探针/ golden 比对中间产物），退出清理
```

布局决策：

1. **数据库文件与 WAL 不可分离**——WAL 路径由 DuckDB 内核固定为同目录 `<db>.wal`，
   备份必须整对拷贝（§10.7），目录设计不再制造分离的假象。
2. **conf 与 data 分区**：规则/配置是代码化资产（随版本发布、可 diff），数据是
   运行时资产（随租户增长）；升级迁移（§10.6）只动 `data/`。
3. **trace 单 CALL 单文件**：`<call_seq>-<proc>.jsonl` 使"定位某次调用"不依赖索引；
   轮转清理策略独立于 engine 日志（trace 体量大）。
4. **crash/ 目录是 10.1 故障一致性的配套**：崩溃时把解释器帧链、当前 IR 指针、
   步骤锚点序列化落盘，恢复后可人工判读断点位置。

### 11.2 日志结构（JSONL 字段契约）

所有结构化日志共用基础字段，通道各加专属字段：

```json
// 公共头（engine/audit/trace/error 全部包含）
{"ts":"2026-11-04T20:30:00.123Z",         // UTC ISO8601，毫秒
 "lvl":"INFO",                             // DEBUG/INFO/WARN/ERROR
 "ch":"engine",                            // 通道：engine|audit|trace|error
 "sid":"s-3f9a…", "cid":"c-000042",        // 会话与调用标识（跨日志关联键）
 "proc":"PKG_X.prc_y", "step":"2.2",       // 过程与步骤锚点（业务代码自带步骤号自动识别）
 "msg":"...", "err":"PL-04001"}            // 错误码来自异常映射表（规则数据）

// audit 专属：{"user":"…","args_digest":"sha256:…","sql_count":12,"duration_ms":1843}
// trace 专属：{"event":"SQL_EXEC|CURSOR_OPEN|FETCH|UDF_CALL|IR_STMT|BATCH_SKIP","detail":{…},"elapsed_us":97}
//   BATCH_SKIP detail 形态（V26，第四轮复审确认并入契约）：
//   {"event":"BATCH_SKIP","detail":{"rows":1,"first_error":"KERNEL-0010 constraint violation"},"elapsed_us":0}
//   ——合并 COMMIT 开关 ON 的回放期被跳过行数与首个错误（含错误码）
//   注意：exec_native 把所有 DuckDB 错误统一映射为 KERNEL-0010，因此该码
//   不承担「引擎缺陷 vs 业务跳行」的判别维度——判别信号是 BatchSkip 事件
//   本身 + first_error 的 message 文本，勿按错误码写告警规则（第五轮复审口径）
// engine 专属：{"component":"interp|exec|catalog|rules","kv":{…}}
```

策略（全部可配置，默认值为企业保守值）：

| 项 | 默认 |
|---|---|
| 轮转 | 按天切割 + 单文件 256MB 上限，保留 30 天，超限 gzip 归档 |
| audit 保留 | 独立更长保留期（默认 180 天），不可运行时改写 |
| trace | 默认关闭（生产），`trace_level` 按会话开启；保留 7 天 |
| 关联性 | `sid/cid` 贯穿 engine→trace→audit→error，任一条日志可反查全链路 |
| Win7 | 文件名仅用 ASCII/日期，无符号链接，路径大小写不敏感安全 |

#### 11.2 落地口径（V39：`call --trace` 与 `explain --trace`）

上面是目标态契约；V39 落地 `call` 一条入口的 trace 通道，与契约的差别必须逐条写明，
否则"文档有 `step`、日志里没有"会变成排障时的误判：

| 项 | 落地形态 |
|---|---|
| 入口 | `spinduck call … --trace <日志路径>`；`--trace-format text\|jsonl`（默认 text）、`--trace-level stmt\|sql\|bind\|all`（默认 bind）。形态/级别只描述汇，脱离 `--trace` 单独给出即用法错 |
| 缺件顺序 | 日志文件在 D5 缺件门**之后**创建：拒绝执行 = 一条 SQL 都没跑，因此既不留日志也不留 sidecar |
| 覆盖语义 | 日志文件**独占创建**（`create_new`，由 OS 判定，无 TOCTOU）：目标已存在 ⇒ `ENV-0016` 拒绝执行，本次一条都不跑、一个字节都不写，退出码 1。理由：重跑同名路径等于把上一轮"跑过什么"的证据清掉，审计通道不接受静默销毁。sidecar 仍允许覆盖——它只在主日志独占打开成功后才写，届时旧的 `<日志>.sql` 只可能来自被人为删掉的主日志。备选「引擎按时间戳自动命名」未采用：那会改掉"路径由调用方给定"这条既有语义，需另行评审 |
| `seq` | **契约新增字段**：本次调用内单调递增行号（首行 1），同毫秒事件排序的唯一依据 |
| `sid` / `cid` | 真实值：`sid = s-<pid>-<进程启动 epoch 毫秒>`、`cid = c-<会话内第 N 次 CALL>`；一次 CLI 进程只有一条调用 ⇒ 恒 `c-000001` |
| `event` | 落在**顶层**而非 `detail` 内（`{"ch":"trace","event":"SQL_EXEC","detail":{…}}`）；detail 只放该事件的专属字段 |
| `step` | 未接入：步骤锚点在解释器里，尚未进入事件面 |
| `elapsed_us` | 字段在、值恒 0：埋点未计耗时 |
| 轮转/保留 | 未落地：写调用方给定的单个文件，逐行 flush（§11.1 at-least-once），无切割无压缩；日志通道的写失败上报为 ENV-0013，不静默少一份审计 |

**语句全文不入日志行。** 每条 SQL 由 `sqlid` 标识：先归一（连续空白折叠为单空格，
`src/trace/render.rs::flat`——与 `explain` 的 `one_line` 同源，同一个函数），再取 FNV-1a
64bit 的低 48bit，渲染为 `sq-` + 12 位小写十六进制。日志行只给归一文本的前 96 字符，
超出以 `…(+N chars)` 报出被削掉的字符数；全文落同前缀 sidecar `<日志路径>.sql`，每行
`sqlid<TAB>归一全文`、按 id 升序去重，`grep '^sq-xxxx' <日志>.sql` 即反查。归一与哈希
同口径 ⇒ 同一条语句在 IR、日志、sidecar 三处是同一个 id，且与源码缩进/换行无关。

**级别阶梯**（`level_allows`，级别越高记越多；BATCH_SKIP 任何级别都不丢）：

| 级别 | 记什么 |
|---|---|
| `stmt` | SQL_EXEC 的 sqlid 骨架（无文本无绑定） |
| `sql` | 上述 + SQL 开头片段 + 游标开/取（`CURSOR_OPEN` 带查询文本、sqlid 与真实列名；`FETCH` 带行号） |
| `bind` | 上述 + 绑定值原值（单值超 32 字符截断）——**默认档** |
| `all` | 上述 + 每条 IR 指令（`IR_STMT`）与 UDF 调用（`UDF_CALL`） |

**关联形态 A（`explain --trace`）**：`spinduck explain --file X.sql --trace <日志> <routine>`
在既有 `<pc>\t<op>\t<detail>` 契约之外追加第四列——静态 SQL 行给 `[trace ×N …]`（绑定不变只报
次数；首末不同报 `首 … 末 …`；日志里没有报 `[trace ×0]`），动态语句给 `[trace 动态：文本在运行期]`；
末尾汇总 `[trace] 对齐 m/n 条静态 SQL · 日志 k 个 sqlid`，并逐条报出"日志里跑了、本 IR 里没有"的
sqlid（三类来源：动态 SQL、攒批合并文本、被调例程自己的语句）。不给 `--trace` 时 explain 输出
逐字不变，故黄金比对与既有契约不受影响。

**覆盖面口径**（V39 已知缺口，排障时按此判读，不得当成完整序列）：`UDF_CALL` 变体尚无发射点；
`FETCH` 的行号是 `Some(1)`/`None`（内核未提供真实游标位置）；SQL 上下文函数下沉后，缓存表自身的
DDL/DML 与 `distinct_sql` 直连内核、不产生事件；`Engine::call` 与 `call_with_sink` 同一实现，
不注入 sink 时事件面为空、既有行为逐字不变。

### 11.2.1 内核托管与 SpinDuck 托管的边界

| 类别 | 文件/目录 | SpinDuck 职责边界 |
|---|---|---|
| **DuckDB 原生**（仅三种持久产物） | `<db>.duckdb`（含 catalog 表——内容是引擎的，文件格式是内核的）、`<db>.wal`（内核强制同目录，不可配置）、`spool/` 内溢出临时文件（`temp_directory` 指定，格式内核定） | 只做路径规划与调度：连接时设置 `temp_directory`、checkpoint/备份时机；备份/迁移必须走内核 API，禁止手工解析内核文件 |
| 扩展二进制 `*.duckdb_extension` | 内核扩展机制（默认 `~/.duckdb/extensions/` + autoinstall） | 企业模式禁用 autoinstall（§10.6），扩展静态链接进二进制，运行态不出现该目录 |
| **SpinDuck 托管** | `conf/`、`logs/*`（engine/error/audit/trace/crash）、`backup/` 目录结构、`tmp/` | 格式（JSONL 契约）、轮转、保留期全部由引擎定义；`backup/` 内**内容**由内核 backup API 产出（目录分层是我们的） |

两条边界规则：

1. **凡内核能生成的文件，SpinDuck 不自定义格式**——只做路径规划与调度；凡
   SpinDuck 生成的文件，内核无感知。唯一耦合点：WAL 与库文件同目录、溢出
   目录连接时显式设置。
2. **崩溃残留责任划分**：`data/`、`spool/` 残留由内核负责回收（下次启动自愈）；
   `logs/crash/` 现场由 SpinDuck 生成与上报——10.8 故障注入测试覆盖此分界线。

### 11.2.2 故障域分离：引擎问题 vs 存储过程业务问题

排障的第一步是判定"这是谁的 bug"。设计**故障域分类学**，每个错误事件在产生点
即打上 `domain` 标签，并路由到不同通道与不同详细级别：

| 故障域 | 判定标准（产生点判定，非事后人工） | 默认路由 |
|---|---|---|
| `ENGINE` 引擎域 | 解释器/改写器/编译器**内部断言失败**（invariant violation：IR 状态非法、规则应用崩溃、类型系统内部矛盾）——必然是 SpinDuck 自身缺陷 | `error/` 通道 ERROR 级 + **现场强制转储**（IR 指针、帧链、rules_version、duckdb_version） |
| `KERNEL` 内核域 | DuckDB 返回错误且经归域判定非业务性（资源耗尽、存储损坏、内核 bug 嫌疑） | `error/` 通道 ERROR 级 |
| `BUSINESS` 业务域 | ① PL `RAISE` 与被过程内 `EXCEPTION..WHEN` 捕获的 SQL 错误（业务已处理的异常）；② 业务校验失败（如 OUT 参数错误文案、业务表约束冲突） | **不进** `engine/` 错误通道；详情在 `trace/`（DEBUG 级），摘要仅在调用链 span 中 |
| `ENV` 环境域 | 资源不足、文件系统、配置缺失 | `engine/` WARN 级 |

**归域判定链**（编译/运行时自动执行）：

1. 解释器与改写器的 `debug_assert!`/显式 invariant 检查失败 → `ENGINE`（唯一
   可确定为引擎缺陷的信号，其他一切不做此判定）；
2. PL 层 `RAISE` / 被 handler 捕获后继续执行的错误 → `BUSINESS`（过程语义
   明确表达了"预期内错误"）；
3. **未捕获**而传播到 CALL 边界的异常 → 故障事件（`error/` ERROR 级 + PL 栈
   回放），归域按 SQL 错误映射表细分：`Catalog Error`（缺函数/缺表）→ 标记
   **改写缺口嫌疑**（可能是规则缺失，见下）；`Constraint Error` → `BUSINESS`
   （业务数据/逻辑）；其余 → `ENV` 待人工改判；
4. 归域可被显式覆盖：PL 侧 `RAISE` 支持声明域标签（默认 BUSINESS），供业务
   标注"这是环境问题，请引擎告警"。

**"改写缺口嫌疑"闭环**：SQL 下发前记录改写器指纹（命中的规则集 + 内核版本），
`Catalog/Parser Error` 事件自动附带该指纹 → 直接回答"是规则没覆盖还是业务
SQL 写错"：同 SQL 在规则探针中通过 = 业务 bug；探针缺失 = 引擎待补规则（转
`config/rewrite-rules.toml` 的 issue 流程）。

**通道与详细级别纪律**：

- `BUSINESS` 域**永不**进入 `engine/error` 通道（防止业务噪音淹没引擎故障）；
  反之引擎 `ENGINE` 域**永不**降级为 DEBUG；
- 业务过程内部日志（如业务落表日志）属**业务数据**，引擎只透传不加工、不吞噬
  ——引擎日志里只记"业务例程执行了写表操作"的审计事实（audit 通道）；
- `cid` 贯穿两域：一次 CALL 中先发生 BUSINESS 噪音、后触发 ENGINE 崩溃时，
  用 `cid` 在 trace 中回放完整序列定位触发链；
- 每条日志强制携带版本三元组（engine_version / rules_version / duckdb_version），
  `ENGINE`/改写缺口类问题可精确归因到规则集与内核版本组合。

**排障工具**（CLI §2.1 扩展）：

- `spinduck explain --last-error <cid>`：按 cid 汇聚四通道日志 + 改写指纹，
  输出归域结论与证据链；
- `spinduck replay <cid>`：按 trace 记录重放该调用（同参数 + SQL 序列），
  业务 bug 与引擎 bug 用同一把枪复现。

### 11.3 与既有设计的衔接

- §10.5 可观测性：trace/ JSONL 即 OTel exporter 的数据源（core 埋点 → cli 渲染 / exporter 输出）
- §10.4 审计：audit/ JSONL 为审计通道唯一格式，`spinduck golden` 不触碰 audit
- §9 规则数据化：conf/ 内规则文件带版本号，升级时旧版自动归档至 `conf/archive/`
- CLI 开发态：未配置 `home` 时使用当前目录（不强制目录结构），`load/register` 前
  可 `init` 生成骨架

---

## 12. 应用接入全生命周期（从研发到运行）

> 描述一个业务应用引用 SpinDuck 执行其存量存储过程的完整过程。每个阶段标注
> 使用的设施（§ 编号引用）与产出物；全程遵循 §9 原则：业务对象（表/过程/样例
> 数据/期望输出）由项目配置声明，引擎侧零硬编码。

### P0 前置评估（可行性与缺口清单）

1. 用 ogsql-parser 对应用 SQL 语料做 `validate/parse` 批量解析（validation.md V1
   同口径），产出：**解析通过率、PL 特性面清单、规则缺口清单**（对照规则文件
   `[rejects]` 与探针状态）。
2. 产出《接入评估报告》：不可降级特性（§6）逐项列出迁移改法；规则缺口转
   规则文件 issue 流程（§11.2.2 闭环）。
3. 本阶段产出物是**决策依据**，不写任何代码。

### P1 研发环境搭建

1. `spinduck init <home>` 生成目录骨架（§11.1）与默认配置/规则副本；
2. `spinduck load <ddl-dir>` 建表/视图/序列（应用 `[ddl]` 规则）；
3. `spinduck register <proc-files>` 注册过程/函数/PACKAGE（解析→编译→catalog，
   错误定位源码行）；
4. 开发调试闭环：`explain <proc>`（看 IR 与 SQL 改写产物）→ `call --trace`
   （看运行事件流）→ `repl`（交互验证）。见 §2.1。

### P2 规则适配循环（研发期核心工作）

`explain/call` 暴露的改写缺口（§11.2.2 改写缺口嫌疑）→ 在**项目规则覆盖文件**
（项目模式为 `rules/spinduck-rules.toml`，部署态为 `conf.d/`，见 §12.1）中补充映射 → `spinduck rules probe` 全量回归 → 通过后合入。
引擎代码原则上不动；动了规则必须带探针测试（§9 规则生命周期）。

### P3 测试与验收（企业准入门禁）

1. **黄金比对**：样例数据 + GaussDB 基线输出声明进项目 case 配置，
   `spinduck golden <case>` 执行（M10 口径；模拟器脚本即首案例，validation.md V3）；
2. 故障注入（CALL 中途 kill → WAL 恢复语义）、长循环内存曲线、性能基准
   （§10.8 三件套）；
3. 产出物：验收报告 + 基线固化进 case 配置（后续回归资产）。
4. **外部依赖前置**：GaussDB 基线输出（baselines/）需要在真实 GaussDB 环境
   对样例数据执行后导出——这是 M10 的**外部依赖**，须在 P1 阶段同步启动准备
   （申请环境、录制基线）；无基线前 golden 以模拟器脚本口径作为替代对照。

### P4 打包与部署

- 内核版本锚定（§5）+ 扩展静态链接、离线安装（§10.6）；
- `SPINDUCK_HOME` 布局落地（§11.1），审计保留期/备份策略按企业要求写入
  engine.toml（§11.2 默认值为基础）；
- 产物 = 引擎二进制 + conf（含项目规则与 case 配置）+ 数据目录（空）。

### P5 应用集成（编码约定）

应用通过引擎公共 API 调用（三种形态，同一 API 语义）：

| 形态 | 场景 |
|---|---|
| 进程内库（Rust lib / C API） | 嵌入应用进程，最低延迟 |
| 旁车进程 + 本地协议 | 应用与引擎解耦部署（语言无关） |
| CLI 脚本驱动 | 批处理/运维场景（`spinduck call`） |

集成约定：

- 调用统一为 `call("<pkg>.<proc>", args)`：IN 传值、OUT/INOUT 经结果结构取回、
  返回标量直接取；
- 会话 = 连接：PACKAGE 级变量、临时对象、事务边界标记随会话存活（§3.6）；
- **错误契约**（§11.2.2）：应用只处理两类——`BUSINESS`（业务预期内，按错误码
  分支）与未捕获异常（转 `ENGINE/KERNEL/ENV`，应用记录 cid 上报，不自行归因）；
  归因由引擎完成，应用职责是带上 cid 报障；
- 应用不直连 DuckDB 执行裸 SQL（绕过改写器/审计的唯一豁免入口必须显式配置）。

### P6 运行运维

监控指标（§10.5）+ 审计（§10.4）+ 备份（§10.7）按 §11 布局运转；trace 按会话
按需开启；故障排查入口：`cid → explain --last-error → replay`（§11.2.2）。

### P7 升级（内核 / 规则 / 例程）

1. **内核升级**：V6 探针套件全量复跑（`rules probe`）→ `golden` 全量回归 →
   数据经内核 API 导出/导入迁移（存储格式约束 §10.6）→ 规则文件差异评估；
2. **规则升级**：规则文件版本化变更 + 探针测试随行（§9）；
3. **例程升级**：`register --replace` 增量替换，catalog 保留版本历史（§3.6），
   支持回滚。

### 阶段-设施对照总表

| 阶段 | 设施 | 产出物 |
|---|---|---|
| P0 | ogsql-parser | 接入评估报告 |
| P1 | cli init/load/register/explain/call/repl | 可运行环境 |
| P2 | 规则覆盖 + rules probe | 项目规则文件 + 探针测试 |
| P3 | golden + §10.8 门禁 | 验收报告 + 基线资产 |
| P4 | §10.6/§11 | 部署产物 |
| P5 | 公共 API + 错误契约 | 应用集成代码 |
| P6 | §10.4/5/7 + §11 | 运行态 |
| P7 | rules probe + golden + catalog 版本 | 升级记录 |

### 12.1 研发态项目目录结构（`spinduck.toml` 项目模式）

推荐布局（P1–P4 的文件载体；引擎与 CLI 识别此结构，P0 评估报告等生成产物
亦落位于此，使项目目录成为**可提交、可评审、可复现**的完整接入单元）：

```
<app>-procs/                        # 该应用的存储过程接入项目根
├── spinduck.toml                   # 项目声明（唯一入口，见下）
├── sql/
│   ├── ddl/                        # 表/视图/序列源码（P1 load 输入）
│   ├── routines/                   # 过程/函数/PACKAGE 源码（P1 register 输入）
│   └── stubs/                      # 外部依赖桩包（无法迁移的远端依赖的本地替身）
├── rules/
│   └── spinduck-rules.toml         # 项目规则覆盖（P2 产出；叠加在引擎种子规则之上）
├── probes/                         # 规则探针测试：*.sql + 期望（P2/M3.5 门禁）
├── cases/
│   ├── datasets/                   # 样例数据（seed SQL / CSV / Parquet）
│   ├── baselines/                  # GaussDB 黄金输出（P3 比对基准）
│   └── *.case.toml                 # case 定义：数据集 × 例程 × 期望输出 + `[[case.assert]]` 断言集
├── reports/                        # 生成产物（P0 assess / P3 验收报告；可提交评审）
└── .spinduck/                      # 本地运行态（gitignore）：dev 库、日志、tmp
                                    #   ——即 SPINDUCK_HOME 的开发态实例（§11）
```

`spinduck.toml` 最小骨架（段落与引擎配置同源，段内键即 §2.1 通用选项的声明式
形态）：

```toml
[project]
name = "app-procs"
kernel = "1.5.5"                    # 目标内核（M1 锚定，rules probe 校验）

[paths]
ddl = "sql/ddl"
routines = "sql/routines"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
probes = "probes"
home = ".spinduck"                  # 开发态 SPINDUCK_HOME（§11 布局的开发态实例）

[engine]                            # 开发态默认；部署态由 engine.toml 覆盖
threads = 4
memory_limit = "2GB"

[cases]
datasets = "cases/datasets"
baselines = "cases/baselines"
include = ["cases/*.case.toml"]

[package]
extensions = []                     # 静态链接清单（离线部署 §10.6）
audit_retention_days = 180          # 部署参数预览（engine.toml 生成模板）

[[entry]]                           # 本项目对外可调用面（0..n 条；项目态 compile 的闭包起点）
routine = "pkg_x.prc_y"             # 必须 package.procedure 全名（归一小写）
kind = "procedure"                  # function | procedure（缺省 procedure）
```

**入口声明与项目态编译（`spinduck compile --dir <项目目录>`）**

接入验收的口径是"从对外可调用面出发，链路上每个对象都能落地"，而不是"文件都编
译得过"。因此项目声明显式承载 `[[entry]]`（全名必须带包限定——裸过程名在语料里
可能同名不同包，链路无法唯一定位；显式 `--entry` 不受此限，独立例程可用裸名）。
项目态 compile 一次 codeweb 导出，按入口各求一份闭包、落盘运行计划（§13.5），
再对闭包源做 IR 体检：缺件（`MISS`）或体检失败（`FAIL`）即退出码 1。
旧 `plan`/`verify` 子命令已删除（issue #38）：计划面收敛到 `compile --entry`，
快照同步收敛到 `compile --sync-manifest`。

链路核验的四类判定口径保留为**库级**能力（`verify_project`，守护测试
`tests/verify_chain.rs` 与用例断言层消费，不再是 CLI 契约）：

| 核验项 | 判定 | 失败含义 |
|---|---|---|
| `Entry` | 全名在例程目录命中且种类（function/procedure）一致 | 入口拼写不符或包体缺失 |
| `Routine` | 从入口 BFS 沿 `CallRoutine` / SQL 上下文下沉调用点展开，每个被调例程都已登记 | 链路断点（源文件未纳入项目） |
| `Relation` / `Sequence` | 链路 SQL 引用的表·视图·序列在内核目录存在 | 缺 DDL/桩，或部署顺序不符 |
| `Scenario` | `*.case.toml` 中声明该入口的每个场景实际调用一次 | 用例缺失显式 `SKIP`（不计失败）：无场景且无断言指向该入口 = 公示"可执行性未验证"；有 `[[case.assert]]` 指向该入口 = 公示"断言层已覆盖，本命令不执行断言" |

SQL 侧对象名取自改写前的静态扫描（`FROM`/`JOIN`/`UPDATE`/`INSERT INTO` 后的标识符
与 `<seq>.nextval`），字符串字面量与表函数不计——这是**近似口径**，其假阴面（动态
拼接文本里的表名）由 `Scenario` 的真实执行兜住。退出码：有 `FAIL` → 1。

**用例断言集（`[[case.assert]]`）**：`Scenario` 只回答"可调用"，不回答"结果对不对"，
因此非简单的函数/过程/包在同一个 `*.case.toml` 里再声明显式断言集——四个维度、逐值
比对，全部跑在进程内内核上（**不依赖任何外部数据库**，AGENTS §4.1 的 cases 层）：

| `kind` | `target` | 期望文本 `want` |
|---|---|---|
| `rows` | 断言 SQL（自带 `ORDER BY` 定序） | 归一行集 `"[a, b] [c, d]"` |
| `call` | 被测例程全名 | 归一化返回值（NULL 显式为 `NULL`） |
| `out` | 被测例程全名 + `param` | 该 OUT/IN OUT 参的归一化值 |
| `error` | 被测例程全名 | `"错误码\|消息片段"`（片段可省略 = 只比码） |

`kind`/`target`/`want` 缺项、`out` 缺 `param` 一律 `ENV-0031` 报错——空 `want` 等于
永真断言，不接受静默通过。执行口径与 `verify` 同源（`deploy_project`：表 DDL → 视图 →
桩 → 例程 → 种子），消费者是 `spinduck/tests/cases_native.rs`；取值探针
`CASES_PROBE=1`，提交前必须以实测值填回 `want`。实测记录见 validation.md V21。

**同一份资产上的两层分工**：黄金层（`--features gauss`，需 A 模式参照实例）比的是
语料级**函数**的逐场景返回值；`[[case.assert]]` 断言层（`--features native`，进程内内核、
零外部环境）比的是包限定**过程**的多维可观察结果（`out`/`rows`/`error`）。
一个用例项目因此可以同时有 `kind = "function"` 的黄金比对与指向另一条过程的断言集——
两者读同一份 `sql/` 与 `cases/datasets/`，互不嵌套：断言层不是黄金层的子集，黄金层的
结论也不作为断言层的期望来源。该划分的实测理由见 validation.md V21.12。

**运行态项目快照（§11.1 `conf/spinduck.runtime.toml`）**：`compile --sync-manifest`
把"运行态所需的那部分声明"（项目名/内核版本 + 可调用面 + 资产绝对路径 + `[runtime]`
来源标注）写入 `<home>/conf/spinduck.runtime.toml`。设计取舍：

1. **单一写入者**——快照由 `compile --sync-manifest` 生成（默认关），手写真源仍是
   项目根的 `spinduck.toml`；快照带 `source`/`generated_by`，内容与真源漂移由下一次
   同步报告（`updated`）。写入成功后删除旧名快照 `conf/spinduck.toml`（issue #38
   彻底换名，不留读取 fallback）。
2. **运行态目录自足**——资产路径已绝对化，故 `ProjectConfig::load` 认
   `<dir>/spinduck.toml` 与 `<dir>/conf/spinduck.runtime.toml` 两处（前者优先），把
   `.spinduck/` 单独交给运行侧也能复现同一结论；此时**不回写**快照，来源
   标注始终指向研发态真源。
3. **不复制资产**——快照只是指针集合（源码/规则/用例仍在研发态目录），避免
   "运行态有一份过期语料"这类二次漂移。

原则重申：目录里全部是**业务侧资产**（源码/规则/用例/报告），`.spinduck/`
运行态与引擎二进制不入库；同一项目目录在研发态（cli 零参数）与部署态
（engine.toml 显式配置）间无缝衔接。

**实现状态**：`spinduck_core::project`（`ProjectConfig` / `CaseConfig` /
`EntryConfig`）已实现本结构的声明识别（路径解析、用例枚举、数据集/基线定位、
物化清单、`[[entry]]` 可调用面）；黄金对比 harness（`spinduck/tests/common`）是其
首个消费者，`golden/case_*` 全部按本布局组织。已入库项目里 `golden/case_corpus_purchase`
声明 1 条入口（包限定过程）。旧 `spinduck verify` 曾在**进程内内核**上跑通全链
（`entries=1 checks=11 failed=0`，无需容器）；issue #38 删除该子命令后，这条链保留为
库级守护测试（`tests/verify_chain.rs`），CLI 面由项目态 compile 承接（需 codeweb 0.9.7）。
其余三个项目的主角是独立例程（`prc_agg_fund` / `fnc_get_stock_kind` / `fnc_com_get_min_tax`，
无包限定），按"入口必须全名定位"的口径不声明 `[[entry]]`，其正确性由 `[[case.assert]]`
断言层兜住。`spinduck_core::verify`（链路抽取 `chain` / 部署 `deploy` / 逐对象判定 `run`）
与 `spinduck_core::project::manifest`（运行态快照）已落地，
`spinduck init/load/register/golden` 命令面属后续里程碑（P1/P3）。

---

## 13. 数据接入与回写（GaussDB 物化，V1 I/O 层）

> V1 数据源**锁定 GaussDB**。运行模型：处理前物化高斯数据表进本地 DuckDB →
> 执行存储过程 → 延迟事务本地提交 → 结果回写高斯。V1 不接入其他数据源；
> `DataSource`/`DataSink` 为接缝 trait，未来扩展其他源不动引擎主干。

### 13.1 物化（Extract → Materialize）

| 项 | 设计 |
|---|---|
| 表清单 | 项目 `spinduck.toml` `[io]` 声明物化表与过滤条件；提供 `--auto` 模式：用 ogsql 表引用分析从注册例程中**自动提取表依赖**生成清单（人工确认后固化，避免运行中撞到未物化表） |
| 读取通道 | V1 首选 **GaussDB COPY（CSV/BINARY）→ 本地 Parquet → DuckDB `read_parquet` 注册**：稳定、可断点续传、产物可缓存复用；DuckDB postgres 扩展直连 GaussDB 存在协议兼容问题（认证/二进制文本协议，社区已有针对性补丁但未上游化），V1 不依赖 |
| 增量策略 | 表级声明物化键（清算场景天然按业务日期列）：全量 / 按分区键增量（`WHERE bcrq = :date` 物化当日分区，幂等覆盖） |
| 类型保真 | 物化列类型按规则文件 `[types]` 反向映射；NUMERIC 精度/DATE/时区往返保真是探针必测项（物化→回写→高斯再读，逐列比对） |
| 一致点 | 物化即快照：记录每表物化时间戳与过滤条件入 catalog；过程执行期间数据与快照一致（本地处理不受源端变化影响） |
| 物化通道（V27 现状） | 默认 COPY STREAM 快路径：GaussDB `COPY TO STDOUT` CSV 流式落盘（与目标库同目录，裸文件名 `--db` 显式落 CWD；成功即删、失败保留供诊断）→ DuckDB `COPY FROM`（两侧显式 `NULL '\N'`，D14 空串保真）。**回落阶梯**：仅「入库前失败」自动回落 500 行批量 INSERT（降级原因经 stderr `WARN` 可观测，stdout 契约不变）；`COPY` 已入库后的任何失败（行数查询、CSV 行数交叉核对——部分加载/重复加载哨兵）一律报错不回落，防止 INSERT 追加把本地表灌成两倍行数。多表 `--jobs` 进程内并行（每线程独立 GaussDB + DuckDB 连接，单表失败不中断其余表）；重复表名 = 用法错。显式逃生门 `mode = "insert_batch"` 与 PR-B 并行分片未落地，见 `docs/materialize-copystream-design.md` |

### 13.2 回写（Sync）

| 项 | 设计 |
|---|---|
| 回写模式 | 表级声明三选一：append（追加，清算结果表主流）/ upsert（按业务键展开为 GaussDB MERGE）/ replace_partition（按分区键整段替换） |
| 批量通道 | GaussDB 侧批量写（COPY FROM STDIN 或 JDBC batch），批大小可配；单表回写为一个 GaussDB 事务（外部事务，独立于 §3.5 延迟事务） |
| 幂等与校验 | 回写前校验本地待写行数 vs 上次回写水位；回写后行数/校验和双向核对；失败重跑按模式幂等（append 需业务键去重或 replace_partition） |
| 失败语义 | 回写失败 = 该表回滚（GaussDB 事务保证），本地结果保留 → 修复后 `sync --retry` 单表重试；多表回写顺序按依赖拓扑（被业务链引用的先写） |

> **落地实态（feat/sync-writeback，机制规格见 `docs/sync-writeback-design.md`）**：
> `sync` 当前落地的模式集为 **append / slice / upsert**（`--mode`，缺省 append 与
> V26 契约逐字一致；append 无 mode/where 时输出不变）。上表的
> `replace_partition`（对应 slice 的分区整段替换语义）由 `--mode slice
> --slice k=v` 承接——结构化等值谓词、同事务先 DELETE 后 COPY、幂等重跑；
> `--slice` 值支持 `$ENV` / `${ENV}` 引用（缺失即 ENV-0016 红）。upsert 未用
> GaussDB MERGE，改为 **staging 临时表 + join 式 UPDATE + INSERT NOT EXISTS**
> （A 模式无 `ON CONFLICT`；语句数与行数无关）。通道为 DuckDB 内核 `COPY TO`
> CSV 中转 → `copy_in` 流式灌入（快路径失败自动回落 V26 逐行通道）。
> 前置守卫族（触碰目标数据之前执行，全部 fail-closed）：G1 目标 schema 漂移
> （ENV-0021）/ G2 slice 越界（BIZ-0102）/ G3 staging key 完整性（BIZ-0103）/
> G4 目标 key 唯一索引覆盖（ENV-0022）。实现位于 `src/io/sync.rs`（纯逻辑）与
> `src/io/sync_tx.rs`（事务骨架与通道）。

### 13.3 运行时序（单次批处理）

```
materialize(物化快照+时间戳)
  → CALL(过程处理，延迟事务本地提交)
  → sync(结果回写 GaussDB，外部事务)
  → 校验(行数/校验和) → 水位推进
任一环失败：本地状态保留 + crash/ 日志现场，支持断点重跑（§10.1 幂等语义）
```

### 13.4 V1 边界

- 仅 GaussDB 单一数据源；实时/流式接入不做
- 物化粒度 = 表级（不做列级投影裁剪优化，留二期）
- 回写不承诺高斯侧触发器行为（属源端语义，未验证）
- 大表物化内存/磁盘上界沿用 §10.3 配置；Parquet 缓存位于 §11.1 `data/` 旁
  （`data/parquet_cache/<db>/<table>/<分区>`）


### 13.5 运行计划（`run-plan`，design §6.1 的落地 schema）

项目态 `compile` 把"一次闭包求值所需的全部对象与文件"落成计划文件，落在
`<home>/plan/<target>.toml`（§11.1 运行态分区）；计划是**数据**不是执行结果，
由 `deploy`（A2-3）消费。产出方 `spinduck/src/plan/`。

```toml
[plan]
schema_version = 1
target    = "pkg_split_trade_step2.prc_pre_deal_shjycf"   # 归一小写的例程全名
generator = "spinduck 0.1.2 (plan schema 1)"             # 红时先怀疑两边口径
project_dir = "../.."                                    # 相对计划文件所在目录，不含绝对路径
schema    = "bigfund"                                    # --schema 透传；缺省整行不写

[sources]
ddl_routines = ["sql/routines/PKG_SPLIT_TRADE_STEP2.sql", "sql/stubs/PACK_LOG.sql"]
ddl_tables   = ["sql/ddl/tables/par_fund_info.sql"]       # 表/视图/序列 DDL（deploy 用）
materialize  = []                                         # 只读物化清单（§6.4 默认空）
writeback    = []                                         # 回写白名单（默认空）

[[missing]]                                               # 缺件红清单（非空即 rc=1）
object = "relation:par_oper_close_date"
from   = "pkg_a.fnc_x"                                    # 引用它的例程（R3 定位）
```

| 项 | 口径 |
|---|---|
| 对象标识 | `relation:` / `sequence:` / `routine:` 三前缀。视图与表同为 `relation`（链路抽取本就归一类），故没有 design 草图里的 `table:` |
| 闭包来源 | 项目闭包来自一次 codeweb 0.9.7 JSON 导出，不再用 SpinDuck 自有的链路抽取 |
| 名字归一 | `db.schema.name` 截尾两段 + 小写；被调例程先按调用方所属包补全再试裸名，DDL 名先全名后尾段（同 `verify::run::resolve_routine` / `present`，两处实现分开的唯一原因是容器类型不同） |
| 引擎自建对象 | `deploy_schema` 自己建的表（`ENGINE_PROVIDED_TABLES`，现为 `sys_dummy`）没有源文件：既不进 `ddl_tables` 也不报缺件 |
| 文件清单 | 一个文件常声明多个闭包成员，故按**文件**去重排序；路径一律相对项目根、`/` 分隔 |
| 可复现 | 渲染前排序去重、段序与字段序固定、不写日期、不写绝对路径 → 同输入逐字节相同；落盘只在内容变化时写，摘要行末词随之为 `updated` 或 `unchanged` |
| 退出码 | `[[missing]]` 空 = 0；非空 = 1（照抄缺件行 `MISS <object> <- <from>`）；目标例程未登记 = 1（`BIZ-0150`）且不落盘；参数用法错 = 2 |
| 静态边界 | 词法扫描不建 AST：注释与字符串字面量不参与，改写后形态（`nextval('seq')`、空格拆开的限定名）按原文单独取一次。计划描述静态引用，"此刻内核里有没有这张表"归 `deploy`/链路核验 |
| 逗号连表的关系名 | `FROM a t, b s` 是语料主形态，而 `FROM` 只出现一次：`sql_object_refs` 取到第一个关系名后走 `comma_join_refs` 把逗号连表里其余名字一并取回（别名只有在紧跟逗号时才当别名，`FROM a, b` 的 `b` 是表不是别名）。括号子查询（`FROM (SELECT 1) x, t_b b`）由 `subquery_tail_refs` 越过整对括号后继续取连表；带括号的调用形态（`unnest(…)`、`read_csv(…)`）与非对象名一律不收——宁少不误。这条刀口同时决定链路核验（库级）的判定与计划的部署清单（两者共用同一函数），改它等于同时改两个面 |
| 视图体的基表 | 视图文件进清单后，再展开一层它的**视图体**引用（`closure::expand_view_bodies`，用 `chain::sql_object_refs`，与例程体同一条抽取管线，不开第二套词法）。理由：下发顺序是"表 → 视图"，视图体读的表没进清单时内核建视图必失败，`deploy_view` 就静默退化成 `SELECT 1` 占位视图——对象在、列不在。只补**有 DDL 文件**的引用；查不到声明的名字（`dual`、外部同义词）不据此报缺件，否则一个语料外的别名就把整份计划判红 |

### 13.6 `deploy`：计划的消费面（design §6.2，A2-3）

三段落点，职责互不重叠：

| 段 | 落点 | 职责 |
|---|---|---|
| 壳 | `spinduck/src/cli/deploy.rs` | 读计划 → 缺件即拒 → 定位项目 → 开库 → 打印一行汇总。零引擎逻辑（§1） |
| 消费 | `spinduck/src/plan/deploy.rs` | `resolve`（计划 → 逐条核对存在的文件，视图/表分流）与 `apply`（逐文件下发）分成两段：全部文件核对完才碰内核 |
| 下发 | `spinduck/src/verify/deploy.rs` 的 `deploy_ddl_file` / `deploy_view_file` / `ensure_engine_provided_tables` / `register_routine_files`（`pub(crate)`） | 与库级链路核验共用同一套逐文件实现，顺序同为 表/序列 → 视图 → `sys_dummy` → 例程；视图是否走视图体由 `creates_view()` 判定，走的是链路同一把词法刀（注释与字符串不参与） |

口径：

- 输入是**计划里的文件清单**（`sources.ddl_routines` / `sources.ddl_tables`），不做目录发现——
  design §6.2 三点差异的第一条。清单外的资产一律不下发，`deploy` 因此可复现。
- 库**落盘**：开库发生在 CLI 接缝处（`DuckExecutor::open(--db)`），库级链路核验仍是
  `open_in_memory`，接缝签名未动。开库排在文件核对之后——`open` 会创建文件，先开后查等于
  留下半成品库。
- 缺件面：`[[missing]]` 非空 → rc=1，照抄 `MISS <object> <- <from>`，不开库、不建对象；
  计划列出的文件不存在 → `ENV-0030`。两条都不要求项目声明 `[[entry]]`（计划的入口是目标例程）。
- **不跑种子**：`cases/datasets/*` 与 `materialize` / `writeback` 两条清单都不消费——数据面归
  §13.1 的物化，装配面只管对象与例程。
- `plan.schema` 只随计划透传、不参与下发：对象一律落在内核默认目录 `main`，与链路核验的既有
  判定口径一致。design §6.1 的草图只给了 `schema` 字段、没有约定 `deploy` 用它建 schema，
  本文把它**明确记为未落地**（validation V31.7），不假装支持。
- **非幂等**：对同一个已装配过的库文件再跑一次会撞 `CREATE TABLE` 已存在（内核报错原样带出，
  不静默跳过）。`deploy` 的语义是"从零装配一个库"，重来请换文件或先删。
- **例程不落盘**（与上一条同量级的口径限制）：`register_routine_files` 走的是进程内例程目录
  （`src/catalog.rs` 的 `register`），写进库文件的只有表/序列/视图与数据。因此"部署好的库"
  只承诺**对象面**可跨进程复用；换一个进程 `call --db <库>` 而不带 `--file`，得到的是
  `[BIZ-0150] routine … not registered`（一手证据见 validation V31.3）。免逐个传源码要靠
  `call --plan`（design D5/R3，A2-4），不是靠 `deploy` 把存过存进库。

退出码：`0` 装配完成 / `1` 计划带缺件 / `2` 用法错（缺 `--db`、多余位置参数）；
未编译 `native` 时 `ENV-0012`。全部输出走 stdout，stderr 保持空（§1 CLI 契约）。

### 13.7 `call` 的缺件门与计划面入口（design D5，A2-4）

`call` 现在有两条入口，**都在 `eng.call` 之前**过同一把缺件刀：

| 入口 | 目标与例程源从哪来 | 缺件门 |
|---|---|---|
| `call [--db <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] <routine> [args...]` | 位置参数给名，`--file` 给源码 | **内核目录**：链路上任何 `relation:`/`sequence:` 在内核 catalog 里没有 ⇒ 列清单、rc=1、不执行 |
| `call --plan <计划.toml> [--db <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] [args...]` | 计划给目标与闭包内例程源，`--file` 退化为"计划外追加" | **两道**：计划自带 `[[missing]]` 非空 ⇒ 先拒且**不开库**；计划干净再走上一条的内核门 |

落点三段：

| 段 | 落点 | 职责 |
|---|---|---|
| 壳 | `spinduck/src/cli/run/call.rs` 的 `call_target_and_sources` / `refuse_on_kernel_gaps` | 解析目标与源清单、按计划拒、按内核拒；零闭包逻辑（§1） |
| 判定 | `spinduck/src/plan/closure.rs` 的 `walk_chain`（BFS 本体）+ `kernel_gaps`（内核参照面） | **一份闭包实现，两个参照面**：`build`（计划面）以项目 DDL 声明为参照，`kernel_gaps`（`call`）以 `verify::kernel_objects` 为参照。不允许再出现第二套 BFS——否则计划说干净、门说缺件的漂移只是换了个方向。被调面的采集同样只有一套：`chain::routine_refs`（结论）+ `chain::expr_func_names`（候选，由目录判），两个参照面共用 |
| 解析 | `spinduck/src/cli/args/mod.rs` 的 `CallTarget::{Routine, Plan}`（`--plan` 形态的解析在 `spinduck/src/cli/args/call.rs`） | 目标形态是枚举不是可选字段：`--plan` 下位置参数**全部**归实参（不猜例程名），两者皆无即用法错且诊断同时点名两条路 |

口径：

- **开关与目标形态无关**：`--batch-commit` / `--step-budget` 在同一个解析循环里读，
  赋值发生在注册循环之前，所以裸名面与计划面吃下的是同一套执行面开关；且用法判定
  （如 `--step-budget 0`）**排在计划文件读取之前**——缺计划文件不得掩盖用法错
  （两个退出码分别是 2 与 1，判据见 validation V36）。
- **红必须早于产出**：门判缺件时 stdout 只有 `MISS <object> <- <from>` 行 + 一行摘要，
  不含 `OUT`/`RETURN`/`CALL … OK`（`tests/cli/call.rs` 逐字锁死）。形态 C 那种
  `RETURN 0` + rc=0 的静默面就此消失（design §3.3）。
- **计划门的拒排在开库之前**：DuckDB 的 `open` 即建文件，晚一步核对就留下半成品库——
  `deploy` 与 `call --plan` 必须先拒在同一件事上（断言面：`!db.exists()`）。
- 目标未登记时 `kernel_gaps` 给 `None`：那是 `BIZ-0150` 的故障域，门不抢判（§5 归域在产生点）。
- `routine:` 缺件与表/序列缺件同列一份清单，按标识排序去重；`engine_provided`（`sys_dummy`）短路。
- 名称口径与调用本身同源：目标走 `catalog.lookup`（与 `Engine::call` 同一个键），摘要行
  `target=` 用小写归一名，与计划/`deploy` 一致——门不能比它守护的那次调用更宽或更严。
- **表达式级被调函数进闭包（design A2-5，V34）**：`routine_refs` 只给"确定是例程"的名字
  （语句级 `CALL` + 带包限定的表达式调用 + SQL 下沉点），PL 表达式里的裸名调用
  （`v := fnc_x()`）走第三个采集面 `expr_func_names`——它交的是**候选**，不是结论：
  闭包逐个拿例程目录精确查名，命中才把被调函数排进 BFS，于是**它自己的表**同时进
  计划与门。判否（查不到）一律丢弃，不报 `routine:` 缺件，理由与运行期同源：
  `runtime::eval::eval_funccall` 也只做一次 `catalog.lookup(name)`，未命中就按内建/库函数
  下沉给内核求值——把每个 `trunc(...)`、`nvl(...)`（规则改名后是 `coalesce`）都判成
  假缺件，等于门比它守护的那次调用更严。考题语料实测：10 条 entry 的 `missing` 仍全为 0，
  而 `routines`/`tables` 两条清单同步变大（entry5 由 7/20 到 24/41）；把"未命中也报缺件"
  这一变体在同一条闭包上跑出来，9 条 entry 共 23 行 `MISS routine:`，去重只有 4 个名字
  （`coalesce`、`now`、`round`、`substring`），全是内建或规则改名后的内建（V34.4）。
- **仍然看不见的两类**（登记于 design §8「缺件门的盲区」行）：① 被调函数**源码不在本次
  注册面里**（`call` 少给了一个 `--file`）——目录里没有这个名字，门与运行期一样无从判断，
  后果仍是 `KERNEL-0010` 响亮报错；② 同包成员函数按裸名调用（登记名是 `pkg.fnc_x`）——
  这是**分派**面的缺口（`eval_funccall` 不做包名补全），不在闭包里补，否则计划要为一次
  根本不会命中的调用铺表。语料实测此类为 0（V34.4），要收口须改解释器分派，属另立编号。

退出码与通道沿用 §1 CLI 契约：`0` 正常返回 / `1` 缺件红或引擎错误 / `2` 用法错（`call` 无目标）。

### 13.8 源码读入的编码判定（design §3.5，D9②）

一处实现、四个消费面、一个刻意留在原地的体检面：

| 段 | 落点 | 职责 |
|---|---|---|
| 判定 | `spinduck/src/source.rs` 的 `read_text` / `decode` | **唯一**的读入实现：`std::fs::read` → UTF-8 严格 → `encoding_rs::GBK.decode`（`had_errors` 即拒）→ `ENV-0030`。`decode` 不碰磁盘，三种字节形态都能单测 |
| 执行/计划/部署/核验/比对面 | `cli::read_source_strict`（`call`、`explain`、项目态 compile 的 IR 体检、`deploy`、`query` 的 SQL 文件、`compare` 的两份快照）与 `verify/deploy.rs` 的四文件下发点（表 DDL、视图、例程注册、数据集种子） | 全部走同一把刀。`verify/deploy.rs` 原先各写一份 `read_to_string`，故 GBK 语料在计划侧可读、核验侧报编码错——同一份资产两个结论，正是 §13.7 反对的漂移形态 |
| 体检面 | `cli::read_source`（`compile`） | **不参与解码**：有损 UTF-8 + `(non-utf8)` 标记，rc=0。语料体检要"一趟扫完并按错误码分类"，停在第一个编码问题就让体检本身没法用（design §3.5 末段的原始取舍，D9② 未改） |
| 依赖 | `spinduck/Cargo.toml` 的 `encoding_rs = "=0.8.44"` | 由 `ogsql-parser` 的传递依赖提为直接依赖：`Cargo.lock` 只多一行（该 crate 进 `spinduck` 的依赖表），零新增 crate、零新增 `cargo audit` 面 |

口径：

- **不猜**：判定只有两把——合法 UTF-8 或合法 GBK，其余一律 `ENV-0030`。D9① 消灭的静默形态
  是"`from_utf8_lossy` 把中文字面量换成 U+FFFD 却 rc=0"，有损兜底一旦回来，MATCH/NO_MATCH
  分叉就回来（判据实验见 design §3.5，`tests/cli/call.rs` 锁在 D9② 两侧都是 `RETURN MATCH`）。
- **UTF-8 优先是判定不是风格**：交付件里 11,462 个 UTF-8 文件中 5,130 个的字节同时是合法 GBK，
  按 GBK 读它们必然乱码。所以顺序先 UTF-8，且这份优先级有实测计数支撑（V33.4）。
- **转码发生在读入时，语料文件逐字不动**（AGENTS §6）。引擎不写回源文件，也不提供
  `--encoding` 覆盖：一旦允许人工指定，同一份语料就能被解成两种文本，正是本轮要消灭的形态。
- 错误码分工：读不到文件 = `ENV-0010`（`read <path>: <io>`），读到但两把判定都不成立 =
  `ENV-0030`（`读取 <path>: stream is neither valid UTF-8 nor valid GBK`）。`verify` 侧原先把
  两者都归 `ENV-0030`，收口后与执行面同码同文本。
- **残留风险（一手计数，判定仍是推算）**：交付件里 3,149 个非 UTF-8 文件全部 GBK 严格可解
  （两把都不成立的 = 0），其中 **418 个同时是合法 Big5 且两种解出的文本不同**。这 418 个到底
  是 GBK 还是 Big5，字面统计判不动（GBK 解出 94.8% 落在 CJK 统一表意文字区，Big5 解出 92.3%），
  采信 GBK 的依据是交付来源（简体中文 GaussDB 环境，design §4 画像）而非检测器。若日后接入
  繁体语料，这条会从"理论"变成"静默错值"，届时必须显式指定编码而不是继续加启发式。

### 13.9 `query` / `compare`：结果快照的落盘与裁决（design §6.2，A5 本地半边）

四段落点，职责互不重叠：

| 段 | 落点 | 职责 |
|---|---|---|
| 壳 | `spinduck/src/cli/query.rs`、`spinduck/src/cli/compare.rs` | 读入 → 调 core → 渲染 → 退出码。零判定逻辑（§1）；`DIFF` 行的**文本形状**归这里，差异**是什么**归 core |
| 载体 | `spinduck/src/snapshot/mod.rs` | `Snapshot{schema, rows}` 与它的 JSON 约定、`capture`（切句 + 取回全部行）、`to_json_string`（逐字节可复现、一行一行、结尾恒带换行） |
| 裁决 | `spinduck/src/snapshot/compare.rs` | `compare`（位置序）与 `compare_by_key`（键序）→ `Vec<Diff>`（类型化：`Value`/`Arity`/`OnlyInA`/`OnlyInB`）。一趟跑完所有行，**不早停** |
| 归一 | `spinduck/src/norm.rs` | **唯一**一份口径实现：`None→"NULL"`、`trim`、去 `+` 前缀、`.5` 补 `0.`、尾零与尾点削掉。黄金层与 `compare` 共用，两侧各自 `pub use` 指回来 |

口径：

- **落盘的是文本，裁决时归一**：单元格存内核给回的原样字符串，`compare` 才按值判定。
  于是同一份快照既能和新快照比、也能和人手写的基线比，而**口径改了不必重跑查询**——
  反过来（落盘即归一）会让每次调整比对规则都要重出一遍结果，千万级面前不可接受。
  宽容面：手写基线里 `600000` 与 `"600000"` 落到同一个值（`Number`/`Bool` 按字面文本收下）。
- **NULL 与空文本在载体里区分、在判定里同值**：`null` 落盘、`""` 落盘，`norm` 后都是
  `"NULL"`。少了前者就没法事后追问"这格到底是空串还是缺值"。
- **判值不判文本；列名不是判据，列数是**：参照侧与本地给回不同别名（`cnt` vs `n`）不是
  差异，`DIFF value` 的 `col=` 一律取**左快照**的列名（没有列名时退到 `col{n}`，报告里
  不许出现空列名）。
- **行序默认是判据，`--key` 下不是**：实例侧不保证行序。键列按大小写不敏感定位，
  **键列找不到即 `ENV-0031`，绝不退化成位置序**——把"没比成"说成"比过了"是本命令最危险的
  静默形态。这条刀**两侧各过一遍**（`key_indices_both`）：只校验本地侧时，参照侧改名会按位置
  读到错列当键（值恰好对上 ⇒ `diffs=0` 的假绿），换位或参照侧行更窄会让键恒 `NULL`（⇒ 整片
  `OnlyIn*` 洪水，报因指错方向）。同理，同一键列在两侧必须同位——键只定位行、不重排列，
  列序不一致时"第 j 列对第 j 列"本来就没有意义，判红而不是报成 `DIFF`（实证 V37）。
  重复键：排序稳定，同键的几行按文件序两两配对。
- **多语句必须拒**（一手发现，validation V35.2）：DuckDB 的 `prepare` 对 `SELECT 1; SELECT 2`
  **不报错**，它把两句都执行、只把最后一句的结果集给回来。若"交给内核报红"，两行的 `.sql`
  会得到一份只覆盖第二句的快照，而第一句若含写入已经落进库。故切句在 core：只在 `'…'`/`"…"`
  （`''`、`""` 为转义）、`--` 行注释、`/* */` 块注释**之外**的分号处切，切出来多于一条即
  `ENV-0031` 且一条都不发给内核。
- **判红不得留下产物**（与 §13.6 同一条纪律）：读 SQL 文件排在开库之前（`open` 即建文件），
  写 `--out` 排在查询成功之后（半份快照会被下一步 `compare` 判成"两边都空 ⇒ 一致"）。
- **内存天花板是明写的限制**：`capture` 把结果集整体驻留——`RowStream` 的契约就是快照。
  所以 `query` 面向**结果裁决**（聚合输出、小表全量），千万级取数属 design A3 的
  COPY OUT 流式改造；本轮不为它改 `SqlExecutor` 接缝签名（Ask first，§1）。
- 错误码分工：读不到文件 = `ENV-0010`，快照 JSON 不合约定 / 语句形状不对 =
  **`ENV-0031`**（与 `case.toml` 缺 `[case]` 段同族，"输入形状不对"这一类），
  内核报错原样带出（`KERNEL-*`，§5 归域在产生点）。口令类参数一个都没有：`query`
  只接本地 DuckDB 文件，实例侧采集属 A0b/A1/A3。
- 归一收口的性质：这是**纯去重**，两份副本的语句序列与 core 实现完全相同，只差形参名与一行
  注释（diff 原文见 V35.3）。等价性的两手证据在 V35.3/V35.6（corpus 层、进程内 cases 层、
  真容器黄金层各自全绿，期望值一个没收），且 `-.0 → -0` 这一既有怪癖被新单测原样锁定，
  而不是"顺手修正"。

退出码与通道沿用 §1 CLI 契约：`query` `0` 快照已落盘 / `1` 语句或形状错 / `2` 用法错；
`compare` `0` 无差异 / `1` 有差异或键列找不到 / `2` 用法错。全部输出走 stdout，stderr 恒空。


> 决策记录 2025-11-05（PR#1 评审）：① COMMIT/ROLLBACK 实现统一事务边界（最外层显式事务，Tx 语句即时提交/回滚并重开，未处理异常统一 ROLLBACK，DuckDB ACID 承载原子性），不降级；② ENG-0062 撤销整体拒绝，动态文本统一经 ogsql 解析管线（解析失败 BIZ-061；解析器不支持的形态向 ogsql-parser 上游提 issue）；③ 内置规则 `config/rewrite-rules.toml` 编译期嵌入，`RuleRegistry::default()` 直接加载，CLI/项目规则叠加其上；④ 容器口令经 `.env` 注入（`${GS_PASSWORD:?}`），端口仅回环；
> ⑤ `DuckExecutor::connection()` 暴露内核连接句柄——**Ask-first 决策记录（2025-11-05）**：
e2e 测试（`tests/gauss_e2e.rs` 直取内核句柄校验 GaussDB 侧落库）需要内核事务/批量能力，
`SqlExecutor` 接缝尚未提供对应方法（扩接缝 = 破坏所有消费者，需单独评审）；接受为**过渡态**，移除条件 =
回写通道参数化改造（评审 PR#1 行内评论）落地时收敛，届时 `gauss.rs` 不再触及
duckdb 类型；⑥ fastaas 子模块保留现状（私有项目）。

> 决策记录 2026-11-06（PR#1 第三轮复审后自行修复）：
> ⑦ **静态/动态 SQL 改写收口为一条管线**：`ir::rewrite_sql` 成为唯一入口（`[patterns]`
> → 调用点下沉规划 → token 规则改名 → AST 变换），动态路径不再自写第二份顺序
> （曾为"改名→AST 变换→规划"，与静态相反）。`[patterns]` 的 AST 实现集中到
> `src/sql/patterns.rs`；两路径产物一致性由 `tests/guard/guard_sql_pipeline_parity.rs`
> 锁定（大小写/空白差异属文本来源不同，按"去空白 + 小写"归一化比对）。
> 覆盖面同时扩到 `INSERT … SELECT` 子查询的 `rownum = 1`（语料主形态，此前只在
> 顶层 SELECT 生效，原样下发必然内核报错）；UPDATE/DELETE 的 rownum 无对应
> DuckDB 语法位，两口径一致地原样下发，留 M8 处理。
> ⑧ **注册静默丢弃护栏 `ENG-0064`**：文本声明了例程（`CREATE … PROCEDURE|FUNCTION|PACKAGE`）
> 而解析后零登记时报 ENGINE 域错误——ogsql-parser 对个别 PL 构造（如 PL 条件里的
> `IN ()`）会把整条 `CREATE PROCEDURE` 降级为 `Empty`，此前 `register` 返回 `Ok(0)`，
> 语料接入时表现为"存过凭空消失"（§1：解析器行为只记录，不在其仓库内动手）。
> 包规格（spec）合法地不产出例程名，护栏显式豁免。
> ⑨ **回滚失败的归域**：`Engine::call` 统一 ROLLBACK 失败时并入原错误消息，
> 主因保持原域与原错误码（§5 禁止在调用上层事后改判），不得改写为 KERNEL。
> ⑩ **CI 审计豁免**：`zip 6.0.0` 的四条公告（RUSTSEC-2026-0367/0368/0369/0429）来自
> `libduckdb-sys` 的 **build-dependency**（`bundled` 解压缩包用），不进运行时产物；
> 升级它即违反 §1 内核版本锚定（`duckdb = "=1.10505.0"`），故 CI 显式 `--ignore` 四条，
> 内核升级 PR（V6 门禁）必须复评该清单。
> ⑪ **CI 口令与目录**：golden job 的 GaussDB 口令改为 runner 内随机生成（一次一密，
> 不落仓库），三条子模块步骤显式 `working-directory: .`（`defaults` 为 `spinduck/`，
> 在 crate 目录内 `git submodule update --init lib/ogsql-parser` 报 pathspec 不匹配）。
> ⑫ **数据通道驱动换为 git 依赖 `gaussdb`（决策 2026-11-07，validation V24）**：纯 Rust
> `postgres` 不认 openGauss/GaussDB 的 sha256 口令认证（握手即 `error parsing response
> from server`），故 `io/gauss.rs` 改用 `rust-opengauss` 的**同步门面** `gaussdb::sync::*`，
> `postgres` 依赖整体移除（两套 PG 线协议客户端并存会让 golden 与物化走不同认证面）。
> 三条约束：⑴ `default-features = false` + 只开 `sync`（运行期不需要的类型扩展与 TLS
> 连接器不进来，v1 仍走 `NoTls`）；⑵ `rev` 钉到 `b95ed0b9…`（与 hepta-dbcli 同基线），
> 升级走独立评估 PR，同 §1 的 ogsql-parser/内核锚定纪律；⑶ **`cargo audit` 对 git 来源
> 依赖没有公告面**——该 crate 不在 CI 审计覆盖内，属新增盲区，复评方式为 rust-opengauss
> 上游打 tag / 发 crates.io 后切回版本依赖；在此之前每次升级 rev 必须人工过一遍
> 认证与线协议改动面（V24 探针 `--features gauss-sha256` 是唯一机械化入口）。
>
> ⑬ **发版产物落地为 `release.yml`，Linux 走 zig 锚定而非低 glibc 容器（决策
> 2026-11-07，validation V25）**：AGENTS §9 此前把 Platform Build 写成"CI 的一部分"，
> 但仓库里并不存在该 job——本轮把它变成实物，范围按用户点名取 **3 个目标**
> （Linux x86_64 / Linux aarch64 / Windows x86_64），macOS 双架构与 Win7 显式登记为
> 未覆盖，不允许再被读成"§10 矩阵已一一对应"。取舍与依据：
> ⑴ **glibc ≥2.28 不由构建工具承诺，由产物反查**——`cargo zigbuild --target
> ....2.28` 只是请求锚定，真正判定权交给 `scripts/ci/check_max_glibc.py`（读
> `readelf -V` 的版本化符号取最大值，超 2.28 即红）。两目标在 rockylinux:8（glibc
> 恰 2.28）里实测上限 = `GLIBC_2.28` 且冒烟通过。
> ⑵ **"在 glibc≤2.28 环境原生构建"这条备选路当前不可用**：Rocky 8.10 的 aarch64
> AppStream repodata 在全网镜像 404，容器内 `dnf install` 直接失败；不是路线本身错，
> 是上游镜像源状态，若日后恢复可重新评估（届时应换成能装下工具链的镜像再测）。
> ⑶ **cargo-zigbuild 版本是硬下限 0.23.4**：0.22.1 在 aarch64 链接期失败——rustc 对
> aarch64-unknown-linux-gnu 无条件传 `-Wl,--fix-cortex-a53-843419`，zig cc 报
> `unsupported linker arg`。版本与 zig 0.15.2 一起钉在 workflow 顶部 env。
> ⑷ **duckdb bundled 的 C++ 交叉编译可用**（`bundled` → cc 路径，非 cmake；
> `buildtime_bindgen` 未开，故不需要 libclang），这是 §10 "某一平台不可用 = Ask first"
> 的第一份实测证据，M1 当初并未逐平台跑过。
> ⑸ **Windows 不加 `+crt-static`**：静态 CRT 只属 §10 的 Win7 行；代价是产物依赖
> VC++ 2015-2022 x64 运行库，该前提入 README 已知限制而不是假装自包含。
> ⑹ 发布动作只认 `push` tag，`workflow_dispatch` 只构建上传 artifact——干跑不得
> 产生真实发布物；Release job 断言资产数量 == 目标数量，禁止部分产物静默缺失。
