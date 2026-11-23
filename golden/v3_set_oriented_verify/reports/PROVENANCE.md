# 取证环境（本包 reports/ 各日志的产生条件）

同一次取证，两类口径分开记：SpinDuck 侧只依赖被测 commit + release 构建；
参照侧额外依赖容器镜像与宿主模拟层。

| 项 | 值 |
|---|---|
| 被测 commit | `a33503597f149737175ee23abc77dffecd525`（PR #59 `feat/set-oriented-v3` head） |
| 该 commit CI | run `44967040478`：format / clippy / test / audit / golden 均 success |
| 被测 CLI | `cargo build --release --features cli`（stable，worktree 内），`spinduck version` = `spinduck 0.2.3` |
| 二进制新鲜度 | `target/release/spinduck` mtime 2026-11-22 09:15，晚于 commit 时间 08:31；`find src Cargo.toml -newer <bin>` 为空 |
| 内核 | DuckDB 1.5.5（bundled） |
| 语料工程 | PR head worktree 的 `golden/case_corpus_purchase`（含 byfund 依赖资产与 `cases/datasets_byfund/seed.sql`） |
| 宿主 | macOS 26.2，Darwin arm64（Apple Silicon），docker 29.9.2 |
| 参照实例 | 容器 `sd-og70`，`openGauss-lite 7.0.0-RC3 build 01b7e318`（x86_64-pc-linux-gnu，在 arm64 上模拟），宿主端口 5435 |
| 参照库 | `sdv3_purchase2`（purchase，归档日志对应）、`sdv3_byfund2`（byfund，归档日志对应）、`sdv3_purchase`（purchase 首轮，计时 168.6 s）、`sdv3_byfund`（byfund 首轮被客户端中断，残留 258 行产物，仅留作事故记录）。全部由 `reference/sp_deploy.sh` 新建，`DBCOMPATIBILITY 'A'`，零二级索引；四个库均为本次取证新建，未触碰既有 `postgres/demo/hz/sd59p/sd59b` |
| 凭据 | 口令仅从容器内环境变量 `$GS_PASSWORD` 取，本包文件与命令行不含明文口令 |
| 工作目录 | `/tmp/v3-set-oriented-verify-work`（DuckDB 文件、plan、每步 SQL 与输出） |

## 与 CI golden job 的口径差异

CI 的 golden job 用 `golden/gaussdb-docker`（openGauss **5.0.0**，amd64 runner）。
本包参照侧用的是 `sd-og70`（openGauss-lite **7.0.0-RC3**），即 validation V26.5
记录的替代参照口径：V21 的「参照实例 procedure CALL 崩溃」在 7.0.0-RC3 上不复现
（本轮 `sp_call.sh` 前后 `RestartCount=0`、`StartedAt` 未变），因此存过侧计时才可用。
两侧不同镜像版本 → 参照侧数字只作**量级对比**，不能当 golden 基线复算。

## reports/ 与命令对应

| 日志 | 产生命令 |
|---|---|
| `bench_<ds>_<N>.log` | `N=<N> bash run_verify.sh`（内部 `sd_bench.py`）；`<N>` = 2000（冒烟）、100000（正式）、0（空数据固定开销） |
| `cnt_<ds>_<N>.log` | 同上（内部 `statement_count.py`） |
| `ref_deploy_<ds>_100000.log` | `reference/sp_deploy.sh <ds> 100000`（`REF_DB=sdv3_purchase2` / `sdv3_byfund2`） |
| `ref_call_<ds>_100000.log` | `reference/sp_call.sh <ds> <REF_DB> 1` |
| `ref_cmp_purchase_100000.log` | `cmp_state.py <ON 库> sdv3_purchase2 dat_trd_repurchase` |
| `ref_cmp_byfund_100000.log` | `cmp_state.py <ON 库> sdv3_byfund2 dat_fund_cjqs dat_fund_xw_venture qfqscjhbrbb_temp db_log` |

被测 CLI 与语料树取自 PR head worktree；本包不改动被测 worktree（语料复制到
`/tmp/v3-set-oriented-verify-work/proj` 后再编译，byfund 入口在副本上追加）。
