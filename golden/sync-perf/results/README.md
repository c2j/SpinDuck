# sync 回写验证实测结果（2026-11-17，openGauss 5.0.0 A 模式容器）

环境：`golden/gaussdb-docker` compose 实例（4GB 内存 / 512MB shared_buffers），
SpinDuck `b3766eb` + 本目录修复补丁，release 构建 `--features gauss,cli`。
命令面：`spinduck sync --db <duckdb> --table <t> --cols id,v,w [--mode …]`。

## 1. 阻断缺陷（修复前全模式不可用，修复后 e2e 7/7 绿）

| 缺陷 | 位置 | 现象 | 修法 |
|---|---|---|---|
| G1 schema 守卫恒红 | `io/sync_tx.rs::guard_target_schema` | `coalesce(column_default,'')` 在 A 模式 `''`=NULL → 无默认值列被丢 → ENV-0021，append/slice/upsert 全灭 | `CASE WHEN column_default IS NOT NULL THEN 'Y' ELSE 'N' END` |
| G4 守卫 SQL 非法/覆盖不全 | `guard_unique_index` | `unnest(conkey)` 报 invalid reference；`conkey::text`=`{1}` 花括号切不动；裸 `CREATE UNIQUE INDEX`（无 pg_constraint）不可见 | 改 `pg_index` + `indkey::text` + `indisvalid`，排除 `indpred`/`indexprs` |
| upsert UPDATE 列名歧义 | `io/sync.rs::build_upsert_statements` | `WHERE tmp.k = k` 裸列 ambiguous → 42702（PG 系同样报错，死代码） | `UPDATE {t} AS tgt … WHERE tmp.k = tgt.k` |

## 2. 冲突处理矩阵（修复后，`tests/gauss_sync_e2e.rs` 7/7）

- PK 表 upsert：UPDATE+INSERT 混合正确、重跑幂等、行数交叉校验通过；
- append 撞 PK：COPY 单语句原子失败、目标零部分写入；
- 无约束表 upsert：ENV-0022 fail-closed 零写入；裸唯一索引表：放行；
- staging key 重复/NULL：BIZ-0103；slice：DELETE+COPY 同事务幂等。

## 3. append 吞吐（真实 sync 路径）

| 规模 | 耗时 | 吞吐 |
|---|---|---|
| 1M 行 → 1M 目标 | 1.07–1.08s | **~93 万行/s**（设计目标 ≥40 万，超额） |

## 4. upsert 分解（staging 1M、f=50%，目标 ~2M 行）

| 环节 | 耗时 |
|---|---|
| 客户端侧（G3 守卫 + DuckDB 导出 CSV + COPY 灌临时表 + CREATE TEMP） | ~1.0s |
| 服务端 COPY ingest | ~0.35s |
| `UPDATE … FROM`（Hash Join，全表扫目标） | ~2.2s（≈0.25s 固定 + ~4µs/命中行） |
| `INSERT … WHERE NOT EXISTS`（Hash Anti Join） | ~1.4–1.6s（≈0.25s 固定 + ~2µs/行） |
| **端到端 `sync --mode upsert`** | **~5.4s / 1M staging** |

密度扫描（目标 1M，hash 区制）：f=0% DML≈2.5s；f=10%≈3.4s；f=50%≈3.6s；
f=100%≈4.7s —— 成本随**命中行数**增长，稠密更新最贵。

## 5. 规模效应：join 策略是主要优化点

目标 5M 行、staging 20 万（S=200k）：

| 计划 | UPDATE | INSERT NOT EXISTS | 合计 |
|---|---|---|---|
| 默认（Merge Join，扫全量 5M PK 索引） | 1.40s | 1.41s | **2.81s** |
| 强制 Nested Loop（PK 索引点查） | 0.50s | 0.35s | **0.85s（~3.3×）** |

- Hash/Merge 成本 ∝ **目标行数**（seq scan ~0.3µs/行 + hash 构建，10M+ 或溢出 work_mem）；
- Nested Loop ~3–6µs/staging 行探测，与目标规模无关；
- 计划器只在 staging 极小（~1 万行）时自动选 NL；中间区间 cost model 系统性偏向全表扫；
- 经验分界：**S < 目标 reltuples/10 走 NL**（1M 目标临界 ~10 万；5M 目标 S=20 万 NL 已快 3.3×）。

## 6. 复现步骤

```bash
cd golden/gaussdb-docker && docker compose up -d   # 或直连真实例
export SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=5433 \
       SPINDUCK_OG_USER=gaussdb SPINDUCK_OG_PASSWORD=… SPINDUCK_OG_DB=postgres
export SPINDUCK_GSQL_DOCKER=1   # 容器内 gsql（本机直连则不设，用本机 gsql）

cd ../sync-perf/scripts
./gsql.sh < setup_tables.sql     # 建三种形态目标表 + 性能表
./gsql.sh < seed_perf.sql        # 灌 1M / 5M 目标数据（\timing 可见耗时）
./sweep_upsert.sh sd_perf_pk 10000000   # 密度×规模分解扫描（每轮自动清理）
./gsql.sh < nl_vs_hash.sql       # 计划器对照（需 sd_perf_pk5）

# 端到端（spinduck CLI，--features gauss,cli 构建）
cargo build --release --locked --features gauss,cli --manifest-path ../../spinduck/Cargo.toml
# 本地 staging 见 local_seed.sql（经 spinduck query 落 DuckDB）
spinduck sync --db perf.duckdb --table sd_perf_pk --cols id,v,w                 # append
spinduck sync --db perf.duckdb --table sd_perf_pk --cols id,v,w --mode upsert --key id
```

正确性矩阵由 `spinduck/tests/gauss_sync_e2e.rs` 常驻覆盖（env 门控，同 gauss_e2e 口径）。
