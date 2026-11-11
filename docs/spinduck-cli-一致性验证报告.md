# SpinDuck CLI 子命令一致性验证报告

- 验证对象：`spinduck` 二进制 v0.1.6（`cargo build --features cli --bin spinduck`，本地实际执行，非仅源码审读）
- 验证范围：全部 13 个子命令（version / help / compile / rules probe / explain / verify / plan / deploy / query / compare / materialize / sync / call 两种形态）
- 验证方式：每命令跑正向 + 缺件 + 非法旗标 + 边界用例，逐条记录退出码与输出；fixture 在 `/tmp` 隔离目录（golden/case_split_agg 副本 + 自建规则项目），未污染仓库工作区
- 日期：2026-11-10

---

## 结论一览

退出码契约（0 成功 / 1 业务失败 / 2 用法错）在全部命令上**一致成立**，旗标任意位置可放（deploy/query/compare 实测顺序自由）。
但发现 **1 个高危行为口径分裂、2 个参数形态不一致、2 个冲突/静默覆盖、若干文档漂移**，逐条如下。

---

## F1【高】`call` 不加载项目规则，与 compile/plan/verify 口径分裂（实测"体检绿、运行红"）

**证据（实测）**：

- 自建项目 `rproj/`：`spinduck.toml [paths].rules = ["rules/custom.toml"]`，`custom.toml` 声明 `MYNVL = "coalesce"`，例程 `fnc_nv` 使用 `MYNVL`。
- `spinduck compile rproj/sql/routines` → **OK, exit 0**（compile 自动发现项目规则，见 `run.rs::rules_for_corpus`，其注释明言"避免'体检绿、运行红'的口径分裂"）。
- `spinduck call --file r.sql fnc_nv 5 null` → **KERNEL-0010 `mynvl does not exist`, exit 1**（call 只用内置种子 + `--rules`，不做项目发现，见 `run.rs::call_cmd`）。
- `spinduck call --file r.sql --rules custom.toml fnc_nv 5 null` → RETURN 5, exit 0。

`call --plan` 形态同样不受项目规则影响：`call_cmd` 在解析计划**之前**就构建了规则表（仅种子 + `--rules`），计划里的 `project_dir` 只用于定位源文件，不回读 `[paths].rules`。

**风险**：正是 `rules_for_corpus` 注释声称要杜绝的那条分裂路径。建议：call 两种形态都叠加 `target`（或计划 `project_dir`）向上发现的项目规则，`--rules` 保留为追加覆盖。

---

## F2【中】本地库文件参数形态分裂：`query` 用位置参数，其余全用 `--db`

| 子命令 | 库文件形态 |
|---|---|
| deploy | `--db <f>`（位置参数是计划文件） |
| query | `<本地 DuckDB 文件>`（位置参数） |
| materialize / sync / call | `--db <f>` |

**加重项**：`args.rs` `parse_query` 文档注释写"库文件是位置参数而不是 `--db`，**与 `deploy` 同形**"——与实际不符：deploy 的位置参数是计划、db 是旗标，两命令并非同形。要么统一为 `--db`（query 改旗标），要么改注释并写清取舍理由。

---

## F3【中】"项目目录"参数形态三分：verify 位置参数 / plan `--dir` 旗标 / compile 位置参数

同一概念三种接口（实测均可用）：

- `verify /tmp/proj`（位置，可省略默认 cwd）
- `plan --dir /tmp/proj <routine>`（旗标）
- `compile /tmp/proj`（位置）

verify 与 plan 复用同一个 `locate()`，却一个位置一个旗标。建议收敛：若坚持 plan 用 `--dir`，至少在 usage/文档里给出与 verify 的对照理由。

---

## F4【中】`compile` 不拒绝 `--` 开头参数，破坏全局旗标纪律（实测）

- `spinduck compile --help` → `[ENV-0010] read --help: No such file or directory`，**exit 1**（`--help` 被当路径逐个读）。
- 其余全部子命令对未知 `--` 参数一律 `[ENG-0010] unknown flag`，**exit 2 + 全文用法**。

compile 是唯一不检查 `t.starts_with("--")` 的解析分支（`args.rs` parse_compile 把 `argv[1..]` 全部映射为路径）。用户拼错旗标（如 `compile --rules x dir`）得到的不是用法提示而是文件读取失败。建议：compile 同样拒绝 `--` 前缀（或显式支持 `--help`）。

---

## F5【中】重复旗标静默 last-wins，与位置参数"重复即报错"两套纪律并存（实测）

| 命令 | 重复用法 | 实测结果 |
|---|---|---|
| `explain --file a.sql --file b.sql foo` | 两次 --file | 静默采用 b.sql（a 被丢弃） |
| `deploy --db x --db y p.toml` | 两次 --db | 静默采用 y |
| `query --sql-file s1 --sql-file s2` | 两次 --sql-file | 静默采用 s2 |
| `compare --key k1 --key k2` | 两次 --key | 静默采用 k2 |
| `call --file a --file a` | 同一文件两次 | 静默注册两遍 |
| `plan a b` / `deploy p1 p2` / `query db1 db2` | 位置参数重复 | **ENG-0010 报错** |

位置参数过度会报"只接受一个…"，旗标过度静默覆盖——容易掩盖手误（尤其 `--db` 指错库文件属破坏性操作面）。建议至少对非可多次旗标（--db/--out/--sql-file/explain --file）重复给 ENG-0010；`call --file` 是有意可多次的，但对重复路径可告警。

---

## F6【中】`call --plan` 下位置参数全部当实参，例程名误入无防护（实测）

`call --plan fnc_fee.toml fnc_fee`（忘记删除例程名）→ `[BIZ-0112] invalid numeric literal: fnc_fee`——fnc_fee 被静默当作第一个实参传给函数，报错信息对根因毫无指认。usage 文本虽有"（--plan 下位置参数全是实参）"说明，但计划文件里就写着 `target = "fnc_fee"`，第一位置参数 == target 时应当直接用法错。低成本高收益的防护点。

---

## F7【低】共享错误消息硬编码 "verify" 字样，plan 触发时同样显示（实测）

`spinduck plan fnc_fee`（无项目声明）输出：
`[ENV-0030] 未在 … 找到项目声明（…；**verify 以项目声明为起点**）`

消息来自 `cli/verify.rs::locate()`（verify/plan 共用），把 verify 的语境带进了 plan。建议消息去掉子命令名或按调用方注入。

---

## F8【低】usage 文本与解析器能力不符：`call --plan` 形态未列 `--rules`

usage 第二形态：`call --plan <计划.toml> [--db <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] [args...]`——没有 `--rules`，但解析器接受（实测 `call --plan p.toml --rules r.toml` 进入规则读取分支）。若属有意（计划形态不建议手动覆盖规则），应显式拒绝并在文档写明；否则补 usage。

顺带：`mod.rs::read_source_strict` 文档称诊断"仍是 `ENV-0030` 形状"，实测缺文件读错误是 `ENV-0010`（`source.rs::read_text` env(10)），仅编码失败才是 ENV-0030——注释漂移。

---

## F9【低】规则供给三通道并存（设计说明，非必改）

1. 项目声明自动发现（compile/verify/plan，见 F1：call 缺席）
2. `call --rules` 手动叠加
3. `rules probe <f>` 版本锚定检查（用途独立，可保留）

收敛 F1 后建议在 usage/architecture 写明三通道职责边界，避免使用者再发明第 4 种组合。

---

## 实测通过面（无需整改）

- 退出码契约 0/1/2 全命令一致；用法错必附全文 usage。
- `version`/`help`/`--version`/`-V`/`--help`/`-h`/空 argv 六入口行为一致。
- deploy/query 旗标任意顺序（`--db` 前/后置均通过）；query 缺旗标报错精确指认缺哪个。
- compare 恰好两快照 + `--key` 空值防护（`--key ""` / `--key ,` 均拒）。
- `--step-budget 0` / 非数字 / 缺值三种非法均拒；`call` 无例程名报 usage。
- plan→deploy→query→compare 全链路（fnc_fee、prc_agg_fund，含 --schema）跑通，缺件门、计划落盘幂等（updated/unchanged）正常。
- materialize/sync 在无 `gauss` feature 下给出明确 `requires feature \`gauss\``（ENV-0012），参数校验先于 feature 判定。

---

## 建议整改优先级

| 优先级 | 条目 | 动作 |
|---|---|---|
| P0 | F1 | call 两形态叠加项目规则发现（守卫测试：compile 绿的例程 call 不得红于规则缺失） |
| P1 | F4 | compile 拒绝 `--` 前缀参数 |
| P1 | F5 | 非可多次旗标重复报用法错 |
| P2 | F2 | 统一库文件形态（建议 query 改 `--db`，或改注释写明取舍） |
| P2 | F6 | `--plan` 下第一位置参数 == target 时报用法错 |
| P3 | F3 / F7 / F8 | 文档与消息对齐 |
