-- 计划器对照：默认计划 vs 强制 Nested Loop（PK 索引点查）
-- 用法：./gsql.sh < nl_vs_hash.sql   （需先有 sd_perf_pk5）
\timing on
CREATE TEMP TABLE sd_sync_tmp AS SELECT id, v, w FROM sd_perf_pk5 LIMIT 0;
INSERT INTO sd_sync_tmp SELECT g, g, 'm' FROM generate_series(1,100000) g;
INSERT INTO sd_sync_tmp SELECT 20000000+g, g, 'i' FROM generate_series(1,100000) g;

-- A. 默认计划（Hash/Merge Join，全表扫描目标）
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp tmp WHERE tmp.id = tgt.id;
INSERT INTO sd_perf_pk5 (id,v,w) SELECT tmp.id, tmp.v, tmp.w FROM sd_sync_tmp tmp
  WHERE NOT EXISTS (SELECT 1 FROM sd_perf_pk5 x WHERE x.id = tmp.id);
DELETE FROM sd_perf_pk5 WHERE id >= 20000000;

-- B. 强制 Nested Loop（索引点查，与目标规模无关）
SET enable_hashjoin = off;
SET enable_mergejoin = off;
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp tmp WHERE tmp.id = tgt.id;
INSERT INTO sd_perf_pk5 (id,v,w) SELECT tmp.id, tmp.v, tmp.w FROM sd_sync_tmp tmp
  WHERE NOT EXISTS (SELECT 1 FROM sd_perf_pk5 x WHERE x.id = tmp.id);
DELETE FROM sd_perf_pk5 WHERE id >= 20000000;
RESET enable_hashjoin;
RESET enable_mergejoin;

-- C. ANALYZE 临时表后统计信息是否改变计划器选择（10万 staging 区间）
ANALYZE sd_sync_tmp;
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp tmp WHERE tmp.id = tgt.id;
INSERT INTO sd_perf_pk5 (id,v,w) SELECT tmp.id, tmp.v, tmp.w FROM sd_sync_tmp tmp
  WHERE NOT EXISTS (SELECT 1 FROM sd_perf_pk5 x WHERE x.id = tmp.id);
DELETE FROM sd_perf_pk5 WHERE id >= 20000000;

-- D. 低真实变更率对照：10 万 staging 中仅 1000 行真变更，
--    IS DISTINCT FROM 命中少量行；与无条件 UPDATE 成对比较同计划耗时。
--    两条 UPDATE 必须面对同一份更新前数据：先跑无条件 UPDATE，再从
--    sd_d_base 快照恢复这 10 万行，然后跑条件 UPDATE。
--    注意：A/B/C 的无条件 UPDATE 已把命中行写成与 sd_sync_tmp 相同的值，
--    直接复用会让谓词命中 0 行（空更新），故另建 tmp_same 基线。
CREATE TEMP TABLE sd_sync_tmp_same AS
SELECT id, v, CASE WHEN id <= 1000 THEN 'chg' ELSE w END AS w
FROM sd_perf_pk5 WHERE id <= 100000;
CREATE TEMP TABLE sd_d_base AS
SELECT id, v, w FROM sd_perf_pk5 WHERE id <= 100000;

-- D1. 无条件 UPDATE 对照（全部 10 万行走写路径）
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp_same tmp
  WHERE tmp.id = tgt.id;

-- 恢复 D 前快照（让 D2 面对与 D1 相同的更新前数据；此条本身计时仅供参考）
UPDATE sd_perf_pk5 AS tgt SET v = b.v, w = b.w FROM sd_d_base b
  WHERE b.id = tgt.id;

-- D2. 条件 UPDATE（仅 1000 个真实变更行落写）
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp_same tmp
  WHERE tmp.id = tgt.id
    AND (tgt.v IS DISTINCT FROM tmp.v OR tgt.w IS DISTINCT FROM tmp.w);

DELETE FROM sd_perf_pk5 WHERE id >= 20000000;
