-- 本地 DuckDB staging 示例（经 `spinduck query --db <f>.duckdb --sql-file <本文件> --out /tmp/q.json` 执行）。
-- 同步要求本地表名 = 目标表名；--cols id,v,w 为三列表公共口径。

-- append 基准：100 万全新 id（目标 sd_perf_pk 已有 1M 行 id 1..1M）
CREATE OR REPLACE TABLE sd_perf_pk AS
SELECT g AS id, g AS v, 'w'||g AS w FROM range(2000000,3000000) t(g);

-- upsert f=50%：50 万命中（id 1..500000）+ 50 万新增（id 4000001..4500000）
CREATE OR REPLACE TABLE sd_perf_pk AS
SELECT g AS id, g AS v, 'm' AS w FROM range(1,500001) t(g)
UNION ALL
SELECT 4000000+g AS id, g AS v, 'i' AS w FROM range(1,500001) t(g);

-- upsert → 5M 目标（sd_perf_pk5）：20 万 staging，50% 命中
CREATE OR REPLACE TABLE sd_perf_pk5 AS
SELECT g AS id, g AS v, 'm' AS w FROM range(1,100001) t(g)
UNION ALL
SELECT 20000000+g AS id, g AS v, 'i' AS w FROM range(1,100001) t(g);

-- 正确性矩阵用的小样例（目标表见 setup_tables.sql）
CREATE OR REPLACE TABLE sd_t_pk AS
SELECT i AS id, 999 AS v, 'new' AS w FROM range(1,6) t(i)
UNION ALL SELECT i, i, 'keep' FROM range(11,16) t(i);
