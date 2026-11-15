-- SpinDuck 侧 purchase 同形表（集合化可转换：裸 VARCHAR + BIGINT，全可空，无约束）。
-- 列形取自 mid_yjqs_detail 的关键列；类型映射到集合化 DDL 白名单（VARCHAR 不带长度，
-- 带长度会使 schema.rs 的 DDL 解析失败 → UnknownObjectDependency）。
-- 两条语句须分开经 `query` 执行（query 一次只跑一条）。
CREATE TABLE mid_yjqs_detail_so(gddm VARCHAR, xwdm VARCHAR, scdm VARCHAR, zqdm VARCHAR, security_id VARCHAR, bs VARCHAR, trade_type VARCHAR, bcrq VARCHAR, cjsl BIGINT);
CREATE TABLE dat_fund_cjqs_so(gddm VARCHAR, xwdm VARCHAR, scdm VARCHAR, zqdm VARCHAR, security_id VARCHAR, bs VARCHAR, trade_type VARCHAR, bcrq VARCHAR, cjsl BIGINT);