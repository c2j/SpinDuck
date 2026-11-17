-- upsert → 5M 目标（sd_perf_pk5）staging：20 万行，50% 命中
-- 用法：spinduck query --db perf.duckdb --sql-file local_seed_upsert_5m.sql
CREATE OR REPLACE TABLE sd_perf_pk5 AS
SELECT g AS id, g AS v, 'm' AS w FROM range(1,100001) t(g)
UNION ALL
SELECT 20000000+g AS id, g AS v, 'i' AS w FROM range(1,100001) t(g);
