-- 正确性矩阵小样例 staging（目标表见 setup_tables.sql）
-- 用法：spinduck query --db perf.duckdb --sql-file local_seed_matrix.sql
CREATE OR REPLACE TABLE sd_t_pk AS
SELECT i AS id, 999 AS v, 'new' AS w FROM range(1,6) t(i)
UNION ALL SELECT i, i, 'keep' FROM range(11,16) t(i);
