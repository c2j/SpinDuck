CREATE TABLE par_fund_rate_info (
    fund_code character varying(8) NOT NULL,
    oper_type character varying(2) NOT NULL,
    rate_flag character varying(1),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8) DEFAULT '99991231'::character varying,
    operator character varying(10),
    stock_kind character varying(4) DEFAULT '0200'::character varying NOT NULL,
    tax_rate numeric(5,4) DEFAULT 0,
    market_code character varying(3) DEFAULT '000'::character varying NOT NULL
);
