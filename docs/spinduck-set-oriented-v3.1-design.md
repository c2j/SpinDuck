# SpinDuck 集合化判定前移设计 V3.1（并入 `compile --admit`：编译期可见、可审计、可判红）

> **基线依据**：`c2j/SpinDuck` main `867a731`（v0.2.4，已含 PR #59 多区域 `RunSpan` / `LookupTuple` 与 PR #60 v3 验证包）。
> **实证依据（F4 勘误，必读）**：本文引用的「purchase 在 `COMMISSION_SWITCH='1'` 下由入口路由守卫
> 拒绝（`UnsupportedControlFlow`）、OFF 与 `batch-atomic` 结果逐列等值、分支体互换探针报
> `ExceptionScopePresent`」三条，**均来自 2026-11-23 会话内的手工取证（真机 `aaspubs.duckdb`），
> 尚未入仓**：`docs/validation.md` 现止于 V51，V52 是**本文预留的编号**，由 PR-4 写入。
> 仓内现存的可查记录是另一组：`golden/pr48_set_oriented_verify/REPORT.md:188` 与
> `corpus/README.md:46` 记同一例程 `Skipped(ExceptionScopePresent)`，其数据侧是 `kind_id='0'`
> 的脱敏语料、版本侧是 v0.2.1。**两个拒因不是一回事，也不互相替代**——所以 §12 的第一步是
> 在当前 main 上把两个象限重测一遍（§12.4 G5），在此之前本文任何「已实证」字样都按
> 「会话内证据」读。语料侧函数已补件（V51.1 第 3 条恢复 `1_FNC_COM_GET_SEAT_PARA` 源码），
> 但 `par_fund_seat_para` 仍无数据行（`cases/datasets/seed.sql` 的 INSERT 清单里没有该表）。
> **上一版**：`docs/spinduck-set-oriented-v3-design.md`（多区域转换与安全协同）。本版**不新增子命令**，判定入口按决定并入 `compile`，由旗标控制。
> **本版范围（三次修订后）**：§1–§11 只前移「判定的产生时机」与「可见性/可门禁化」——不改准入规则、生成 SQL、回退语义与结果等价性，**零语义风险**；§12（D7）**已决定纳入本版**，是唯一带语义变更的工作包：它把 IF 路由从「证明 THEN 不可达」改成「按判别式取值选臂」，只**扩大**可转面，并以「ELSE-only 语料生成 SQL 逐字不变」+ OFF/ON 四象限等价作为放行门（§12.5）。两半独立 PR、独立回滚点。

**风险声明（三条，不得省略）**

1. **§3–§7 的证据面是「可转性 + 结构/数据形状守卫」，不是业务正确性。** 空库、stub 库与真实数据可以给出不同结论，所以这些章节**不得**用来验收「结果正确」。业务正确性只在 §6 Phase 2（golden 等价基线）与 §12.5 门 3 判定。
2. **本版不关闭 D4。** `convert` 级判定依赖同机参照实例（memory: openGauss reference instance——amd64 postmaster 在过程调用上崩、arm64 runner 起不动镜像）。参照面缺席期间，转换结论一律是 `conditional`，不是 `verified`。
3. **§12 属 AGENTS 的 Ask-first 面**（守卫语义变更 + `SkipReason` 新变体 + 人类 fixture/基线变更），落地前须逐项过 §12.7 的请示清单；本文档不构成批准。
4. **本文引用的 V52 证据尚未入仓**（见 §0 实证依据与 §12.4 G5）：凡依赖「purchase 现在到底报什么」的判据，一律以 PR-5 第一步的重测为准，不得把本文当基线。

---

## 1. 问题定义：三个缺口

复现命令（会话内手工取证，待 PR-4 落 validation V52）：

```bash
spinduck call --db aaspubs.duckdb --set-oriented=batch-atomic \
  --plan .spinduck/plan/pkg_split_trade_step3_sh.prc_trd_deal_purchase.toml "'20260105'" null
# → 33.883s 后：SET_ORIENTED … skipped reason=UnsupportedControlFlow / OUT p_o_succeed = 0
```

| 缺口 | 事实 | 代码坐标 |
| :--- | :--- | :--- |
| **G1 时机** | 判定其实发生在踏入 `open_pc` **之前**（事务此刻完全干净），但**打印发生在整次调用返回之后**——`call_engine` 在 `call_with_sink` 返回后才调 `render_set_oriented_reports` 读 `engine.set_oriented_reports()`。于是一行结论要等 33.9s | `cli/run/call.rs:287` → `:464-481`；守卫链 `ir/set_oriented/entry.rs:113-168` |
| **G2 可见性** | 项目态 `compile` **完全不做**集合化分析：`ProjectCompile` 无 `--db`、无 `--set-oriented`，`compile_with_source` 全程无 `set_oriented` 调用（只有 `gate2b` 的 stub prepare）。用户在 compile 阶段拿不到任何集合化结论 | `cli/run/compile_project.rs:19-32`、`:112-150`；`cli/args/compile.rs:42-99` |
| **G3 静默** | `Skipped` 只输出一行且退出码不变（现规则只看 `missing==0 && failed.is_empty()`），无法进门禁。发布前把关形同虚设 | `cli/run/compile_project.rs:145-149` |

> **附带结论（口径按 §0 实证依据限定后使用）**：`UnsupportedControlFlow` 这一条**不是** IR 形态不支持，而是**数据路由**：`swh_all_kind.kind_id='1'` 命中 1 行时入口路由守卫判「THEN 分支可达」→ 保守退回。这类判定本质是数据的函数，与参数无关，因此**可以**在编译期判——前提是有数据面。

---

## 2. 判定分层与「编译期可判」的准确边界

| 层 | 内容 | 现有实现 | 编译期可判 | 需要数据面 |
| :--- | :--- | :--- | :--- | :--- |
| **L1 形状证明** | 区域抽取、trap 深度、IF 剪枝、写形态、符号折叠；无执行器、刻意保守 | `ir/set_oriented/candidate.rs:66-80`（`schema::Source::Static(snapshot)`） | ✅ | 否 |
| **L2 带执行器重证** | verbatim 列探测、视图定义展开；离线失败后以运行期为准 | `engine/set_oriented.rs:288-304` | ✅ | 是（需真实 schema） |
| **L3-a 无绑定守卫** | `sys_dummy` 恰 1 行、源/目标/查找表结构漂移、快照一致、**路由守卫** | `entry.rs:119`、`:122-133`、`:146-155`；SQL 见 `route.rs:49-60`（`ex.query(guard, &BindMap::new())`） | ✅ | **是** |
| **L3-b 随帧守卫** | Top-1 并列、内联表达式全定义、`fn_preludes`/`view_preludes` | `entry.rs:135-145`、`:156-166`；字段 `ConversionPlan.top1_guards/expr_guards/fn_preludes/view_preludes`（`entry.rs:59-79`） | ❌ 只登记不判定 | — |

**判定制**（`Verdict`）：

* `skipped(reason)` —— L1/L2 失败，或任一 L3-a 被否证。此结论**与入参无关，编译期即可下**（purchase 正落在此）；
* `conditional` —— L1/L2/L3-a 全通过，仅剩 L3-b 未决。含义是「静态部分已证，剩余部分逐次调用自证」；
* `converted` —— 无任何守卫（无数据依赖、无帧依赖）。

> D7（§12）合入后本表只变一处：`route` 槽位从「证明 THEN 分支不可达」变成「取判别式实际值 → 选臂」。
> 档位口径不变——取值不可证或选中臂不可转，仍是 `skipped`，只是 reason 更具体；`arm` 作为 verdict 的
> 附加字段随判定一起出（§12.3(e)），不进档位判定。

### 2.1 Never：绝不在 stub 库求值 L3（本版最重要的一条）

Gate 2B 已用 `DuckExecutor::open_in_memory()` + 闭包 DDL 建内存 stub 库（`gate/prepare.rs:301-333`，`compile_project.rs:321-327`）。**这套 stub 不能拿来判 L3**，理由是它会同时造出假绿与假红：

* **假绿**：路由守卫 `SELECT ((CASE WHEN <hit> THEN <value> ELSE <default> END) = '1') IS NOT TRUE`（`route.rs:49-60`）。**前提是 stub 有表无行**（Gate 2B 会按闭包 DDL 建出 `swh_all_kind`，`gate/prepare.rs:313-333`，但不灌数据）→ `hit` 聚合为 NULL → `CASE` 落 `<default>` → `(<default> = '1')` 恒不为 TRUE → `IS NOT TRUE` → **TRUE，守卫"通过"**。守卫设计成全定义不抛错，正因为它全定义，无数据时**恒真**；`guard()` 对 `key_checks` 非空还有一道 `UnsupportedControlFlow` 拒绝（`route.rs:50-52`），也不改变这个结论。D7 之后同一份渲染证据改产**判别式取值 SQL**（§12.3），空 stub 上取到 NULL → 按 §12.3(d) 第 4 步保守回退，不会假绿；
* **假红**：`sys_dummy_ok` 要求恰 1 行（`entry.rs:119`），stub 无行即判 `UnprovenSysDummyInvariant`；
* **同义反复**：`drifted`/`snapshot_matches` 是「运行时库 vs 生成期快照」的比对，stub 与快照同源，恒等成立，不构成任何证据。

因此：**无 `--db` 时整个 L3 一律 `Unchecked`（只列清单、不出判定）**，且 `admit` 绝不退回用 stub 求值。

推广为一条总原则（F1/F2 修订）：**没有对应取证面就不求值，也不下结论**——

* 无数据面（executor=None）→ L3 全 `Unchecked`；
* 无帧（admit 永远无帧）→ 帧守卫 `Unchecked`；
* 无 preludes 物化（admit 不物化）→ 引用 `sd_fn*`/`sd_bf*`/`sd_vw*`/`sd_cur` 缓存表的守卫 `Unchecked(prelude-dependent)`（§4.2）；
* L1 拒因属"探测不可证"路径 → `conservative`，不作 firm `skipped`（§4.2 F2）。

---

## 3. CLI 设计：`compile --admit`（不新增子命令）

### 3.1 旗标集（扩 `Command::CompileProject`，全部置 `project_mode = true`）

```
--admit[=report|error]                     判定开关；默认关。=report 只报告；=error 使 firm Skipped 参与退出码
--db <f>                                   admit 的数据面（真实库或部署产物）；是否强制见 D9
--set-oriented[=off|strict|batch-atomic]   覆盖 [engine].set_oriented，优先级 CLI > 项目（同 call.rs:416）
--admit-record <f>                         判定工件输出路径；显式给才写，不隐式写项目树
```

约束（全部沿用 `compile` 现有旗标纪律，`args/compile.rs:42-99`）：

1. 新旗标属**项目态**：与裸路径共存仍是 `ENG-0010` 用法错（`:23-25`）；重复单值旗标 `dup_flag_err`（`:105-111`）；非 `--admit` 时给 `--db`/`--admit-record` → 用法错（不静默忽略，保持 compile「不接受旗标」的既有严格性 `:92-94`）。
2. **native 门**：`--admit` 需要内核句柄。非 native 构建显式打 `admit=skipped(no-native)`，镜像 `gate2b=skipped(no-native)` 的既有口径（`compile_project.rs:130-134`）——**不假装门禁通过**。
3. 入口目标沿用 `--entry`/`[[entry]]` 解析（`compile_project.rs:157-176`），不引入新的目标语义。
4. 计划文件模式不在本版：`call --plan` 的判定仍由 `call` 自己给；`compile --admit` 走项目态（闭包目录与快照已在手，见 §3.4）。
5. **`--admit` 与 `--db` 的搭配**：若 D9 采纳强制，`--admit` 不带 `--db` → 用法错（与约束 1 同口径，不静默降级）；`conservative` verdict 不参与 `--admit=error` 判红（§3.3）。

### 3.2 stdout 逐字规格

现有行**一字不改**（`plan:`/`REF`/`MISS`/`DYN`/`FAIL`/`compile:` 摘要行）。判定块插在 `compile:` 摘要行**之后**：

```text
admit: targets=1 converted=0 conditional=0 skipped=1 conservative=0 unchecked=0 native=on
ADMIT pkg_split_trade_step3_sh.prc_trd_deal_purchase skipped reason=UnsupportedControlFlow
ADMIT   guard=sys-dummy checked=compile state=passed object=sys_dummy expect="恰 1 行"
ADMIT   guard=drift checked=compile state=passed object=swh_all_kind expect="列集/限定名与生成期证据一致"
ADMIT   guard=snapshot checked=compile state=passed object=swh_all_kind expect="与编译期快照一致"
ADMIT   guard=top1 checked=frame state=unchecked object=par_fund_seat_para binds=seat_code,inure_begin_date,inure_end_date
ADMIT   guard=route checked=compile state=failed object=swh_all_kind expect="THEN 分支不可达"
ADMIT     sql=SELECT ((CASE WHEN … END) = '1') IS NOT TRUE
ADMIT   guard=expr checked=frame state=unchecked
admit: 结论 = 当前数据下不可集合化；将 swh_all_kind.kind_id 由 '1' 改为非 '1'（或 0/≥2 行/异值）后可转
```

* **同源铁律**：第 2 行 `ADMIT <routine> skipped reason=X` 的 `<routine> skipped reason=X` 片段与 `call` 的 `SET_ORIENTED <routine> skipped reason=X` **必须是同一格式化函数的两个前缀**（从 `call.rs:464-481` 抽出 `fn verdict_line(prefix, routine, &Status)`）。判定口径出现两套 = 本设计失败。
* **reason 口径**：`reason=` 取**静态面首个失败守卫**对应的 `SkipReason`。`entry_ok` 原始次序中排在该守卫之前的帧守卫（top1/expr）admit 无法求值，运行期若它们也失败，`call` 报的 reason 可能与 admit 不同——但两者同为 `skipped`，判红语义不受影响；测试 6 的逐字断言只在无此歧义的 fixture 上成立（§4.2 详述）。
* 守卫清单按 `entry_ok` 原始求值序输出（sys-dummy → drift → snapshot → top1 → route → expr），不按 scope 分组。
* `conservative` verdict 单独渲染为 `ADMIT <routine> conservative reason=<Debug>`——**不**走 `verdict_line`，不计入 `skipped`，不参与 `--admit=error`（F2）；`unchecked` 原因后缀取值域 `{frame-bound, prelude-dependent, macro-dependent, no-db, probe-unavailable}`（§6 工件同口径）。
* 末行「结论」是给人看的话术层，只在 `skipped` 且命中 L3-a 数据型守卫时打；不参与契约断言（决策项 D5）。
* `unchecked` 计数让「没判」与「判过」在摘要行即可区分，避免误读为通过。
* **本块示例是 D7（§12）落地前口径**，合入后 purchase 在 `kind_id='1'` 下的形态改为「判别式取值 → 选臂」：
  末行「结论」里「将 kind_id 由 '1' 改为非 '1' 后可转」的说法随之作废（改为「当前数据下选 then 臂」或报该臂拒因）；
  verdict 行与守卫行各加 `arm=then|else|none` 字段（§12.3(e)）。**即使 THEN 臂最终仍不可转**，
  reason 也从「路由不可达证明失败」变成**该臂自己的具体拒因**（§12.4）——可诊断性本身就是 D7 的收益。

### 3.3 退出码

现规则（`compile_project.rs:145-149`）：`missing == 0 && failed.is_empty() → EXIT_OK`，否则 `EXIT_FAIL`。

* `--admit` / `--admit=report`：**不改退出码**（零回归）；
* `--admit=error`：追加 `&& admit_firm_skipped == 0`——只数 `confidence=Firm` 的 `skipped`；`conservative` 与 `conditional` 均不判红（F2/D1）。把 conservative 判红会让所有含探测依赖拒因的语料在无重证面时误红；
* 内核/环境失败仍走 `Err(EngineError) → EXIT_FAIL`（`cli/run/mod.rs:40-46`），不新造错误码段（AGENTS §5 故障域不变）。

### 3.4 落点：catalog 复用 + codeweb 快照另取（注意：不是 `gate.snapshot`）

`ir_failures` 已在项目态用 `Engine::new(Box::new(NoopExecutor), rules)` 注册了整个闭包（`compile_project.rs:284-303`）。把它的 `Engine.catalog`（`pub` 字段，`engine.rs:30`）提到调用层，即得 L1 的第一个入参 `&RoutineCatalog`。

**第二个入参不能用 `gate.snapshot`**——那是 `gate::schema::SchemaSnapshot`（闭包 DDL 文件 AST 构建，`gate/schema.rs:49`），与 L1 所需的 `ir::set_oriented::metadata::SchemaSnapshot`（codeweb `SchemaExport` 构建，`metadata.rs:9/52`）是两个类型、两种来源。正确来源是现成的 `plan::codeweb::schema_snapshot(codeweb_root, rules)`（`plan/codeweb/source.rs:78-89`）：再开一次 `EmbeddedProject` + `export_table_schemas`，只读、不碰内核。`codeweb_root` 用 `codeweb_project_root(project)`（`plans.rs:48-63`，与取图同口径）。

规则口径：快照与 `compile_set_oriented_plans` 的 `ctx.rules` 都取 `catalog.sql_rules()`——与 `call` Routine 形态的 `discover_rules_files` → `with_project_rules` 同链（`call.rs:399`、`compile.rs:25-28`）。已知口径差：`call --rules` 手动覆盖 compile 没有对应旗标，显式覆盖时 admit 结论可能与该次 call 不同——记入文档，不为此给 compile 加 `--rules`（超出本版范围）。

测试接缝（关键）：`schema_snapshot` 绕过 `CodewebSource` 直接开真实 codeweb 项目，`JsonCodewebSource` 夹具（现有测试大量使用，只写空 `codeweb.toml`）供给不了 `SchemaExport`。因此 admit 数据面定义为可注入结构：

```rust
/// admit 的数据面注入（生产 = codeweb 快照 + 可选真实库；测试 = 字面量构造）
pub struct AdmitInput {
    pub snapshot: metadata::SchemaSnapshot,          // 字段全 pub，测试可直接构造
    pub executor: Option<Box<dyn SqlExecutor>>,      // Some = --db 数据面；None = 纯静态
    /// 已宏化纯函数名集（Engine::macro_names 只读访问器的产物）：
    /// 守卫 SQL 命中 → Unchecked(macro-dependent)，见 §4.2 F3 闸。
    pub macro_names: BTreeSet<String>,
}
```

`compile_project_cmd` 生产路径：`AdmitInput::open(project, catalog.sql_rules(), db, macro_names)` 内部调 `codeweb::schema_snapshot` + `DuckExecutor::open`；`macro_names` 与 `catalog` 一并从 `ir_failures` 的 Engine 取出（`install_sql_macro` 在 NoopExecutor 下仍会登记名字——`execute` 空转返回 Ok，引擎侧 `macro_names` 集合是完整的，`engine.rs:191-205`）。`compile_project_with_source`（pub，测试注入图 JSON）增加 admit 姊妹入口 `compile_project_admit_with_source(opts, source, admit: AdmitInput, out)`，旧函数委托 `admit=None`——**不改既有 pub 签名**（人类测试只读）。

```text
compile_project_cmd
 ├─ plans_from_source → write_plans → render_plans        （不变）
 ├─ ir_failures → (failed, catalog)                       （返回 catalog，不改语义）
 ├─ admit_input = AdmitInput::open(project, catalog.sql_rules(), db?)
 ├─ admit_verify(catalog, snapshot, mode, executor?)      （新增：L1 + L2 + L3-a；L3-b 只登记）
 ├─ 摘要行 + ADMIT 判定块 + admit: 结论行                   （新增渲染）
 └─ 退出码 = 现规则 ∧ (--admit!=error ∨ skipped==0)        （唯一门禁变化）
```

---

## 4. 库级接缝：同一份判定代码，两个入口

### 4.1 `entry_ok` 拆分（PR-1，行为零变更）

现状把六类守卫串在一个函数里且需要 `frame`（`entry.rs:114-169`）。**关键约束：静态守卫与帧守卫在原始次序中是交错的**——sys_dummy → drift(源/目标/查找) → snapshot → **top1（帧）** → **route（静态）** → **expr（帧）**。拆成"静态段 + 帧段"两阶段会改变求值次序：top1 与 route 同时失败时，现行报 `AmbiguousTop1Tie`，静态段先行会先命中 route 报 `UnsupportedControlFlow`——`SkipReason` 可观察面变了，违反零变更。

因此拆分单位是**守卫槽位（guard slot）而非阶段**：

```rust
/// entry_ok 原始次序的守卫清单；每个槽位知道自己要不要 frame
pub(crate) struct GuardSlot {
    pub kind: GuardKind,        // SysDummy | Drift | Snapshot | Top1Tie | Route | ExprTotalness
    pub scope: GuardScope,      // Static | Frame
    // 求值所需数据（SQL 原文 / 涉案对象 / 期望值），与现有字段一一对应
}

// 单个守卫求值：EngineError 一律 ? 上抛；逻辑拒绝 → Ok(Err(SkipReason))
pub(crate) fn eval_guard(ex: &dyn SqlExecutor, slot: &GuardSlot,
                         ctx: &OptimizeCtx<'_>, frame: Option<&Frame>)
    -> Result<Result<(), SkipReason>, EngineError>;

// entry_ok = 按 GuardSlot 原始次序逐个 eval_guard，首个不合格者返回
// （求值次序、短路点、SQL 文本、拒因映射逐字不变）
pub(crate) fn entry_ok(...) -> Result<Result<(), SkipReason>, EngineError> { … }
```

`GuardOutcome`（admit 报告用）：

```rust
pub(crate) struct GuardOutcome {
    pub kind: GuardKind,
    pub object: String,       // 涉案表/视图（未限定名，人类可读）
    pub sql: String,          // 守卫 SQL 原文（逐字可复核；sys_dummy/drift/snapshot 为目录探测，sql 记探测语句）
    pub binds: Vec<String>,   // 空 = 无帧依赖
    pub scope: GuardScope,    // Static | Frame
    pub state: GuardState,    // Passed | Failed(SkipReason) | Unchecked
}
```

admit 侧消费方式：**静态槽位全量求值**（守卫已证全定义不抛错、只读），帧槽位一律 `Unchecked`；verdict 的 reason 取原始次序中首个 `Failed` 槽位——若其前存在帧槽位，运行期 reason 可能被帧守卫抢先（§3.2 reason 口径）。

**求值面差异（必须写明，防 trace 对账误判）**：admit 与 call 的守卫求值面不同——call 在 `entry_ok` 内**短路**（首个失败即返回，不再发后续守卫 SQL），admit 为给出完整清单**全量求值**全部可求值的静态守卫；且 admit 不发 `TraceSink` 事件。两者发出的守卫 SQL 序列与条数本就不同，拿 trace 对账会判成不一致——此差异是设计使然而非缺陷，写进 `docs/architecture.md` 候选说明。

**硬约束（继承 v3 §2.1 不变量 3，不得松动）**：`eval_guard` 里内核错误一律 `?` 上抛（外层 `EngineError` → `call_with_sink` 回滚），**只有**逻辑拒绝才映射成 `SkipReason`。拆分过程严禁把 `EngineError` 顺手降级为 `Skipped`——那是把「事务可能已脏」伪装成「保守退回」。admit 侧同理：静态守卫求值遇 `EngineError` 整体 `Err` 上抛，不产出半份判定。

### 4.2 判定 API

```rust
// src/ir/set_oriented/admit.rs（新模块）
pub struct AdmissionVerdict {
    pub routine: String,
    pub verdict: Verdict,                 // Converted | Conditional{unchecked} | Skipped(SkipReason) | StaticOnly
    pub confidence: Confidence,           // Firm | Conservative —— 与 verdict **正交**的字段，不塞进变体（F2）
    pub guards: Vec<GuardOutcome>,
    pub note: Option<String>,             // §3.2 的「结论」话术（仅数据型守卫失败时）
    pub arm: Option<RouteArmTag>,         // §12.3(e) 双臂计划才非空；今天形态恒 None
}
/// confidence 混进变体（`Skipped(SkipReason, Firm)` / `Conservative(SkipReason)` 两套写法）
/// 会让工件与 stdout 口径漂移；变体只描述结论，档位单独一列。
pub enum Verdict { Converted, Conditional, Skipped(SkipReason), StaticOnly }
// 单一入口：executor = None → 纯静态面；Some → L2 重证 + L3-a 全求值
pub fn admit_verify(catalog: &RoutineCatalog, admit: &AdmitInput,
                    mode: SetOrientedMode, entries: &[String])
    -> Result<Vec<AdmissionVerdict>, EngineError>;
```

* **F2 — L1 拒因分 firm / conservative 两档（本版最重要修正）**：`compile_set_oriented_plans` 的 `Err(SkipReason)` 混合了两类拒因——形状类（区域抽取、mode 门、IR 形态、`DynamicSql` 等不涉执行器探测的，v2_admit/analyze_region 阶段产出）与探测依赖类（`generate()` 消费 `Source` 探测而不可证）。**探测面口径（三次修订勘误，逐条实测）**：`Source::Static` 下 `view_sql` → `None`、`object_names` → `None`、`probe_columns` → `None`（`schema.rs:52-79/81-83/85-87`），而 `sys_dummy_ok` → `Ok(true)`（`schema.rs:61-65`，注释「数据基数不是静态事实，必须延迟到区域入口检查」——**它是延迟，不是不可证**，不得计入探测缺口）。不可证即保守 `UnknownObjectDependency` 等（`resolve.rs:70/77/91`，视图另在 `:83-90` 改引 `sd_vwN`）。
  **判档依据必须是「本条拒因是否由探测缺口导出」，不能是「(阶段, 原因) 白名单」**——同一个 `UnknownObjectDependency` 有两义：`resolve.rs:70/77/91` 是探测缺口（executor=Static，必须 conservative），`resolve.rs:101-103` 的列缺失在 `Source::Runtime` 下是真实结构缺陷（必须 firm）；Static 下该点理论上也可达（`static_table` 列集来自快照），故 `evidence_gap` 标记必须挂在 `view_sql`/`object_names`/`probe_columns` 返回 `None` 的**调用点**上逐跳上抛，而不是按行号或阶段硬编码——非探测缺口导出的拒因默认 firm，探测缺口导出的默认 conservative。白名单会把真缺陷一起降成不判红，门禁静默变松。
  实现：给 `schema::Source` 的三个探测方法加显式缺口标记并沿调用链上抛（`SkipReason` 之外带 `evidence_gap: Option<EvidenceGap>`，或 `plan_from` 出口按来源分档）；`GuardOutcome`/`Verdict` 携带 `confidence: Firm | Conservative` 并写入工件。**executor=Some 时走 L2 运行期重证**（`engine/set_oriented.rs:288-304` 既有口径），重证结果即 firm——这条不变。
  Conservative verdict 的 stdout 渲染为 `ADMIT <r> conservative reason=X`（不走 `verdict_line`，不计入 `skipped`、不参与退出码）。这正是本次手工取证里 v0.2.3 离线恒报 `UnknownObjectDependency`、挂执行器后才走到 route guard 的同一机制（memory: purchase switch admission）。
* **F1 — prelude 依赖守卫闸**：`preludes_first` 计划的守卫 SQL 可内嵌 `sd_fn*`/`sd_bf*`/`sd_vw*`/`sd_cur` 缓存表（`engine/set_oriented.rs:125-127`、`:159-161`；渲染侧 `resolve.rs:83-90`）。admit 不物化 preludes（§下条），这些守卫若照常求值会打到不存在的表 → `EngineError` → 按硬约束整体上抛，compile 直接失败。修法：建守卫清单时以 plan 的 prelude 表名集对守卫 SQL 做词法匹配，命中者标 `Unchecked(prelude-dependent)`、不参与求值；verdict 上限 `Conditional`，reason 不得由这些守卫导出。**不选"admit 里物化 preludes"**：`FnPrelude` 需帧解析 binds + 回调例程 + `MAX_TUPLES` 预算（`entry.rs:20-27`），admit 无帧；且 TEMP 建表下发会作废 §7 零写入结论。
* **F3 — 宏依赖守卫闸（零写入的代价补闸）**：`call` 经 `Engine::register` → `install_sql_macro` 把纯函数下发为 `CREATE OR REPLACE MACRO`（`engine.rs:194-205`、`catalog/sql_macro.rs:32`），admit 不注册 → 目标库无宏。若守卫 SQL 引用宏名，在 `--db` 上求值得内核 Catalog Error（`engine.rs:192-193` 注释明示"不静默降级"）→ 假红。修法（选方案 ii）：守卫 SQL 与 `AdmitInput.macro_names` 做词法比对（标识符边界口径沿用 `substitute_binds` 内嵌的 `ident_tail`，`mod.rs:356-360`），命中标 `Unchecked(macro-dependent)`、verdict 上限 `Conditional`。`Engine::macro_names` 需新增只读访问器（现为私有字段 `engine.rs:32`；NoopExecutor 下 `execute` 空转返回 Ok，登记仍发生 → compile 态集合是完整的，`cli/mod.rs:32-34`）。
  **代价必须量化，不能口头声称保守**：词法比对会把与宏同名的列/别名一并降档（假阴性方向是安全，但会大面积吃掉 `conditional`）。PR-3 前置验证除 fixture 生效证据外，还须在 byfund + purchase 两个 target 上打出「命中宏名的守卫条数 / 守卫总条数」并记入 validation；若命中面超过半数，改走方案 i（admit 也注册）并回到 D4/D9 复议。选方案 i 会让零写入不成立——这是它的代价，不是它的缺点。
* L1 通过者：无执行器 → `Verdict::StaticOnly`（守卫清单全 `Unchecked`）；有执行器 → L2 重证 → 静态槽位全量求值（减去 prelude/macro 依赖者）→ 帧槽位登记 `Unchecked`；静态面全过且存在未决守卫 → `Conditional`，无守卫 → `Converted`。**firm `skipped` 只来自**：形状类 L1 拒因、或数据面下静态守卫实测失败。
* **reason 取静态面首个失败**（§3.2 口径）：帧守卫排在失败静态守卫之前时，运行期 `call` 的 reason 可能不同（同为 skipped）；`--admit=error` 只看 firm verdict 不看 reason，不受影响。
* **不物化 preludes**：`fn_preludes`/`view_preludes`（`entry.rs:36-40`、`:59-62`）在 admit 中一律不执行——视图/例程缓存表是会话临时对象、`distinct_sql` 依赖游标 site 与 `MAX_TUPLES` 预算，只有运行期有意义。相关守卫标 `Unchecked(frame-bound)` 或 `Unchecked(prelude-dependent)`（F1）。
* **零业务写入（有条件成立）**：admit 只发守卫 `SELECT` 与目录探测，不经 `Engine::register`——宏 DDL 与 `spinduck_routines` 写均不下发（§7）。前提是 F3 宏闸在位；若守卫 SQL 含未登记宏名而闸缺失，Catalog Error 会上抛而非静默——宁可 `Err` 不可假绿。

### 4.3 `SkipReason` 稳定串

现枚举仅 `Debug`，20 个变体（`mod.rs:78-112`）。新增 `as_str()` / `TryFrom<&str>`，snake_case（`unsupported-control-flow`…），**只用于工件与 `--json`**；stdout 保留现有 `reason=UnsupportedControlFlow`（Debug）以守 `tests/cli.rs` 既有契约。`Status`/`Report`（`mod.rs:115-125`）形状不动。

> 写进工件后即为对外契约：变体改名 = 破坏性变更（Ask first）。

---

## 5. 判红（缺口 G3）：两条入口共享同一判定

| 入口 | 形态 | 用途 |
| :--- | :--- | :--- |
| **B1** | `compile --admit=error` | 发布前门禁（编译期把关，零业务写入） |
| **B2** | `call --on-skip=report\|error` + 项目键 `[engine].set_oriented_on_skip`（优先级同 mode） | 消除这次「33.9s 静默退回」：判定行前移到区域入口（`entry_ok` 失败点即时输出，不等整次调用返回），并让 `Skipped` 参与退出码 |
| **B3** | v3 验证包 `run/verify_chain.sh` 增 S 步 `compile --admit=error` | 使 CI 与手工同口径，PR #60 的 S5 计时步骤不再掩盖判定失败 |

* B2 的**行前移**需要一条引擎→CLI 的输出通道：判定发生在 `adjudicate_region` 内部（`engine/set_oriented.rs:162-165`），`call` 尚未返回，CLI 读不到 `set_oriented_reports()`。候选方案见决策项 D8——推荐 Engine 增加报告回调字段（不改 `TraceSink` 契约）；若改走 `TraceSink` 新事件则属接缝契约变更，需 Ask first。OFF 路径 stdout 必须零新增（AGENTS §4.1 CLI 契约层逐子命令断言；`push_report` 只在 mode≠Off 时触发，天然满足）。
* B2 提供 `--on-skip-error-if <reason-list>` 逃生阀：只对本次已修复的拒因判红，避免快照类 `UnprovenTargetConstraints` 在无 `--ddl` 的临时库上抖动误红。

---

## 6. 判定工件与 `RunPlan` 的边界

**为什么不写进计划文件**：

1. `SCHEMA_VERSION = 1` 是严格门（`plan/mod.rs:24`、`:129-133`，不符即 `ENV-0030` 拒读）——加段即升版，**现存全部计划作废**；
2. 判定是**数据的函数**（`swh_all_kind.kind_id` 一行就能翻转）。写进计划 = 制造「过期判定」新问题：计划命中后直接沿用旧结论，反而比现在更危险；
3. 计划已被 `deploy`/`call` 当闭包重放契约消费（`ddl_routines`/`ddl_tables`/`missing`/`refs`），混入判定会让「计划变更」与「数据变更」不可分。

**Phase 1 只出显式工件**（`--admit-record`）：

```toml
# spinduck-admit/1
[provenance]
binary = "spinduck 0.2.4+867a731"
kernel = "1.5.5"
rules_version = "…"
mode = "batch-atomic"
db = "aaspubs.duckdb"

[[routine]]
name = "pkg_split_trade_step3_sh.prc_trd_deal_purchase"
verdict = "skipped"
confidence = "firm"                                # firm | conservative；conservative 不判红（F2）
reason = "unsupported-control-flow"
note = "swh_all_kind.kind_id='1' 命中 1 行 → THEN 分支可达"
fingerprint = "swh_all_kind:rows=1;digest=…"        # 只读聚合摘要，Phase 3 才用于 staleness

[[routine.guard]]
kind = "route"; scope = "static"; state = "failed"; object = "swh_all_kind"
sql = "SELECT ((CASE WHEN … END) = '1') IS NOT TRUE"
expect = "THEN 分支不可达"

[[routine.guard]]
kind = "top1-tie"; scope = "frame"; state = "unchecked"; object = "par_fund_seat_para"
binds = ["seat_code", "inure_begin_date", "inure_end_date"]

# state 取值域：passed | failed | unchecked；unchecked 必须带 reason ∈
# {frame-bound, prelude-dependent, macro-dependent, no-db, probe-unavailable}
```

**Phase 3（可选，需批准）**：`fingerprint` 参与 staleness——`call --plan` 命中数据指纹不符时报 `stale` 并要求重跑 `compile --admit`，此时才把段并入计划并升 `schema_version`。收益只是省一条只读 `SELECT` 与一个静默窗口，代价是整条失效检测，**默认不做**。**前置**：fingerprint 的 staleness 语义依赖 F1/F2 修完（守卫判定先是 firm 的，指纹比对才有意义）；在 confidence 分层落地前不得启动本 Phase。

---

## 7. 副作用与安全边界

| 面 | 事实 | 结论 |
| :--- | :--- | :--- |
| compile 打开内核 | 已有先例：Gate 2B `DuckExecutor::open_in_memory()`（`gate/prepare.rs:323`） | `--admit --db` 不是「让 compile 变成执行器」，它仍是**只读取证** |
| 宏注册 | `Engine::register` 下发 `CREATE OR REPLACE MACRO …`（`catalog/sql_macro.rs:32`），是 catalog 持久对象 | **有条件成立**：admit 不经 `Engine::register` → 宏 DDL 与 `spinduck_routines` 写（`engine.rs:297`）不下发，`--admit --db` 对目标库零写入。**前提 = F3 宏闸在位**（§4.2）：守卫 SQL 命中 `macro_names` 即 `Unchecked(macro-dependent)`，否则 Catalog Error 上抛。取证面与 `call` 的分歧由该闸显式承载，不假装等价 |
| 业务写入 | `admit` 不发区域 SQL、不调 `call_with_sink`、不执行存过体、`register_persistent` 不在路径上（`engine.rs:297` 的 `spinduck_routines` 写只在持久化注册时发生） | 零业务写入 |
| preludes | `sd_vw*`/`sd_fn*` 物化会建临时表并回调例程 | admit **不执行**（§4.2） |
| 只读打开 | 内核仅 `Connection::open(path)`（`exec_native.rs:42-48`），无只读入口 | 三选项见下 |

只读边界三选项：**A** 文档要求 + 复现包默认对生产库先 `cp`（推荐，Phase 1 零引擎改动）；**B** 新增只读打开能力（`sql/exec.rs` 接缝扩展，Ask first）；**C** 改 TEMP 宏（波及 `call`/`deploy`/EngineCache 口径，不建议顺带做）。

---

## 8. 测试矩阵（Red → Green，一次锁一个行为）

| # | 层 | 测试 | 关键断言 |
| :--- | :--- | :--- | :--- |
| 1 | 守护 | `should_keep_entry_ok_verdict_identical_after_guard_slot_split` | 拆分前后同一 plan 的 `Result<Result<(),_>,EngineError>`、首个失败 `SkipReason` 与 FakeExecutor 记录的 SQL 序列**逐字相同**（PR-1 的唯一放行条件；含 top1+route 同败的次序用例，锁交错次序不重排） |
| 2 | 单元 | `should_tag_route_guard_as_static_and_top1_as_frame` | `route → scope=Static,binds 空`；`top1/expr → scope=Frame`；槽位次序 = `entry_ok` 原始次序 |
| 2b | 单元 | `should_admit_via_injected_snapshot_without_codeweb_project` | `compile_project_admit_with_source` + 字面量 `AdmitInput` 在无真实 codeweb 项目下产出判定（§3.4 接缝的存在理由） |
| 3 | 单元 | `should_leave_all_l3_guards_unchecked_without_database` | 无数据面时 `state` 只能是 `Unchecked`，**绝不出现 `Passed`**（§2.1 防假绿根） |
| 4 | 集成（FakeExecutor） | `should_not_execute_routine_body_during_admission` | 收到的 SQL 清单无 `INSERT`/`CALL`/`UPDATE`/宏 DDL，只有守卫 `SELECT` 与目录探测 |
| 4b | 单元 | `should_report_first_static_failure_reason_when_frame_guard_precedes` | route 失败且 top1 槽位在前 → verdict=`skipped(firm)`、reason=route 拒因、top1=`Unchecked`（§3.2 reason 口径的锁） |
| 4c | 单元（F2） | `should_not_report_firm_verdict_for_probe_dependent_rejection` | 同一语料两次 admit：`executor=None` 时探测依赖拒因（如 `UnknownObjectDependency`）只产 `conservative`/`StaticOnly`，**绝不出 firm `skipped`**；`executor=Some` 重证后才允许 firm |
| 4d | 单元（F1） | `should_mark_prelude_dependent_guards_unchecked` | `preludes_first` 计划的守卫 SQL 含 `sd_fn*`/`sd_vw*` → `Unchecked(prelude-dependent)`，不求值（FakeExecutor 无对应 SQL），verdict 上限 `conditional` |
| 4e | 单元（F3） | `should_mark_macro_dependent_guards_unchecked` | 守卫 SQL 命中 `macro_names` → `Unchecked(macro-dependent)`；PR-3 前置验证项 |
| 5 | 集成（native，临时库） | `should_report_skipped_route_guard_when_kind_switch_is_one` | 插 `swh_all_kind='1'` → `skipped(UnsupportedControlFlow)`；改 `'0'`/删至 0 行/两行 → `conditional`（四象限基线取 §12.4 G5 重测，非仓内既有记录） |
| 6 | CLI 契约 | `compile_admit_should_render_verdict_line_identical_to_call` | `ADMIT` 行的 `<routine> … reason=` 片段与同库同例程 `call` 的 `SET_ORIENTED` 行逐字相等；fixture 须避开"帧守卫排在失败静态守卫之前"的歧义场景（§3.2 reason 口径） |
| 7 | CLI 契约 | `compile_without_admit_should_render_unchanged` | 现有 compile 语料 stdout/退出码零变化（回归锁） |
| 8 | CLI 契约 | `compile_admit_error_should_exit_fail_on_firm_skipped_only` | `report`=0/`error`=1；`conditional` 与 `conservative` 在 `error` 下仍 0（conservative 不判红是 F2 的 CLI 面） |
| 9 | CLI 契约 | `compile_admit_should_reject_db_without_admit_and_report_no_native` | `--db`/`--admit-record` 无 `--admit` 用法错、非 native 打 `admit=skipped(no-native)`；若 D9 采纳强制，另加 `--admit` 无 `--db` 用法错断言 |
| 10 | CLI 契约 | `call_on_skip_error_should_exit_nonzero_and_print_before_body` | 判定行出现在 `OUT` 之前；`off` 模式 stdout 零新增 |
| 11 | 语料 | `cases_native` purchase switch ON/OFF 两态 | 复用 §12.7 数据前置（V52 fixture **尚未入仓**，需先建）；**只加断言集，不改任何既有期望值**。D7 合入后 ON 态断言由 `skipped(UnsupportedControlFlow)` 改为「选 `then` 臂 + 批写等价」，属**期望集扩**而非期望值变更（§12.5 门 3 数据前置见 §12.7） |
| 12 | 复现包 | v3 包 `verify_chain.sh` 新 S 步 | switch ON 场景在**执行存过之前**出判定行；D7 落地前断言 `skipped reason=unsupported-control-flow`，落地后改断 `… converted arm=then`（同一 S 步两处期望，按 PR 次序启用） |

> D7（§12）的测试 13–17 续排在同一编号空间，表见 §12.6；门禁口径同本节末段。`RouteArm`/`Discriminator`
> 落在 `src/ir/set_oriented/` 内 → 自动进变异范围（AGENTS §4.4），PR-5 合入后须补跑一轮变异（M6.5 节点）。

门禁（每个 PR）：`cargo fmt --all -- --check`、`cargo clippy --all-features -- -D warnings`、`cargo test --features cli`、`cargo audit`、Golden job、`cases_native`、`verify_chain`。`src/ir/set_oriented/` 已在变异范围（AGENTS §4.4），新增 `admit.rs` 随目录自动纳入。

---

## 9. 落地顺序与 PR 切分

| PR | 内容 | 依赖 | 回滚点 |
| :--- | :--- | :--- | :--- |
| **PR-1** `refactor(set-oriented): entry_ok 按 GuardSlot 原始次序重构 + GuardOutcome` | §4.1（槽位制、非两阶段），行为零变更，靠测试 1 放行 | — | 纯重构，revert 无风险 |
| **PR-2** `feat(compile): --admit 静态判定 + unchecked 清单 + AdmitInput 接缝` | §3.1/§3.2/§3.4、§4.2 `admit_verify`（executor=None 分支，**含 F2 confidence 分层**）、测试 2/2b/3/4/4c/7/9 | PR-1 | 不加旗标即完全不触发 |
| **PR-3** `feat(compile): --admit --db 数据守卫判定 + --admit=error + --admit-record` | §3.3、§6、`admit_verify`（executor=Some 分支，**含 F1 prelude 闸 + F3 宏闸**）、测试 4b/4d/4e/5/6/8/11 | PR-2 | 旗标默认关 |
| **PR-4** `feat(call): 判定行前移 + --on-skip[=error]` + v3 包 S 步 + README/REPORT 口径 | §5 B2/B3、测试 10/12、`docs/validation.md` **V52 首次落档**（compile 侧证据 + 会话内手工取证补齐） | PR-1 | 独立，可与 PR-3 并行评审 |
| **PR-5** `feat(set-oriented): IF 路由双臂判别式选臂（D7）` | §12.3(b)(c)(d)(e)、§12.4 G0 探针取证、§12.5 三道门、测试 13–17 | PR-1（GuardSlot 复用）；§12.3(e) 的 admit 面依赖 PR-3 | 旗标之外唯一语义变更包：`discriminator = None` 即回到今天全部行为，revert 只回本 PR |

> **PR-1 的边界**：只做 `entry_ok` 槽位化，**不动 `ConversionPlan` 形状**——双臂要加的
> `discriminator`/`RouteArm` 字段属 PR-5，提前塞进 PR-1 会让「行为零变更」这条放行判据
> 失去意义（测试 1 与测试 13 将同时依赖两次变更）。

> PR-5 不阻塞 PR-1…PR-4，也不被它们阻塞（除 PR-1 的接缝与 PR-3 的报告字段）。若 §12.7 的
> 请示项任一被否，PR-5 整体退回 v3.2，§1–§11 照常交付——这就是「两半独立回滚点」的含义。

> 发版序（AGENTS §9）：每个 PR 走 bump→PR→五门禁绿→merge→在 merge commit 打 tag；PR-4 若与 v0.2.5 合并发布，判定行前移属 CLI 可见变化，须在 README「已知限制」与 validation 同 PR 记录。

> 文档归属（AGENTS §2「三者不得互相越界」）：本文是**待实现设计**，随 PR 演进；机制真正落地后把结论并入 `docs/architecture.md`，实测证据追加到 `docs/validation.md`（V52 = 本版预留小节，现止于 V51），规则值变动才进 `config/rewrite-rules.toml`——本版不动规则值。

---

## 10. 决策项状态（2026-11-23 定稿）

> D1–D6、D8、D9 **按推荐口径定稿**；D7 由人类决定**纳入本版**，从「决策项」转为「设计项 §12」，
> 其落地前置另引出 D10/D11 两项待请示。

| # | 决策 | 结论 |
| :--- | :--- | :--- |
| D1 | `conditional` 是否也判红 | **定稿：不判红**（含 Top-1/表达式守卫的语料会大面积误红） |
| D2 | 是否要 `call --on-skip=error`（B2） | **定稿：要**，它直接消除本次静默；判定行前移是唯一可见变化 |
| D3 | 工件是否入库 git | **定稿：默认不写项目树**；验证包内 `reports/` 按现有惯例入库 |
| D4 | 只读边界 A/B/C | **定稿：A**（对生产库 `cp` 后再 `--admit --db`），B 留待需求；本版不关闭 |
| D5 | stdout 末行「结论」话术是否保留 | **定稿：保留**，仅 `skipped` + 数据型守卫失败时打，不进契约断言 |
| D6 | `SkipReason` snake_case 作为长期契约 | **定稿：是**，自 PR-3 起写入工件 |
| D7 | purchase 的 `kind_id='1'` **形态支持**是否纳入本版 | **定稿：纳入本版**（人类指示）。设计 = §12，落地 = PR-5；前置请示项 = §12.7 |
| D8 | B2 判定行前移的输出通道（§5） | **定稿：Engine 报告回调字段**（`on_set_oriented_report: Option<Box<dyn FnMut(&Report)>>`，`push_report` 内同步调用；不改 `TraceSink`/`SqlExecutor` 接缝签名，无需 Ask first）。备选 `TraceSink` 新事件 = 接缝契约变更，若选它须先 Ask first 并同步 §11.2 JSONL 契约 |
| D9 | `--admit` 是否强制要求 `--db` | **定稿：强制**。`--admit` 不带 `--db` → 用法错（ENG-0010 口径）。F2 修复后无数据面只剩形状类 firm 判定，CLI 层直接堵死误用面；库级 `admit_verify` 保留 `executor=None` 路径供测试与未来只读面，输出恒为 `StaticOnly`/`conservative` 并显式打 `no-db` 标记。因此测试 3/4c 在**库级**断言（CLI 契约层测不到 no-db 面） |
| **D10** | `SkipReason::RouteDiscriminatorNotTotal` 是否算 `[rejects]` 增项（§12.3(d)） | **建议：不算**——它是回退而非拒绝，兼容范围未收缩；但须同步 `config/rewrite-rules.toml` 注释并挂探针（AGENTS §6：`assumed → verified`）。仍属 Ask first，需业务确认 |
| **D11** | 双臂报告形态：`Status::Converted { sql, arm }` 还是 `Report.note`（§12.3(e)） | **建议：加 `arm` 枚举字段**（可进门禁断言）。代价 = `Status`/`Report` 形状变更、触及人类既有测试断言（测试权限表里是只读项）→ Ask first。`note` 字符串只在 D11 被否时作过渡，且明确「不进契约断言」 |

---

## 11. 验收标准

### 11.1 §1–§11（判定前移）

1. 复现命令不再需要 33.9s：`compile --dir golden/case_corpus_purchase --entry …prc_trd_deal_purchase --admit --db aaspubs.duckdb` 在**未执行存过体**的前置条件下输出 `skipped reason=UnsupportedControlFlow` + 守卫清单（D7 合入后本条的**预期输出改变**，见 §12.3(e)：改为选臂结果或该臂具体拒因；本条按合入次序分别记录基线）；
2. 同一库同一例程：`compile --admit` 与 `call` 的判定行片段逐字相同（测试 6 锁死，杜绝双口径）；
3. 无数据面时 L3 输出恒为 `unchecked`、探测依赖 L1 拒因恒为 `conservative`，绝不出现假绿/假红（测试 3、4c，库级）；
4. `call --on-skip=error` 在 switch ON 数据下退出码非零、判定行先于 `OUT`；`off` 模式 stdout 零新增；
5. 全部门禁绿 + purchase/byfund 等价 golden 零回归 + 四象限复跑通过（基线取 §12.4 G5 重测）；
6. 明确不做：不新增子命令、**不改 ELSE 侧准入规则**、不把判定写进 `RunPlan`、不在 admit 内物化 preludes、不执行任何存过体。（「不改准入规则」的例外只有一处，即 §12 的双臂选臂，且它只扩大可转面。）

### 11.2 §12（D7 双臂）—— 三道放行门逐条可验

7. **门 1 零回归**：byfund 与 purchase（switch='0'）的生成 SQL / `SET_ORIENTED` 行 / 退出码与 v0.2.4 基线逐字一致，基线 sha256 入 `docs/validation.md`；
8. **门 2 状态合并**：测试 14 的守护期望值在后续任何重构中不得变更（AGENTS §4.3 守护测试纪律）；
9. **门 3 等价**：OFF/ON × 单行/多行/无命中/NULL 四象限，选中臂批写与逐行解释一致；§12.7 数据前置未到位时，本条**只能**记「自证级」证据并显式标注，不得记为业务正确性已证；
10. **G5 重测 + G0 站点证据入 validation V52（新小节），探针本身删除**：本文所有「已实证」在 V52 落档前只是会话证据；`ExceptionScopePresent` 的实际站点须以臂分叉下的实测替换，临时探针不得留在仓库（AGENTS Never 条目）。

---

## 12. D7：IF 路由双臂集合化（`COMMISSION_SWITCH='1'` 本版纳入）

> 决定：**纳入本版**。§1–§11 把判定前移到编译期，但 purchase 的 ON 侧仍是 `skipped`——
> 判红只是把失败说得清楚，没有解决它。D7 解决的是「看得见解论之后，让这条分支也真能转」。
> 独立 PR（PR-5），依赖 PR-1；PR-1…PR-4 不被它阻塞。

### 12.1 今天的机理：三段，缺一不可

1. **分析期只走 ELSE 臂**。`JumpIfNot(cond, to)` 且 `cond` 命中「循环不变量 lookup 变量 = 常量」
   形状时（`route::cond`，`route.rs:8-24`：要求 `correlations.is_empty() && top1.is_none()`），
   `analyze/body.rs:245-254` 记下 `self.route` 后**返回 `Ok(to)`**——走查点直接落到 ELSE 分支
   首指令。**THEN 块从来不进分析**：它的语句不折叠、不进 copy、不产生守卫，也不参与 tail proof。
2. **生成期把「THEN 不可达」做成入口守卫**。`generate.rs:400-408` 用 lookup 的渲染证据
   （`hit`/`value`/`default_sql`，`lookup/mod.rs:252-259`）产
   `SELECT ((CASE WHEN hit THEN value ELSE default END) = '1') IS NOT TRUE` →
   `ConversionPlan.route_guards`（`generate.rs:386`）→ `entry.rs:147-156` 求值，
   非 TRUE（= THEN 可达）即 `Err(SkipReason::UnsupportedControlFlow)`。
3. **运行期无损退回**。`engine/set_oriented.rs:163-177` 收 `Err(reason)` → `push_report(Skipped)`
   → 原区域 `[open_pc, close_pc+1)` 逐行解释执行。本次手工复现的 33.9s 就是这么来的。

常量折叠那条捷径（`analyze/body.rs:221-228` + `route::static_cond`，`route.rs:42-47`）只能证「编译期
已知 `v_kind` 恒非 '1'」；`v_kind` 来自运行期 lookup，折叠不出来 → 只剩入口守卫。

**所以 D7 的定义很窄**：把第 1 步改成「两臂都走查、各出一份计划」，把第 2 步改成「守卫取判别式
的实际值 → 选臂」。不是新证明技术，是补上一条从未被分析过的分支。

### 12.2 purchase 的 THEN 臂里到底有什么（含对「分支体互换探针」的准确解读）

`golden/case_corpus_purchase/sql/routines/PKG_SPLIT_TRADE_STEP3_SH.sql:926-1011`：

| 行 | 构件 | 双臂化后的归属 |
| :--- | :--- | :--- |
| 926-935 | `BEGIN SELECT kind_id INTO v_kind FROM swh_all_kind WHERE operation_kind='COMMISSION_SWITCH' EXCEPTION WHEN OTHERS → v_kind := '0' END` | **判别式源**：非相关、非 Top-1 的不变量 lookup，今天已被 `route::cond` 认可 |
| 938 | `IF v_kind = '1' THEN` | 路由点 |
| 939-946 | `v_commision_fee_type := fnc_com_get_seat_para(8 参，含 3 个 OUT)` | THEN 臂：多 OUT 调用 #59 已落（`LookupTuple`），但这里是**返回值赋标量 + OUT 同时写回**的复合形状，`ScalarAssignFromTuple` 口径待实测（§12.4 G1） |
| 948-975 | `SELECT CASE v_commision_fee_type WHEN '11'…'36' ELSE … END INTO v_yj FROM sys_dummy`（12 键，取 `r_get_purchase.purchase_yjNN` 12 列） | THEN 臂：`sys_dummy` 门已有（`entry.rs:119`）；CASE 键是**臂内刚赋值的标量** → 依赖 §12.3(b) 第 4 步的状态合并；12 个游标列直接展开进批写 SELECT 列表 |
| 978-993 | ELSE：`SELECT a.commision_type INTO v_commision_fee_type FROM (… ORDER BY seat_code DESC, stock_kind DESC) a WHERE rownum = 1` | 今天已证的 Top-1 lookup（`tie_guard`/`miss_guard` 那套） |
| 995-1006 | ELSE：`SELECT CASE v_commision_fee_type WHEN '1'/'4'/'6' ELSE … END INTO v_yj FROM sys_dummy` | 今天已证的折叠 CASE |
| 1008-1010 | `EXCEPTION WHEN OTHERS THEN v_yj := '0'` | 两臂**共用**同一个处理器作用域。该处理器所需的 `miss_outs`/`residual_rows`/`no_data_found_uniform` 机制在代码里存在、且 ELSE 侧计划能**生成成功**（真机能报到运行期 route guard = 分析期已通过），但「批写路径在同数据上实际跑通」尚未取得仓内证据 → 归 §12.4 G5/G3 一并实测 |

**「分支体互换」探针怎么读才对**（三次修订：本段上一稿写反了，以下为重新推演的口径）：
互换 fixture 只搬两段分支体的**位置**，IF 条件与它依赖的判别式 lookup 都留在原处 →
`route::cond` 依旧成立 → `branch()` 依旧 `Ok(to)`，而 `to` 处现在放的正是**原 THEN 内容**
（多 OUT 调用 + 12 键 CASE）。所以那次走查**确实分析到了 THEN 内容**，
它报 `ExceptionScopePresent` 是关于该内容的直接证据，不是关于路由识别的旁证。

两点限定让它不能当成「不可解」的定论：

* **站点未分辨**：`ExceptionScopePresent` 有 8 个产出点（`region/mod.rs:142`、
  `proof.rs:72/101/107/116`、`traps.rs:39/106/499`）。可用的一条反推是：当前构建在真机上
  报到运行期 `UnsupportedControlFlow`，说明分析期的区域边界检查（`:142`）已经通过 →
  探针那次的大概率是**处理器/证明类站点**（proof.rs / traps.rs），而非边界站点；
  这是推论不是实测，PR-5 仍须打出实际站点（§12.4 G0）。
* **fixture 口径**：互换件是临时副本，不是仓库资产；它把 THEN 内容放在**没有 ELSE 臂折叠事实**
  的上下文里分析，与 §12.3(b) 的臂分叉（两臂各自独立走查 + 出口合并）周边条件不同，
  结论不可直接搬运。

### 12.3 设计

**(a) IR 形状：主计划 + 备选臂**（不搞「一次转换出两个独立区域」）

```rust
pub struct Discriminator {
    pub sql: String,        // route::value()：SELECT CASE WHEN hit THEN value ELSE default END —— 取值，不判不可达
    pub arms: Vec<RouteArm>,// 值域划分；空 = 今天形态（route_guards 必须不可达）
}
pub struct RouteArm {
    pub value: String,           // 判别式取到该值时选本臂，例如 "1"
    pub plan: Box<ConversionPlan>,// 该臂自己的 sql/binds/guards/miss_outs/preludes/tail_commit
}
pub struct ConversionPlan {
    …现有字段不变…,               // 主计划 = ELSE 臂（今天的一切输出逐字保持）
    pub discriminator: Option<Discriminator>,
}
```

* `route_guards` 与 `discriminator` **互斥且并存于类型**：`discriminator = None` 时走今天语义
  （守卫证明 THEN 不可达）；非 None 时取值选臂，不再要求不可达。
* ELSE 臂占用现有字段、不动形状 → ELSE-only 语料的生成 SQL 必然逐字不变（§12.5 门 1 的依据）。
* `route::value()` 复用 `guard()` 的同一份渲染证据与同一道 `key_checks.is_empty()` 前提
  （`route.rs:49-52`）——两臂共享一次 lookup 证明，不允许出现「守卫能用证据、判别式另找一套」。

**(b) 分析期：臂分叉走查 + 出口状态合并**（PR-5 最难的一段）

`analyze/body.rs` 命中 route 形状时不再 `Ok(to)`，改为：

1. 快照当前 `Body` 状态（`lookups`/`folded`/`inlined`/`dead`/`arith`/`aggs`/`route`）；
2. 从快照走查 THEN 块 `[pc+1, to)`，到该 IF 的出口跳转收束，**独立**产出 then 臂的
   copy/tail/cursor_deps/has_trap 与独立 `TailProof`；
3. 从同一快照走查 ELSE 块（今天的行为，产出主计划）；
4. **合并两臂出口状态后才继续走 END IF 之后的公共后缀**：写入集取并集；只在两臂都成立且同值的
   folded/inlined 事实上保留，其余降级为「未知」重走 lookup 路径。

第 4 步是这条路上唯一会「静默出错」的地方：后缀若沿用单臂事实，就把「只有 THEN 跑过」当成恒真，
生成的批写 SQL 会带着错常量继续跑而不报错。必须有守护测试锁它（§12.5 门 2）。

**(c) 生成期**：两臂各调一次 `generate()`，但 `sd_fn*`/`sd_bf*`/`sd_vwN` 表名改由**区域级**计数器
一次分配——同函数同绑定两臂复用同一张缓存表，不同绑定各建一张。各臂独立编号会撞名或漏建，
表现为运行期「守卫打到不存在的表」（正是 F1 在 admit 侧要防的那类事，只是这里发生在 call 侧）。

**(d) 运行期**（`engine/set_oriented.rs::adjudicate_region`）

1. 前导物化次序**不变**（`preludes_first` → fn 前导 → 视图物化，`:129-161`）；
2. 新增：判别式求值（bind-free、恰一行、非 NULL）→ 选臂；
3. 选中臂跑自己那份守卫链（sys_dummy → drift → snapshot → top1 → ~~route~~ → expr，次序不变）；
4. 新增保守回退条件：判别式 0 行 / >1 行 / NULL / 值落在 arms 值域外 →
   `SkipReason::RouteDiscriminatorNotTotal`，整段按原 IR 解释执行；
   选中臂守卫失败 → 该臂的原始拒因，同样整段回退（**绝不半批写**）；
5. 批写、`miss_outs`、`tail_commit`、`RunSpan` 边界全部按选中臂的计划走。

错误分层不变：外层 `EngineError` 一律上抛交 `call_with_sink` 回滚，只有内层逻辑拒绝才回退
（v3 §2.1 不变量 3，§4.1 硬约束）。

**(e) 可观测面：必须报「选了哪一臂」**

这是 v3.1 立项理由的延伸——双臂之后「转换了」这句话如果不带臂信息，又重新变得不可审计：

```text
ADMIT pkg_split_trade_step3_sh.prc_trd_deal_purchase converted arm=then guards=3/3
ADMIT   arm=then  discriminator="1"  verdict=converted
ADMIT   arm=else  verdict=conditional unchecked=top1(frame),expr(frame)
SET_ORIENTED pkg_split_trade_step3_sh.prc_trd_deal_purchase converted arm=then sql="…"
```

* 未选中臂的守卫**不求值**、状态恒 `Unchecked(control-flow-excluded)`（新增取值域项），且
  不计入 firm `skipped`——它是确定的控制流事实，不是证据缺口，与 §2.1 总原则一致。
* `call` 与 `admit` 的臂行仍走同一个 `verdict_line`（§3.2 同源铁律）。
* `Status::Converted { sql }` 加 `arm` 字段属 `Status`/`Report` 形状变更、会动到人类既有断言
  → Ask first（D11）；过渡方案是 `Report` 增 `note`，但字符串不能进门禁断言。

### 12.4 待证门禁清单（PR-5 的取证任务，不是实现任务）

| # | 待证 | 现状 | 过关判据 |
| :--- | :--- | :--- | :--- |
| **G0** | THEN 臂被哪个 `ExceptionScopePresent` 站点拒 | 互换探针已证「该内容报此因」，但 8 个产出点未分辨（§12.2） | 臂分叉实现后打出**实际站点 + 拒因**；若确为边界站点 `region/mod.rs:142` 则与臂无关，另案升级 |
| **G5** | 当前 main 上 purchase 两个象限各自报什么 | 仓内记录是 v0.2.1 + `kind_id='0'` 的 `ExceptionScopePresent`（`REPORT.md:188`）；会话内手工记录是 v0.2.4 + 真机 `kind_id='1'` 的 `UnsupportedControlFlow`；**两条都不可作本文基线** | PR-5 第一步之前先重测（含 `--admit` 输出），把结果写进 validation V52；门 1 的「逐字一致」基线由这次重测产出 |
| **G1** | `v := fn(8 参含 3 OUT)` 的标量赋值形状 | #59 `LookupTuple` 覆盖多 OUT 调用；`ScalarAssignFromTuple` 口径未测 | 单元层用 FakeExecutor 断言产出的 binds 表与批写列一一对应 |
| **G2** | 12 键 CASE + `sys_dummy` 折叠 | ELSE 臂 4 键版已通 | 生成 SQL 与逐行结果等价（内核态 fixture） |
| **G3** | 两臂共用处理器时的 miss 均匀性 | `no_data_found_uniform`/`miss_outs` 只在单臂下证过 | 选臂后处理器赋值集合仍逐行均匀；否则该臂 `Skipped`，不得放行 |
| **G4** | 区域级缓存表命名（§12.3(c)） | 现按 plan 编号 | 两臂同函数同绑定复用、异绑定不撞名 |

### 12.5 放行门（三道，缺一不得合入）

1. **零回归门**：byfund + purchase（switch='0'）两语料的生成 SQL、`SET_ORIENTED` 行、退出码
   与基线**逐字一致**；基线由 §12.4 G5 的当前 main 重测产出（**不是** v0.2.1 的 `REPORT.md:188`），
   sha256 与重测记录一并入 `docs/validation.md`。
2. **状态合并门**：守护测试构造「两臂对同一变量写不同常量、后缀读它」的 fixture，断言后缀
   **不**沿用任单臂事实（§12.3(b) 第 4 步）。此测试的期望值在后续重构中不得变更。
3. **四象限等价门**：OFF/ON × 单行/多行/无命中/NULL，选中臂批写 vs 逐行解释的
   `dat_trd_repurchase` 全列 + `p_o_succeed` 一致。ON 象限需要 §12.7 的数据前置；
   前置未到位时本门只跑 SpinDuck 内部一致（证据等级记「自证」），**不得**写成业务正确性已证。

### 12.6 测试增量（并入 §8 的编号续排）

| # | 层 | 测试 | 关键断言 |
| :--- | :--- | :--- | :--- |
| 13 | 守护 | `should_keep_else_only_plan_verbatim_with_arms_field` | 门 1 的代码侧锁：`discriminator = None` 时生成 SQL 与守卫序列逐字不变 |
| 14 | 单元 | `should_analyze_both_route_arms_and_merge_disagreeing_facts` | 臂分叉 + 出口合并：两臂异值变量在后缀不得作为 folded 常量出现（门 2） |
| 15 | 单元 | `should_fall_back_when_route_discriminator_not_single_valued` | 判别式 0 行 / 2 行 / NULL → `Skipped(RouteDiscriminatorNotTotal)` 且原区域完整执行（无半批写） |
| 16 | 集成（native） | `should_convert_purchase_when_commission_switch_is_one` | ON 数据下选 `then` 臂、批写与逐行结果一致（门 3 的可执行面；数据前置见 §12.7） |
| 17 | CLI 契约 | `admit_and_call_should_report_same_selected_arm` | `ADMIT … arm=then` 与 `SET_ORIENTED … arm=then` 同函数产出（§3.2 同源铁律在双臂下的延伸） |

### 12.7 落地顺序与 Ask-first 清单

PR-5 内部顺序：G0 探针取证 → 13/14（分析期分叉）→ 判别式 + 选臂（运行期）→ G1…G4 → 15/16/17 →
删探针 → 门 1/2/3 → 取证写入 `docs/validation.md` **V52（本小节即首次落档**，含 G5 重测、
G0 站点、门 1 基线 sha256；§0 预留的编号在此兑现）。

以下每一项都属 AGENTS 的 Ask first，**默认不做，逐项请示**：

1. **准入语义变更**：路由守卫从「证明 THEN 不可达」改为「按取值选臂」，并新增
   `SkipReason::RouteDiscriminatorNotTotal`（20 变体 → 21）与 `Unchecked` 取值域新项
   `control-flow-excluded`。`[rejects]` 清单**不增项**（选臂不是拒绝），但须同步规则注释与探针。
2. **`Status`/`Report` 形状加 `arm` 字段**（D11）——会动到人类既有断言，测试权限表里是只读项。
3. **golden fixture**：现语料 `swh_all_kind` 只有 `('COMMISSION_SWITCH','0',…)`
   （`cases/datasets/seed.sql:24-25`），且**没有任何 `par_fund_seat_para` 数据行**（seed.sql 的
   INSERT 清单里没有该表）→ ON 臂在现语料上恒走 NO_DATA_FOUND 兜底，测不到真实分支。
   需要**新增独立数据集**（不改现有行，否则 ELSE 侧基线一起漂），属人类 fixture 变更。
4. **列宽缺陷**：`par_fund_seat_para.commision_type/commision_fee_type` 是 `varchar(1)`
   （`sql/ddl/tables.sql:153-154`），而 step4 CASE 的费型码是 2 字符（'11'/'12'…）。仓库自己在
   `cases/datasets/seed.sql:2-5` 记了这笔账并写明「CASE 路径须先修基线列宽后另行覆盖」。
   DuckDB 不截断、GaussDB 截断/报错 → 不修列宽就是**语义分叉**，修列宽 = 改人类 DDL 基线。
   这是 D7 真正的硬前置，不是可以绕过的测试细节。
5. **ON 侧参照基线目前两条路都不通**（这也是 D4 不关闭的原因）：CI 的 arm64 hosted runner
   起不动官方镜像（validation V21.12，`gosu: Exec format error`），amd64 参照实例在**过程调用**
   上打崩 postmaster，且同 tag 不同 build（memory: openGauss reference instance）。因此门 3 在
   本版只能出「SpinDuck 自证」级证据，参照侧比对属未决外部依赖，须在验收里显式标注证据等级。

---

## 附录 A：本设计引用的代码坐标（main `867a731` 实测）

| 坐标 | 事实 |
| :--- | :--- |
| `cli/run/call.rs:287` → `:464-481` | 判定渲染发生在整次调用返回之后（先 `call_with_sink`，后 `render_set_oriented_reports`，再打 `OUT`） |
| `cli/run/call.rs:305-328` | `compile_offline`：`NoopExecutor` 注册 → `compile_set_oriented_plans`（候选闭包） |
| `cli/run/call.rs:396/399` | 项目 `set_oriented` 发现、`discover_rules_files` 规则链（与 compile `rules_for_paths` 同口径） |
| `cli/run/compile_project.rs:19-32 / :112-150 / :145-149` | `ProjectCompile` 字段、项目态 compile 主流程、退出码规则 |
| `cli/run/compile_project.rs:284-303 / :321-328 / :455-515` | NoopExecutor 闭包目录（`Engine.catalog` 可取）、Gate 2B prepare 调用、`gate::schema::SchemaSnapshot` 产出（**非** L1 所需类型） |
| `cli/args/compile.rs:21-99` | compile 旗标扫描：`project_mode` 互斥、重复旗标拒、未知旗标拒 |
| `gate/prepare.rs:313-333` | `DuckExecutor::open_in_memory()` + stub DDL（compile 期打开内核的既有先例） |
| `ir/set_oriented/candidate.rs:66-80` | L1 离线入口签名与 `Source::Static(snapshot)` |
| `ir/set_oriented/entry.rs:54-87 / :114-169` | `ConversionPlan` 守卫/前导字段、六类守卫链（top1 帧 → route 静态 → expr 帧交错）与拒因映射 |
| `ir/set_oriented/route.rs:49-60` | 路由守卫 SQL（三值否定，无数据时恒真 → §2.1 假绿） |
| `ir/set_oriented/mod.rs:78-112 / :115-125` | 20 个 `SkipReason` 变体、`Status`/`Report` 形状 |
| `ir/set_oriented/metadata.rs:9/52`、`plan/codeweb/source.rs:78-89` | L1 所需 `SchemaSnapshot` 类型与 `schema_snapshot(codeweb_root, rules)` 生产者 |
| `engine/set_oriented.rs:162-165 / :288-304` | `entry_ok` 失败点（B2 行前移锚点）、离线保守 Err 的运行期重证口径 |
| `plan/mod.rs:24 / :129-133` | `SCHEMA_VERSION=1` 与版本门 `ENV-0030` |
| `catalog/sql_macro.rs:32`、`engine.rs:191-205/297`、`sql/exec_native.rs:42-48` | 宏 DDL（catalog 持久）、`install_sql_macro` 下发点与 `macro_names` 登记（`engine.rs:32` 私有字段，需只读访问器）、`spinduck_routines` 写、无只读打开 |
| `ir/set_oriented/schema.rs:52-87`、`resolve.rs:70/77/91/103` | `Source::Static` 下 `view_sql`/`object_names`/`probe_columns` → `None`（**`sys_dummy_ok` → `Ok(true)` 是延迟而非缺口，`schema.rs:61-65`**）；同一 `UnknownObjectDependency` 两义：探测缺口 vs 真实列缺失（F2 分档依据） |
| `ir/set_oriented/analyze/body.rs:245-254` | route 形状命中后 `Ok(to)`：走查点直接落 ELSE 分支首指令，**THEN 块从不进分析**（§12.1 第 1 步） |
| `ir/set_oriented/analyze/body.rs:221-228`、`route.rs:42-47` | 常量剪枝捷径，只对可折叠的 `Var = Const` 成立；运行期 lookup 变量折叠不出来 |
| `ir/set_oriented/route.rs:8-24/49-60` | `cond`（要求非相关 + 非 Top-1）与 `guard`（`key_checks` 非空即 `UnsupportedControlFlow`）——§12.3(a) `route::value()` 须共用同一份证据与同一道前提 |
| `ir/set_oriented/generate.rs:386/400-408`、`lookup/mod.rs:252-259` | `ConversionPlan.route_guards` 字段与渲染点；`LookupRender{hit,value,default_sql,key_checks,…}` |
| `region/mod.rs:142`、`proof.rs:72/101/107/116`、`traps.rs:39/106/499` | `ExceptionScopePresent` 的 **8 个产出点**（G0 待分辨；互换探针只证明「该内容报此因」，未分辨站点） |
| `golden/…/PKG_SPLIT_TRADE_STEP3_SH.sql:926-935/938/939-946/948-975/978-993/995-1006/1008-1010` | 判别式源 / `IF v_kind='1'` / 多 OUT 调用臂 / 12 键 CASE 臂 / Top-1 lookup 臂 / 4 键 CASE 臂 / 两臂共用的 `WHEN OTHERS` |
| `golden/pr48_set_oriented_verify/REPORT.md:188`、`corpus/README.md:46` | 仓内**现存**的 purchase 判定记录：`Skipped(ExceptionScopePresent)` @ v0.2.1、语料侧 `kind_id='0'`（与本文引用的真机 `UnsupportedControlFlow` 不是一回事，§0/§12.4 G5） |
| `golden/…/cases/datasets/seed.sql:2-5`、`:24-25`、`sql/ddl/tables.sql:153-154` | 仓库自述的列宽缺陷（「CASE 路径须先修基线列宽后另行覆盖」）；`swh_all_kind` 仅有 `kind_id='0'` 行；`par_fund_seat_para` **无数据行**（D7 硬前置，§12.7） |
