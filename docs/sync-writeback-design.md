# sync 回写改进设计（部分回写 + 模式化 + 快路径）

> 状态：**已实现（feat/sync-writeback），GaussDB 容器 E2E 待补**。参照实现：fastaas `prototype-1.7/dota-scheduler/
> dota-adapter/src/main/java/com/dota/commit`（回写框架五层拆解见 §2）。
> 本文只承载机制设计：实证数字落地后更新 `docs/validation.md`，机制描述在实现
> 合入时同步 `docs/architecture.md` §13.2——三者不互相越界（AGENTS §3）。

## 1. 背景与问题

`sync --db <f> --table <t> --cols a,b`（`spinduck/src/io/gauss.rs::sync_append`，
V26 通道）现状：

- **功能面**：仅 append 全表回写——无 WHERE 过滤、无模式选择；重跑必翻倍
  （append-only 幂等性缺失）；列面只有 `--cols` 白名单，无改名/剔除/补值。
- **性能面**：客户端逐 cell `csv_field` 字符串分配 + 每行两次 `write_all`。
  V26.4 实测 98k 行 0.89s（≈110k 行/s）；同仓库物化 COPY STREAM 快路径
  100k 行 0.216s（≈463k 行/s）——4 倍差距全部来自回写侧的逐行客户端编码，
  而非网络或服务端。

### 1.1 与 fastaas 回写框架的差距对照

| 维度 | fastaas（参照） | SpinDuck sync（现状） | 本设计取舍 |
|---|---|---|---|
| 回写模式 | SLICE（幂等）/ UPSERT / APPEND | 仅 append | 移植 slice / upsert（§4.2/§4.3），append 保默认契约 |
| 数据通道 | DuckDB `COPY TO` CSV 文件 → `copyIn(reader)` 流式 | 逐行客户端编码 | 移植文件中转通道（§4.1） |
| 列映射 | `ColumnResolver`：rename/exclude/constants | `--cols` 白名单 | **本期不做**（`--cols` 保留；哑翻译层列为 §8 展望） |
| 前置守卫 | schema 漂移 / slice / key / 目标 unique 索引四族 | 仅本地 `ensure_local_table` | 移植四族中与模式相关的三族（§5） |
| 事务单元 | commit unit + compensation（多目标） | 单事务单表 | 多表 unit 超出本期范围（§8） |
| 逐行 JDBC UPSERT | UPDATE-then-INSERT 逐行 + 23505 重试 | — | 不移植逐行形态；Rust 侧改 staging 表 join 式 upsert（§4.3，天然规避竞态） |

### 1.2 设计原则

1. **默认行为零变更**：`sync` 无新参数时输出与语义与现状逐字一致（`SYNC {t} rows={n}`、
   append、全表）；全部新能力经显式旗标开启。
2. **过滤与目标侧语义绑定**：读取过滤（WHERE）单独存在只是"少写一点的 append"，
   容易被误当增量同步；凡带过滤的写入，必须让用户显式声明目标侧语义
   （slice=先删后灌 / upsert=键控合并），usage 与输出都要显性标注。
3. **守卫全部前置于触碰目标数据**（fastaas 的 fail-closed 纪律）：任何守卫失败
   不得留下部分写入；唯一例外是 append 模式维持现状（copy_in 单事务天然原子）。
4. **通道对称**：回写快路径与物化 V27 快路径同一套 workdir/成功删失败保留语义，
   CSV 口径两侧统一。

## 2. 参照实现要点（fastaas `com.dota.commit`）

- `GaussCopyCommitter`：DuckDB `COPY (SELECT …) TO` CSV → `CopyManager.copyIn`
  流式灌入，单事务 DELETE+COPY；失败保留 CSV 供诊断 + audit。
- `GaussTargetCommitter`（JDBC 模式）：SLICE = 按 biz_date DELETE + batch INSERT
  （同事务）；UPSERT = keyed UPDATE-then-INSERT，23505 唯一冲突重试
  （`GaussTargetCommitter.upsertOnConnection`），UPDATE 匹配数 ≠ 1 即
  `UPSERT_KEY_NOT_UNIQUE` 红。
- `ColumnResolver`：exclude / rename / constants 哑翻译，exclude 解析不到
  fail-closed；`TargetSchemaGuard` 在触碰目标前校验列集兼容性（NOT NULL 无默认
  不写列 → 必失败）。
- `StagingSliceGuard`：staging 内不满足 slice 等值的行数 > 0 → 红（NULL 视为
  越界）；`StagingKeyGuard`：key 列存在、非空、distinct == rows；
  `UpsertKeyIndexGuard`：目标 key 无 unique/PK 索引 → 红（并发 INSERT 竞态
  前置消灭）。

## 3. 目标与非目标

**目标**

1. 回写吞吐对齐物化快路径量级（预期 ≥40 万行/s，10M 行回写 ≤1 分钟量级；
   实证数字以 validation.md 为准，本文不预支）。
2. 部分回写三层次齐备：读取过滤（`--where`）、slice 幂等重跑、upsert 键控合并。
3. 触碰目标前的守卫族，失败留可执行处置文案。
4. 耗时/行数/模式/删除行数经 `TraceSink` 埋点。

**非目标（本期明确不做）**

- 列映射哑翻译层（rename/exclude/constants）——`--cols` 语义不变；
- 多表回写事务单元与 compensation；
- 与 `materialize` 的并行 jobs 通道整合（回写保持单连接单事务；并行化涉及
  分片语义，单独评审）；
- GaussDB 侧 `ON CONFLICT`（A 模式无此语法，不引入方言分支）。

## 4. 设计

### 4.1 性能：CSV 文件中转快路径（默认，所有模式共用）

```
本地 DuckDB                        GaussDB
──────────                        ────────
COPY (SELECT {cols} FROM {t} [WHERE {w}])
  TO '{run}/.spinduck-sync-{t}-{pid}.csv'
  (FORMAT CSV, HEADER false, DELIMITER ',')
        │ 内核导出，零逐行客户端编码
        ▼
   临时 CSV 文件（成功即删，失败保留供诊断）
        │ client.copy_in 流式读取
        ▼
COPY {t} ({cols}) FROM STDIN (FORMAT csv)      ← 单事务
```

- **NULL/空串保真（本方向的 D14 对偶）**：DuckDB `COPY TO` CSV 缺省
  NULL → 不带引号空字段、空串 → `""`；GaussDB/PG `COPY FROM STDIN (FORMAT csv)`
  缺省不带引号空字段 → NULL、`""` → 空串。两侧缺省即无损对齐，
  **不引入任何显式 NULLSTR/QUOTE 覆盖**，避免物化方向 V27 踩过的塌缩坑。
- 列清单显式给出（`HEADER false`），列序由 `--cols` 声明序决定，两侧同列序。
- `--where` 保留为读取端过滤，拼接口径与 `materialize` 一致（调用方信任边界，
  文档标注；不额外转义）。
- 临时文件放项目 run 目录（§11 运行态布局），命名 `.spinduck-sync-{table}-{pid}.csv`，
  成功删除、失败保留并在错误消息带路径——与物化 §3.1 同一套纪律。
- 旧逐行路径保留为快路径失败时的回落（快路径失败原因并入最终错误消息，
  不静默），回落行为与 V26 完全一致。

### 4.2 `--mode slice`：部分回写 + 幂等重跑

```
同事务内：
1. DELETE FROM {t} WHERE {slice 谓词}
2. COPY {t} ({cols}) FROM STDIN (FORMAT csv)   ← 4.1 通道
3. commit（任一步失败整体回滚）
```

- CLI：`--mode slice --slice biz_date=20261111[,split_src=BUY]`。
  slice 谓词由结构化键值拼装（`col = 'val' AND …`，值单引号转义），
  **不接自由 SQL**——DELETE 是破坏性操作，声明式谓词缩小注入面。
- `rows=` 输出为 COPY 行数；DELETE 行数经 TraceSink 埋点（字段新增属破坏性
  变更，随实现 PR 评审）。
- 配套守卫：`StagingSliceGuard` 对偶——DuckDB staging 内存在不满足 slice 等值
  的行（NULL 同判越界）→ 红，防止漏删目标旧行却写入越界新行。

### 4.3 `--mode upsert`：键控部分合并

GaussDB A 模式无 `ON CONFLICT`；参照实现的逐行 UPDATE-then-INSERT 不移植，
改用 **staging 表 join 式**（集合操作，行数无关的语句数）：

```
同事务内：
1. CREATE TEMP TABLE sd_sync_tmp AS SELECT * FROM {t} LIMIT 0   ← 复用目标 schema
2. COPY sd_sync_tmp ({cols}) FROM STDIN (FORMAT csv)             ← 4.1 通道
3. UPDATE {t} SET {非 key 列 = tmp.列, …} FROM sd_sync_tmp tmp
     WHERE {t}.{k} = tmp.{k} AND …                               ← 键控更新
4. INSERT INTO {t} ({cols}) SELECT {cols} FROM sd_sync_tmp tmp
     WHERE NOT EXISTS (SELECT 1 FROM {t} x WHERE x.{k} = tmp.{k} AND …)
5. DROP TABLE sd_sync_tmp; commit
```

- CLI：`--mode upsert --key a,b`（key 必须是 `--cols` 的**真子集**——全键列无更新目标，直接红）。
- 无 unique 索引时步骤 3/4 在并发下可能双写；**fail-closed 前置**（§5.4），
  而不是运行时竞态重试。
- 每语句影响行数交叉校验（updated + inserted == staging 行数），不一致即红。

### 4.4 `--where`（读取过滤，append 语义）

- 独立可用：`sync --db … --table t --cols a,b --where "…"`，仅过滤读取端，
  目标侧仍为 append。
- usage 文本与输出显性标注 append 语义（§1.2 原则 2），防"增量同步"误用。

## 5. 前置守卫族（全部在触碰目标数据之前执行）

| 守卫 | 触发条件（→ 红） | 适用模式 | 错误码建议 |
|---|---|---|---|
| G1 目标 schema 漂移 | `--cols` 中存在目标表没有的列；或目标 NOT NULL 无默认列不在写入列集 | 全部 | ENV 段（连接目标元数据可观测的环境/结构失配） |
| G2 slice 一致性 | staging 存在不满足 `--slice` 等值的行（NULL 同判越界），报出行数 | slice | BIZ 段（业务数据不满足回写契约） |
| G3 key 完整性 | key 列不在 `--cols`；staging key 含 NULL；staging key 不唯一（报 rows 与 distinct 差值） | upsert | BIZ 段 |
| G4 目标 key 唯一索引 | 目标表 key 列集无覆盖它的 unique/PK 索引（查 `pg_index`/`pg_constraint`） | upsert | ENV 段（目标库结构前置未就绪，口径同 ENV-0015 的"先 deploy"提示风格） |

- 实现位置：守卫只经 `gauss.rs` 通道查目标元数据、经 `SqlExecutor` 查 staging，
  DuckDB API 不出 `sql/exec.rs`（§1 接缝纪律）。
- 错误码为规则数据：**码值与文案入错误码表并挂探针测试**（AGENTS §5/§6），
  具体编号在实现 PR 定稿；分段归属以上为建议。

## 6. CLI 契约

```
sync --db <path> --table <name> --cols a,b[,c…]
     [--where <sql>]                             # 读取过滤（append 语义）
     [--mode append|slice|upsert]                # 默认 append（现状契约）
     [--slice k=v[,k=v…]]                        # mode=slice 必填
     [--key a,b]                                 # mode=upsert 必填
```

- 参数互斥/依赖在 `parse_sync` 显式红（`--mode slice` 无 `--slice`、
  `--mode upsert` 无 `--key`、`--slice/--key` 与 `--mode` 不匹配等），
  usage 错误文案同步更新（usage 契约测试面）。
- stdout 契约：默认（无新参）逐字不变；带模式时
  `SYNC {t} mode={m} rows={n}`，append 且无 `--where` 时保持旧形态
  `SYNC {t} rows={n}`。
- 退出码契约不变（0 成功 / 1 失败）。

## 7. TDD 落地计划（每 PR 一组循环，顺序执行）

| PR | 行为（示例测试名） | 说明 |
|---|---|---|
| S0 守护 + 快路径 | `should_sync_full_table_via_copy_stream_and_keep_append_semantics`、`should_fall_back_to_row_path_when_copy_stream_fails` | 先守护测试锁定现行为（FakeExecutor 断言收到的 SELECT 无 WHERE）；再实现 4.1；实证数字入 validation.md |
| S1 读取过滤 | `should_filter_source_rows_by_where_clause` | `--where`，append 语义 |
| S2 slice | `should_delete_slice_then_copy_in_one_transaction`、`should_err_when_staging_rows_fall_outside_slice`（G2）、`should_require_slice_predicate_for_slice_mode` | 4.2 + G2 + CLI 互斥 |
| S3 upsert | `should_merge_by_key_via_staging_table`、`should_err_when_staging_key_not_unique`（G3）、`should_err_when_target_key_lacks_unique_index`（G4）、`should_reconcile_statement_row_counts` | 4.3 + G3/G4 + 行数交叉校验 |
| S4 观测 | 耗时/删除行数/模式 TraceSink 字段 | 字段新增属破坏性变更，随本 PR 评审 |

边界纪律：S0 的守护期望值在后续 PR **永不变更**（AGENTS §4.3）；新增错误码
探针与码表同 PR；快路径/回落路径的分叉只在 `gauss.rs` 内部，`sync_cmd`
（`cli/run.rs`）只消费结果。

## 8. 展望（本期不做，登记待评审）

- **列映射哑翻译层**（fastaas `ColumnResolver`）：rename/exclude/constants，
  配 `deploy` 计划的 binding 声明落地，属规则数据面扩展。
- **多表回写单元**：多目标共享单事务或失败补偿（fastaas `TargetCommitUnit`）；
  依赖 CLI 多表语法与补偿语义评审。
- **回写并行**：表级 jobs（对齐 `materialize_parallel`）或单表分片；需先定义
  分片键与 slice 语义的交互。

## 9. 评审遗留登记（2026-11-11 评审，#23）

已修（必修 1–3 + 建议 4–6）：`--where` 执行层消费（`mode_select_sql` 唯一消费点
+ 回落路径同口径）；`${ENV}` 展开 off-by-one；BIZ-0102 域标签与用法错域统一
（ENG-0010）；`WritePlan` 纯函数测试三态；G1 schema 限定名/跨 schema 过滤、
G4 `conkey` 经 `string_to_array::int[]` 可移植 unnest；快路径本地行数核对 +
upsert `UPDATE+INSERT == staging` 交叉校验（DROP 移出计数）。

登记待后续（不阻断合入）：

1. G1/G4 元数据 SQL 未对真实 openGauss 执行过——E2E 守护补齐前，G4 若因
   `string_to_array` 口径不支持而恒红，属 fail-closed 方向，修 SQL 即可。
2. 回滚失败（`let _ = self.exec("ROLLBACK")`）吞错后仍继续回落——应终止回落。
3. 回落与快路径读取同一张本地表，两次尝试可能读到不同快照——可复用快路径
   保留的 CSV 作为回落数据源。
4. `RunCtx { workdir, fast }` 两字段同义——改 `enum Channel { Fast(PathBuf), Rows }`。
5. `--slice` 语法预检在 `args.rs` 与 `parse_slice` 两处（同形真值来源）；slice
   谓词**列名**裸拼进 DELETE，建议加标识符白名单（破坏性语句，成本极低）。
6. 临时 CSV 命名用 mode 标签而非设计 §4.1 的 `{table}`；失败保留的 CSV（含
   业务数据）落在库文件同目录——runbook 需写清理口径。
7. 规模：`args.rs`（1464 行）/ `run.rs`（846 行）继续超 §3 上限（本 PR
   sync.rs / sync_tx.rs 合规），按 §3 纪律评审已指出，拆分另行 PR。
