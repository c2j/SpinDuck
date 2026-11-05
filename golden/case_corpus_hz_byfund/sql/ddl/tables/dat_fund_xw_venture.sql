CREATE TABLE dat_fund_xw_venture (
    fund_code character varying(8) NOT NULL,
    xwdm character varying(6) NOT NULL,
    bcrq character varying(8) NOT NULL,
    qsje numeric(15,3),
    scdm character varying(3) NOT NULL,
    operation_kind character varying(5) NOT NULL,
    trade_type character varying(1) DEFAULT '0'::character varying NOT NULL,
    venture_type character varying(1),
    qsje2 numeric(15,3)
);
