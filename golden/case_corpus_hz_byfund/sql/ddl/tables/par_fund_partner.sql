CREATE TABLE par_fund_partner (
    fund_code character varying(8) NOT NULL,
    partner_code character varying(20) NOT NULL,
    index_code character varying(12),
    market_code character varying(3) NOT NULL,
    sub_partner_code character varying(12) DEFAULT '000000'::character varying NOT NULL,
    operator character varying(10),
    check_user character varying(10),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8),
    accnt_type character varying(2) DEFAULT 1 NOT NULL,
    check_time timestamp(6) without time zone,
    oper_time timestamp(6) without time zone,
    one_yard_no character varying(12),
    one_yard_name character varying(200),
    stock_change_date character varying(8),
    stock_change_mode character varying(1),
    stock_change_remark character varying(200)
);
