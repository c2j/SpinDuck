# SpinDuck CLI 一致性修复设计

> 依据：[spinduck-cli-一致性验证报告.md](spinduck-cli-一致性验证报告.md)（2026-11-10 实测）
> 状态：**设计待审核，未实施**。批准后按 §7 顺序逐项走 TDD（一次循环一个行为）。

---

## 0. 总原则（对所有项生效）

1. CLI 仍是壳（AGENTS §1/§2.1）：全部修改落在 `src/cli/` 与 `src/source.rs` 注释，不触碰引擎、接缝签名、规则数据。
2. 退出码契约不动摇：0 成功 / 1 业务失败 / 2 用法错。新增的拒绝一律是用法错（`ENG-0010`，exit 2）。
3. 每项先写**会失败的守护/行为测试**（Red）再改实现（Green），测试命名 `should_*`；人类已有测试（`tests/cli/*.rs`）只读——凡需要动它们的项，逐条标 ⚠️ **Ask first**。
4. 不新增依赖、不新增 feature flag、不改 usage 文本以外的对外可见行为除非本设计写明。

---

## D-F1【P0】`call` 叠加项目规则发现（消除"体检绿、运行红"）

### 现状
`run.rs::call_cmd` 只构建 `RuleRegistry::default()` + `--rules` overlay；`rules_for_corpus`（compile 专用）会向上发现项目 `[paths].rules`。call 两种形态都不走发现。

### 方案
1. 把 `rules_for_corpus(paths)` 改名为 `rules_for_paths(paths)`（语义已通用：对一组路径做项目规则发现并叠加），返回签名不变。compile 调用点同步改名。
2. `call_cmd` 构建规则表改为：
   ```
   seed 基座
     → 叠加发现的项目规则（发现根 = 源文件路径集合）
     → 叠加 --rules（保留为最高优先级手动覆盖，§6 只增不删语义不变）
   ```
3. **发现根的取法**（两种形态）：
   - `Routine` 形态：`parts.files` 里的每个 `--file` 路径（与 compile 同口径：从语料路径向上找 `spinduck.toml`）。`files` 为空时不发现，维持现状（纯种子规则）。
   - `Plan` 形态：计划已带 `project_dir`，`call_target_and_sources` 已算出 `project`（`ProjectConfig`）——直接取 `project.rules_files`，不再对计划文件本身做向上发现（计划就在项目里，两种来源必然命中同一声明；取 project 是精确路径，不受计划文件被软链等影响）。
4. 多个 `--file` 分属不同项目时：发现文件取并集去重（`BTreeSet`，与 `rules_for_corpus` 现有做法一致）。
5. **优先级声明**（写进 `call_cmd` 注释与 usage 不动）：seed < 项目发现 < `--rules`。与 compile/verify 的"叠加只增不删"一致，但 call 的 `--rules` 在叠加链最顶——这是现状语义的延伸，不是新规则。

### 变更面
- `src/cli/run.rs`：`rules_for_corpus` → `rules_for_paths`（含其测试）；`call_cmd` 规则构建段；`call_target_and_sources` 返回值附带 `project`（或改为返回 `(target, sources, rules_files)`，避免二次 locate）。
- `src/cli/args.rs`：无改动。

### 测试（新建，Red→Green）
- `should_load_project_rules_for_call_routine_form`：--file 指向带 `[paths].rules` 的项目内源文件，例程用自定义函数映射，不传 --rules 必须调用成功（复刻验证报告里的 MYNVL 场景，fix 前必红）。
- `should_load_project_rules_for_call_plan_form`：plan 形态同样不传 --rules 调用成功。
- `should_apply_rules_flag_after_project_rules_for_call`：项目规则与 `--rules` 对同一函数名都给出映射时，`--rules` 的映射生效（锁定优先级）。
- `should_call_with_seed_rules_when_no_project_declared`：项目外的 `--file` 保持纯种子规则（守护现状，防发现逻辑扩大化）。

### 兼容性
不传 `--rules` 且项目有规则的既有调用方：从红变绿，属修复非破坏。显式 `--rules` 调用方行为不变（覆盖链顶）。

---

## D-F4【P1】`compile` 拒绝 `--` 前缀参数

### 方案
`args.rs::parse_compile` 在映射路径前检查：`t.starts_with("--")` → `usage_err(format!("compile 不接受旗标，未知参数: {t}"))`。**不支持** `compile --help` 单独展开为用法文本（与"每子命令一处 usage"现状一致，全局 `help` 已可用；若你要求逐子命令 --help，见 §8 决策点 D4）。

### 变更面
`src/cli/args.rs::parse_compile`（约 4 行）。

### 测试
- `should_reject_compile_with_flag_argument`：`compile --help` → ENG-0010、exit 2、消息含 `compile 不接受旗标`。
- `should_reject_compile_with_db_like_flag`：`compile --rules x dir` → ENG-0010（防手误把其他子命令旗标带进来）。
- 既有 `should_parse_compile_with_multiple_paths` / `should_reject_compile_without_path` 不得变动（守护）。

---

## D-F5【P1】单值旗标重复 → 用法错

### 方案
各解析器对**语义上单值**的旗标，赋值前检查"已给过"即 `usage_err(format!("重复旗标 {t}"))`：

| 解析器 | 纳入单值检查的旗标 |
|---|---|
| plan | `--dir`、`--schema` |
| deploy | `--db` |
| query | `--sql-file`、`--out` |
| compare | `--key`（单键组语义；多键走逗号分隔，不给第二次机会） |
| explain | `--file` |
| materialize | `--db`、`--table`、`--where` |
| sync | `--db`、`--table`、`--cols` |
| call | `--db`、`--rules`、`--plan`、`--step-budget` |

**不纳入**：`call --file`（有意可多次，保持现状；重复同一路径不另行去重/报错——注册两遍无害，若你要去重见决策点 D5）。

实现为解析器内一行 guard（`if x.is_some() { return Err(...) }`），不引入通用旗标结构体——改动面已控制在每分支 1–2 行，抽象收益不抵结构变动（§3：不为超出改动路径重构）。

### 变更面
仅 `src/cli/args.rs` 各 `parse_*`。

### 测试
- `should_reject_duplicate_single_value_flag`（参数化表驱动：上表 8 组各一例，断言 ENG-0010 + 消息含 `重复旗标`）。
- `should_accept_repeated_file_flag_for_call`：守护 `call --file a --file b` 仍合法。
- ⚠️ **Ask first**：`tests/cli/` 若有用例依赖重复旗标 last-wins（初查未见，实施前逐文件确认），按 AGENTS 先请示。

---

## D-F6【P2】`call --plan` 下位置参数首项 == 计划 target → 用法错

### 方案
检查点放在 **`args.rs::parse_call`**（解析层，保证 exit 2），但 parse 今天是纯函数不读文件。折衷设计：

- parse 阶段**仅当** `--plan` 存在且存在位置参数时，读取计划文件提取 `target`（`RunPlan::from_toml_str` 已有解析器，复用；只取 `target` 字段）。
- 文件读取失败**不在 parse 阶段报错**（保持"解析不产生 I/O 错误"的隐性契约）——读不到就跳过本检查，错误照旧由执行层（BIZ/ENV 面）给出。
- `args.first() == plan.target` → `usage_err("call --plan 下位置参数全是实参；目标例程来自计划（{target}），请去掉位置参数或改用无计划形态")`。

备选方案（不推荐）：检查放 `run.rs::call_target_and_sources`，实现最薄但退出码只能是 1，破坏用法错契约。

### 变更面
- `src/cli/args.rs::parse_call`：+15 行左右（含 target 提取）。
- `src/cli/plan.rs` 的 `RunPlan` 若字段非 pub 需补一条只读访问（现 `target` 已是 pub 字段，预计零改动）。

### 测试
- `should_reject_call_plan_with_target_as_first_positional`：`call --plan p.toml fnc_fee` → ENG-0010、exit 2。
- `should_still_treat_positionals_as_args_under_call_plan`：`call --plan p.toml '20250715'` 仍解析为实参（守护既有行为，对应既有测试 `should_take_every_positional_as_an_argument_when_call_uses_a_plan` 不得变）。
- `should_skip_target_check_when_plan_unreadable`：计划文件不存在时 parse 不报用法错（错误留给执行层）。

---

## D-F7【P3】`locate()` 错误消息去子命令绑定

### 方案
`verify.rs::locate()` 消息改为：
`未在 {start} 及其上级目录找到项目声明（spinduck.toml 或 conf/spinduck.toml）`
——删除"；verify 以项目声明为起点"从句。verify/plan 各自的 usage 行已说明起点语义，消息不需要再带子命令名（也免去将来新调用方再复制粘贴错）。

### 变更面
`src/cli/verify.rs::locate` 一行字符串。

### 测试
- `should_locate_error_mention_only_project_declaration`：verify 与 plan 触发同一消息且不含 "verify" 字样。
- ⚠️ **Ask first**：`tests/cli/verify.rs` 现有断言只查 `contains("spinduck.toml")`（初查），不受影响；实施前确认 `src/cli/verify.rs` 与 `plan.rs` 的 `#[cfg(test)]` 内嵌测试无对从句的断言。

---

## D-F8【P3】usage 文本与注释对齐

1. usage 的 `call --plan` 形态补 `--rules`：
   `call --plan <计划.toml> [--db <f>] [--rules <f>] [--file <sql>]... [--batch-commit] [--step-budget <n>] [args...]`
   （与 D-F1 后"--rules 仍为覆盖链顶"一致，解析器本就接受。）
2. `args.rs::usage()` 中 call 两形态行尾注释补一句：`（规则：seed < 项目发现 < --rules）`——三通道职责一句话写清（对应报告 F9，不另立文档）。
3. `src/cli/mod.rs::read_source_strict` 文档注释修正："缺文件等 I/O 失败为 `ENV-0010`（`source.rs` env(10)），编码双判失败为 `ENV-0030`（D9①②）"。

### 测试
- `usage_snapshot_should_mention_rules_in_plan_form`：对 `usage()` 输出断言含 `--rules` 于 --plan 行（快照式断言该行全文，防再次漂移）。
- 无行为变化，纯文档；不触碰人类测试。

---

## D-F2【P2，待你拍板】库文件参数形态统一

**方案 A（推荐）：`query` 库文件改 `--db` 旗标，与 deploy/materialize/sync/call 四命令一致。**
- 新形态：`query --db <f> --sql-file <F.sql> --out <F.json>`，三件全旗标、顺序自由。
- 变更面：`args.rs::parse_query`、`usage()`、`run.rs::exec` 解构不变（Command::Query 字段不变）；`tests/cli/query.rs` 需改（⚠️ **Ask first**：人类契约测试）。
- 代价：脚本/文档中既有 `query <db> ...` 调用式会红。可接受面：CLI 尚在 0.1.x、query 属研发工具链，且报告已把现状判为注释失实的"伪同形"。**不提供位置参数兼容期**（双形态并存 = 把不一致制度化）。

**方案 B（保守）：维持现状，仅改 `args.rs::parse_query` 注释**为"query 库文件取位置参数，与 deploy 不同形：deploy 的位置参数是计划、db 用旗标；取舍理由 = query 三件输入里 db 最像'操作数'…"——即承认分裂、写明理由。

另需同步修正 `args.rs` 中"与 `deploy` 同形"这句失实注释（无论选哪个方案都改）。

**推荐 A**。若你否决，落 B。

---

## D-F3【P3，建议不改，写明理由】

"项目目录"三形态（verify 位置 / plan `--dir` / compile 位置）收敛空间有限：plan 的位置参数已被 routine 占用，`--dir` 是唯一无冲突放法；verify 与 compile 的目录是主操作数，位置参数恰当。**建议不改接口**，仅在 `args.rs::parse_plan` 的 `--dir` 分支补一行注释写明上述取舍（与 usage 无关，纯代码内说明）。若你要求强收敛（如 verify 加 `--dir` 别名），见决策点 D3。

---

## 7. 实施顺序与门禁

按依赖与风险排序，每项独立 TDD 循环，独立可提交：

| 序 | 项 | 前置 |
|---|---|---|
| 1 | F1 规则发现 | 无（P0，先做） |
| 2 | F4 compile 拒旗标 | 无 |
| 3 | F5 重复旗标拒绝 | 无 |
| 4 | F6 --plan 位置参数防护 | 无 |
| 5 | F8 usage/注释对齐 | F1 之后（口径才定稿） |
| 6 | F7 locate 消息 | 无 |
| 7 | F2 query --db（若批 A） | 无，单独 PR |
| 8 | F3 注释 | 随任一 PR 顺带 |

每项完成后跑：`cargo test --all-features <test_name>` → 全量 `cargo test --features cli`；合入前跑 §8 全门禁（fmt / clippy / test / audit）。规则数据、`sql/exec.rs` 接缝、golden 期望值均不触碰。

## 8. 决策点（请逐条批复）

| # | 问题 | 我的建议 |
|---|---|---|
| D1 | F1 优先级链 `seed < 项目发现 < --rules` 是否成立？（备选：--rules 压项目，或项目压 --rules） | 如文；"手动旗标覆盖一切"最符合工具直觉 |
| D2 | F2 选方案 A（query 改 --db）还是 B（只改注释）？ | A |
| D3 | F3 是否需要 verify 增设 `--dir` 别名强收敛？ | 不需要，注释写理由即可 |
| D4 | 是否为每个子命令实现 `--help`（逐命令用法文本）？成本：usage() 需拆分 + 各 parse 分支 + 测试，约一个独立循环 | 暂不做，单独立项评估 |
| D5 | `call --file` 重复同一路径是否去重/告警？ | 不动（无害且现状测试锁定 files 列表逐字相等） |
| D6 | F6 的检查点：parse 读计划文件（本设计的折衷）还是接受 exit 1 的执行层检查？ | 如文（parse，保 exit 2） |

## 9. 明确不做

- 不引入 clap/argh 等解析库（AGENTS §1 零依赖手写口径）。
- 不为 usage 错误增加 stderr 分流（现状全走 stdout 是契约层既定行为）。
- 不动 materialize/sync 的 gauss feature 报错路径（验证已确认其行为合理）。
- 不改 `Command` 枚举字段名/形状（除 F2 若选 A 亦不动字段，仅解析面变化）。
