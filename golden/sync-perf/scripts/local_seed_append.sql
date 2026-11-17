-- append 基准 staging：100 万全新 id（目标 sd_perf_pk 已有 1M 行 id 1..1M）
-- 用法：spinduck query --db perf-append.duckdb --sql-file local_seed_append.sql
CREATE OR REPLACE TABLE sd_perf_pk AS
SELECT g AS id, g AS v, 'w'||g AS w FROM range(2000000,3000000) t(g);
