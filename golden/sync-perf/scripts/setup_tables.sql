-- sync 回写验证：目标侧表结构（幂等，可重跑）
-- 用法：./gsql.sh < setup_tables.sql
DROP TABLE IF EXISTS sd_t_pk, sd_t_uq, sd_t_nopk;
DROP TABLE IF EXISTS sd_perf_pk, sd_perf_nopk, sd_perf_pk5;

-- 正确性矩阵：PK / 裸唯一索引 / 无约束 三种目标形态
CREATE TABLE sd_t_pk(id int PRIMARY KEY, v int, w text);
CREATE TABLE sd_t_uq(id int, v int, w text);
CREATE UNIQUE INDEX sd_t_uq_id_key ON sd_t_uq(id);
CREATE TABLE sd_t_nopk(id int, v int, w text);

-- 性能矩阵：百万级目标（数据由 seed_perf.sql 灌入）
CREATE TABLE sd_perf_pk(id int PRIMARY KEY, v int, w text);
CREATE TABLE sd_perf_nopk(id int, v int, w text);
CREATE TABLE sd_perf_pk5(id int PRIMARY KEY, v int, w text);
