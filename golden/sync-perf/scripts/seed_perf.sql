-- 性能矩阵目标数据：1M / 5M 行（幂等：先清再灌）
\timing on
TRUNCATE TABLE sd_perf_pk;
INSERT INTO sd_perf_pk SELECT g, g, 'w'||g FROM generate_series(1,1000000) g;

TRUNCATE TABLE sd_perf_pk5;
INSERT INTO sd_perf_pk5 SELECT g, g, 'w'||g FROM generate_series(1,5000000) g;
ANALYZE sd_perf_pk5;

SELECT (SELECT count(*) FROM sd_perf_pk) AS pk_rows,
       (SELECT count(*) FROM sd_perf_pk5) AS pk5_rows;
