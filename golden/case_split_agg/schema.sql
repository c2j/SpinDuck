-- 黄金用例：清算拆分聚合（表结构取自考题基线的最小可用子集，字段类型保持一致）
DROP TABLE IF EXISTS dat_inst_fund_cjqs;
DROP TABLE IF EXISTS mid_yjqs_detail;

CREATE TABLE mid_yjqs_detail (
    bcrq        VARCHAR(8) NOT NULL,
    fund_code   VARCHAR(10) NOT NULL,
    scdm        VARCHAR(10) NOT NULL,
    cjsl        NUMERIC(19,4) NOT NULL,
    cjjg        NUMERIC(19,6) NOT NULL,
    cjbz        VARCHAR(1)
);

CREATE TABLE dat_inst_fund_cjqs (
    bcrq        VARCHAR(8) NOT NULL,
    fund_code   VARCHAR(10) NOT NULL,
    scdm        VARCHAR(10) NOT NULL,
    cjsl        NUMERIC(19,4) NOT NULL,
    cjje        NUMERIC(19,2) NOT NULL,
    fee         NUMERIC(19,2) NOT NULL
);
