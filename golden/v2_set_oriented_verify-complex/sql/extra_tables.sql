-- hz_byfund 依赖的补充表（语料 tables.sql 未含；列取自过程体实际引用，确定性最小集）
CREATE TABLE par_oper_venture (
    stock_kind_str character varying(200),
    operation_kind character varying(5) NOT NULL,
    market_code character varying(3) NOT NULL
);

CREATE TABLE dat_fund_xw_venture (
    fund_code character varying(8) NOT NULL,
    xwdm character varying(6) NOT NULL,
    bcrq character varying(8) NOT NULL,
    qsje numeric(20,8),
    scdm character varying(3) NOT NULL,
    operation_kind character varying(5) NOT NULL,
    trade_type character varying(2),
    venture_type character varying(2),
    qsje2 numeric(20,8)
);

CREATE TABLE dic_stock_kind_sub (
    market_code character varying(3) NOT NULL,
    begin_code character varying(12) NOT NULL,
    end_code character varying(12) NOT NULL,
    stock_kind character varying(4) NOT NULL
);

CREATE TABLE jyfy_temp (
    zqdm character varying(12),
    gddm character varying(16),
    data_date character varying(8),
    bs character varying(2),
    jydy character varying(6),
    jyfy numeric(20,8)
);

-- 脱敏基线列宽修正：commision_type 实际取 '11'..'36' 两字符费型码，
-- varchar(1) 无法存储（同 seed.sql 头注的 commision_fee_type 同类问题）。
ALTER TABLE par_fund_seat_para ALTER COLUMN commision_type TYPE character varying(2);
