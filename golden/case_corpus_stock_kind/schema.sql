-- 黄金用例（语料级）：表 DDL 逐字取自考题基线 1_表结构（函数 %TYPE 引用所需）
DROP TABLE IF EXISTS dic_stock_kind_sub;
DROP TABLE IF EXISTS par_sys_stock;
DROP TABLE IF EXISTS par_sys_securities;

CREATE TABLE dic_stock_kind_sub (
    market_code character varying(3) NOT NULL,
    stock_kind character varying(4) NOT NULL,
    begin_code character varying(12) NOT NULL,
    end_code character varying(12) NOT NULL,
    descript character varying(40),
    trade_unit numeric(5,0) DEFAULT 1,
    operator character varying(10),
    check_user character varying(10),
    check_state character varying(1) DEFAULT '1'::character varying,
    check_action character varying(1),
    check_time character varying(20),
    settle_type character varying(2)
);

CREATE TABLE par_sys_stock (
    security_id character varying(19) NOT NULL,
    stock_kind character varying(4),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8)
);

CREATE TABLE par_sys_securities (
    security_id character varying(19) NOT NULL,
    stock_kind character varying(4)
);
