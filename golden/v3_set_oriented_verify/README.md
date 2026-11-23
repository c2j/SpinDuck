# PR #59 集合化 V3 实战复现包（purchase / byfund × 存过 / 非集合化 / 集合化）

本包复现 PR #59（`feat/set-oriented-v3`，`[engine].set_oriented` 在真实考题语料
`prc_trd_deal_purchase` 与 `prc_trd_hz_byfund` 上的准入修复）的**两条核验面**：

1. **数据一致性**：同一份数据、同一个过程，在
   `--set-oriented=off`（逐行 IR）与 `--set-oriented=batch-atomic`（集合化）下
   跑完后**全库状态逐值一致**；再与 openGauss 原生存过的产物做**跨引擎聚合一致**。
2. **性能**：三方同规模对比——openGauss 原生存过（SP）、SpinDuck 非集合化（OFF）、
   SpinDuck 集合化（ON），外加**内核语句数**作为「为什么快」的机制证据。

结论与限制见 [REPORT.md](./REPORT.md)。与 `golden/pr48_set_oriented_verify` 的
区别：那一包验证的是 PR #48 的**能力边界**（真实语料全部 `Skipped`，收益只在
准入面内）；本包验证的是 PR #59 把两条真实过程**推进准入面之后**的实战结果。

## 文件清单

```
v3_set_oriented_verify/
├── README.md                  ← 本文件（方法论 + 复现步骤）
├── REPORT.md                  ← 实测报告（三方数据 / 一致性 / 机制计数 / 限制）
├── run_verify.sh              ← 一键入口（SpinDuck 两侧；WITH_REFERENCE=1 追加存过侧）
├── sd_bench.py                ← SpinDuck 侧驱动：deploy → 种子 → 生成 N 行 → 计时 call → 全库摘要
├── statement_count.py         ← 机制证据：--trace-level all 的 SQL_EXEC/FETCH/CURSOR_OPEN 直方图
├── cmp_state.py               ← 跨引擎一致性：DuckDB 产物 vs openGauss 产物逐聚合值比对
├── sql/
│   ├── gen_purchase.sql       ← 驱动数据模板（:N 行，主游标逐行 ⇒ N 次循环）
│   ├── gen_byfund.sql         ← 驱动数据模板（:N 个分组 + 同形状债券主数据）
│   └── byfund_params_extra.sql← byfund 依赖补件（每条附理由，缺件则走业务失败分支）
├── reference/
│   ├── sp_deploy.sh           ← 参照侧：新建独立库并部署同一份语料对象 + 同一份数据
│   ├── sp_call.sh             ← 参照侧：postmaster 内 clock_timestamp() 夹住过程调用
│   └── sp_call_body.sql       ← 计时主体（DO 块，非 CALL 语句；理由见文件头）
└── reports/                   ← 归档实测日志（bench/cnt/ref_*）+ PROVENANCE.md（取证环境）
```

## 口径要点（决定数字怎么读）

- **规模定义**：`N` 是驱动数据行数。purchase 主游标是逐行 `SELECT` ⇒ `N` 行 = `N` 次
  循环体；byfund 主游标按含 `security_id` 的多键 `GROUP BY` ⇒ 生成器让每行一个
  distinct `security_id`，即 `N` 个分组 = `N` 次循环体。计时前由
  `sd_bench.py` 的 `cursor_groups()` 实测核对，不靠假设。
- **同一份数据、两条引擎**：`sql/gen_*.sql` 是唯一模板，参照侧 `sp_deploy.sh` 用
  sed 替换 `:N` 后喂给 gsql，SpinDuck 侧 `sd_bench.py` 替换后喂给 `query`。
  两侧种子也同源（语料 `cases/datasets*/seed.sql` 去掉 `mid_yjqs_detail` 的 INSERT）。
- **每个模式独立 deploy**：OFF 与 ON 各自新建 DuckDB 文件、灌同一份种子与同一份
  生成数据，避免「ON 用了 OFF 跑完的库」这种假等价。
- **计时面**：SpinDuck 侧是 `call` 子命令的**进程整段墙钟**（含 CLI 启动、plan 加载、
  DDL 无关）；参照侧是 postmaster **内部** `clock_timestamp()` 差值，不含连接与
  gsql 启动。两者基准不同，比较只看量级，不做「同一把尺」的宣称。
- **一致性判据**：同库 OFF/ON 用全库逐表行集排序后的 sha256 逐值相等；跨引擎用
  行数 + 每列聚合（数值 SUM/MIN/MAX，字符 COUNT DISTINCT/MIN/MAX）逐值比对。
  两侧序列化呈现差异（数字定标、日期格式）不进判据。
- **参照实例**：容器 `sd-og70` 里的 `openGauss-lite 7.0.0-RC3`（build 01b7e318，
  x86_64 镜像在 Apple Silicon 上模拟运行），`DBCOMPATIBILITY 'A'`。注意这**不是**
  CI golden job 用的 `golden/gaussdb-docker`（openGauss 5.0.0）——V21 记录的
  「参照实例 procedure CALL 崩溃」在 7.0.0-RC3 上不复现（validation V26.5 替代参照
  口径），存过侧计时因此才可用；也因此参照数字不能当 golden 基线复算。
  参照夹具按语料 DDL 建表，**零二级索引**——这是 SP 侧数字偏慢的上界因素之一，
  见 REPORT §限制。

## 环境要求

- Rust **stable** + 被测 head 的 **release** CLI（debug 构建的绝对值不可比）。
- 被测语料工程：默认自动选择「含 byfund 资产」的那棵树
  （主仓 `golden/case_corpus_purchase`，PR #59 合并后即命中；未合并时回退
  `../SpinDuck__worktrees/feat-set-oriented-v3/golden/case_corpus_purchase`）。
  可用 `CORPUS=` 显式指定。
- **无需** openGauss 容器：`WITH_REFERENCE` 默认 0，SpinDuck 两侧核验自成闭环。
- 参照侧（可选）：docker + 一个运行中的 `sd-og70` 容器（openGauss-lite 7.0.0-RC3，
  见 `reports/PROVENANCE.md`；`CONTAINER=` 可换名）。口令只从容器内 `$GS_PASSWORD`
  取，本包不落任何凭据。CI golden job 用的 `golden/gaussdb-docker`（5.0.0）
  **不能**直接顶替：V21 的 procedure CALL 崩溃面会让参照侧计时无法完成。

```bash
# 构建被测 CLI（在 PR head worktree 内）
cd ../SpinDuck__worktrees/feat-set-oriented-v3/spinduck
cargo build --release --features cli
```

## 复现步骤

### SpinDuck 两侧（一致性 + 性能 + 机制）

```bash
# 冒烟（约数分钟）
N=2000 bash golden/v3_set_oriented_verify/run_verify.sh

# 100K 全量（OFF 侧 byfund 约 3 分钟、purchase 约 1 分钟；ON 侧秒级）
bash golden/v3_set_oriented_verify/run_verify.sh

# 只做某一个过程 / 指定模式序列
DATASETS=purchase MODES="off strict batch-atomic" N=2000 \
  bash golden/v3_set_oriented_verify/run_verify.sh
```

产物：`reports/bench_<ds>_<N>.log`（逐模式 JSON + `BENCH_SUMMARY`）、
`reports/cnt_<ds>_<N>.log`（事件直方图与倍数）、工作库与每步 SQL/输出在
`/tmp/v3-set-oriented-verify-work/`。判定：`VERIFY_PASS` 且每个过程
`"state_equal": true`。

### 参照侧存过（需容器）

```bash
WITH_REFERENCE=1 N=100000 bash golden/v3_set_oriented_verify/run_verify.sh
# 或分步：
CORPUS=<被测 head>/golden/case_corpus_purchase \
  bash golden/v3_set_oriented_verify/reference/sp_deploy.sh purchase 100000
bash golden/v3_set_oriented_verify/reference/sp_call.sh purchase sdv3_purchase 1
python3 golden/v3_set_oriented_verify/cmp_state.py \
  /tmp/v3-set-oriented-verify-work/db-purchase-100000-batch-atomic.duckdb \
  sdv3_purchase dat_trd_repurchase
```

参照侧库名默认 `sdv3_<dataset>`：脚本只 `CREATE` 新库，绝不 `DROP`/`ALTER` 既有库；
重跑请换 `REF_DB=`（`sp_deploy.sh` 在 `mid_yjqs_detail` 非空且未设 `ALLOW_APPEND=1`
时直接退出，防止二次灌数把行数翻倍）。`byfund` 的 SP 侧一轮在模拟 amd64 上约 **1 小时**
量级，建议 `REF_ROUNDS=1` 并放到后台跑。

中途取消参照侧长调用要留意：客户端断连后服务端可能已按内部 `COMMIT` 落了部分产物，
参照库即不再干净——换新 `REF_DB` 重投，不要在同一库上续跑。

### 手工分步（排查用）

```bash
export SPINDUCK_BIN=<abs>/spinduck/target/release/spinduck
export CORPUS=<abs>/golden/case_corpus_purchase WORK=/tmp/v3-set-oriented-verify-work
python3 golden/v3_set_oriented_verify/sd_bench.py byfund 2000 off batch-atomic
python3 golden/v3_set_oriented_verify/statement_count.py byfund 2000 off batch-atomic
```

## 核验

把 `reports/` 与 [REPORT.md](./REPORT.md) 的表对照：

- `BENCH_SUMMARY` 里 `"state_equal": true`，`verdict` 含 `Converted`（ON 侧）
  与 `Skipped`/无改写（OFF 侧）。
- 行数：purchase `dat_trd_repurchase = N`；byfund `dat_fund_cjqs ≈ N+1`
  （种子 2 行明细并入首个分组，故 +1）。
- 机制：`SQL_EXEC` 由 OFF 的千级降到 ON 的十级，倍数与加速比同量级。
- 跨引擎：`cmp_state.py` 末行 `STATE_EQUAL`。

绝对墙钟随机器/构建漂移，**只核验方向与量级**。
