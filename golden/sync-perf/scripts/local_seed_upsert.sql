-- upsert f=50% staging：50 万命中（id 1..500000）+ 50 万新增（id 4000001..4500000）
-- 用法：spinduck query --db perf.duckdb --sql-file local_seed_upsert.sql
CREATE OR REPLACE TABLE sd_perf_pk AS
SELECT g AS id, g AS v, 'm' AS w FROM range(1,500001) t(g)
UNION ALL
SELECT 4000000+g AS id, g AS v, 'i' AS w FROM range(1,500001) t(g);
