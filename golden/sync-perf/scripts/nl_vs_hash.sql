-- 计划器对照：默认计划 vs 强制 Nested Loop（PK 索引点查）
-- 用法：./gsql.sh < nl_vs_hash.sql   （需先有 sd_perf_pk5 与临时表数据）
\timing on
CREATE TEMP TABLE sd_sync_tmp AS SELECT id, v, w FROM sd_perf_pk5 LIMIT 0;
INSERT INTO sd_sync_tmp SELECT g, g, 'm' FROM generate_series(1,100000) g;
INSERT INTO sd_sync_tmp SELECT 20000000+g, g, 'i' FROM generate_series(1,100000) g;

-- 默认计划（Hash/Merge Join，全表扫描目标）
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp tmp WHERE tmp.id = tgt.id;
INSERT INTO sd_perf_pk5 (id,v,w) SELECT tmp.id, tmp.v, tmp.w FROM sd_sync_tmp tmp
  WHERE NOT EXISTS (SELECT 1 FROM sd_perf_pk5 x WHERE x.id = tmp.id);
DELETE FROM sd_perf_pk5 WHERE id >= 20000000;

-- 强制 Nested Loop（索引点查，与目标规模无关）
SET enable_hashjoin = off;
SET enable_mergejoin = off;
UPDATE sd_perf_pk5 AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp tmp WHERE tmp.id = tgt.id;
INSERT INTO sd_perf_pk5 (id,v,w) SELECT tmp.id, tmp.v, tmp.w FROM sd_sync_tmp tmp
  WHERE NOT EXISTS (SELECT 1 FROM sd_perf_pk5 x WHERE x.id = tmp.id);
DELETE FROM sd_perf_pk5 WHERE id >= 20000000;
