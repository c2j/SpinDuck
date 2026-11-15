# PR #48 集合化 —— 真实语料 CLI 验证（case_corpus_purchase）

用 `golden/case_corpus_purchase/sql/routines/` 下的**原始考题语料 SQL** 验证 PR #48
集合化（`[engine].set_oriented`）的转换判定与语义等价。**不写任何 Rust**：
全靠「特殊项目 toml 配置 + `compile`/`deploy`/`call` 指令」驱动，
逐例程捕获 CLI 输出的 `SET_ORIENTED <routine> converted/skipped reason=...` 判定行。

> 与上层 [probe/](../probe/) 的玩具差分探针互补：探针在受控构造例程上扫边界，
> 本包在**真实生产语料**上验证「集合化是否会触发、触发后是否等价」。

## 文件清单

```
corpus/
├── README.md                  ← 本文件
├── run_corpus_verify.sh       ← 一键脚本（构建 PR-head CLI → 特殊项目 → compile/deploy/call → ON/OFF 差分断言）
└── reports/
    ├── corpus_on.log          ← set_oriented=true 逐例程输出（含 SET_ORIENTED 判定）
    ├── corpus_off.log         ← set_oriented=false 逐例程输出（业务基线）
    └── corpus_summary.log     ← 汇总表 + 等价性结论
```

原始 SQL 不复制进本目录入库——脚本运行时从 `golden/case_corpus_purchase/sql/`
**逐字复制**到临时项目，保证始终基于原始语料。

## 方法论

1. **特殊项目 toml**：临时项目声明 `[engine].set_oriented = true`（OFF 轮翻为 false），
   `[paths]` 指向复制来的原始 `sql/ddl`、`sql/routines`、`sql/stubs`、`rules`。
   CLI 的 `call` 从项目 toml 发现该开关（`--file` 形态向上发现、`--plan` 形态读项目）。
2. **compile --analyze**：进程内嵌 codeweb 取图，按入口落运行计划并做 IR 体检。
   需 `codeweb.toml` 标记（脚本自动生成，`[analysis].paths=["sql/"]`）。
3. **deploy**：按计划把 DDL 表 + 例程装进落盘 DuckDB。
4. **call**：逐例程调用，CLI 在调用起点生成集合化判定并打印 `SET_ORIENTED` 行，
   随后打印业务输出（`RETURN`/`OUT`）。
5. **差分断言**：ON 日志去掉 `SET_ORIENTED` 行后与 OFF 日志**逐字 diff 为空**（等价）；
   并断言四个例程的 Skipped 原因符合结构预期。

## 实测结果（head f62787d，DuckDB 1.5.5，空种子确定性路径）

| 例程 | ON 判定 | 业务输出（ON==OFF） |
|---|---|---|
| `fnc_com_getday` | `Skipped(UnsupportedControlFlow)` | `RETURN 20250716` |
| `fnc_com_if_deal_date` | `Skipped(UnsupportedControlFlow)` | `RETURN 0` |
| `FNC_GET_PURCHASE_JS_DAYS` | `Skipped(ExceptionScopePresent)` | `RETURN 0` |
| `pkg_split_trade_step3_sh.prc_trd_deal_purchase` | `Skipped(ExceptionScopePresent)` | `OUT p_o_succeed = 0` |

**结论**：四个真实语料例程**全部 Skipped**——PR #48 的集合化不在生产语料上触发。
原因与结构一致：独立函数是单 `RETURN` 表达式（非「游标+纯投影+单 INSERT」六指令套路，
故 `UnsupportedControlFlow`）；JS_DAYS 与主过程含 `EXCEPTION WHEN OTHERS` 作用域
（`ExceptionScopePresent`）。ON/OFF 业务输出逐字一致，等价不变量成立。
这与上层探针报告「PR #48 是路线一模式 C 的最窄落点」相互印证：
真实金融过程的表达式 / 分支 / 异常 / 多写形态都落在准入边界之外。

## 复现

```bash
# 仓库根目录下执行
bash golden/pr48_set_oriented_verify/corpus/run_corpus_verify.sh
# 复用已构建的 PR-head CLI（跳过约 10 分钟的首次构建）：
SPINDUCK_BIN=/path/to/spinduck bash golden/pr48_set_oriented_verify/corpus/run_corpus_verify.sh
KEEP_WORKTREE=1 bash ...   # 保留临时项目目录供检查
```

脚本自包含：未给 `SPINDUCK_BIN` 时自动 fetch PR head、建临时 worktree、初始化
`ogsql-parser` 子模块、`cargo build --features cli`（复用主仓 target 缓存），
退出时清理。无需 GaussDB 容器。

## 口径与边界（务必阅读）

- **空种子确定性路径**：deploy 只建表结构、不灌业务数据。主过程在空数据下
  主游标零次迭代，走「删除空数据 → 开游标 → 零次循环 → `p_o_succeed='0'`」
  确定性成功路径。这足以捕获**调用起点**生成的集合化判定并验证 ON/OFF 等价；
  **带业务数据的多维结果断言**是黄金原生断言层（`cases_native`）的职责，
  不在本纯 CLI 包范围内（那需要 Rust，且需完整外部依赖桩）。
- **外部依赖 `fnc_com_get_seat_para`**：语料主过程引用但语料内未提供（与日志桩
  `pkg_split_trade_log` 同类的外部件）。`compile` 因此报 `missing=1` 并退出 1，
  但计划照常写出、IR 体检 `failed=0`。脚本以「计划已生成 且 failed=0」为成功判据，
  并在部署计划中剥离 `[[missing]]` 段——`call --plan` 的缺件门只核表/视图/序列，
  空种子下该外部函数不会被调用，故无需伪造其业务行为。
- **判定是纯结构的**：`analyze()` 在 IR 上做六指令模式匹配，对上述例程在查询任何
  表结构之前就拒绝；集合化判定不依赖业务数据，因此空种子不影响判定结论。
- 性能不在本包范围（见上层 [reports/perf.log](../reports/perf.log)）。