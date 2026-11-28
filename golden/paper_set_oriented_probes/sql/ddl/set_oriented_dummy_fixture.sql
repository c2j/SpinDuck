-- 仓库夹具表——逐字取自 spinduck/tests/set_oriented_dummy.rs 的 BASE_DDL。
-- 用途：把该文件里的 caller_dummy（byfund c_get_data 段的同型夹具）原样移植进来，
-- 作为「已知可转换的复杂形状」参照（探针 T2.f）。
-- 这里的 VARCHAR / DECIMAL 不带长度，与夹具一致；sys_dummy 由引擎内建，不在此建表。
DROP TABLE IF EXISTS dst_rows;
DROP TABLE IF EXISTS cur_rows;
DROP TABLE IF EXISTS seat_para;

CREATE TABLE seat_para(security_id VARCHAR, commision_type VARCHAR,
       return_rate_flag VARCHAR, return_rate_org DECIMAL(10,4), return_rate DECIMAL(10,4));
CREATE TABLE cur_rows(security_id VARCHAR, cjje DECIMAL(15,2),
       yj11 DECIMAL(15,2), yj12 DECIMAL(15,2), tag VARCHAR);
CREATE TABLE dst_rows(security_id VARCHAR, yj DECIMAL(20,6), tag VARCHAR);
