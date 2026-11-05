CREATE TABLE par_fund_seat_para (
    fund_code character varying(8) NOT NULL,
    market_code character varying(3) NOT NULL,
    seat_code character varying(6) NOT NULL,
    stock_kind character varying(4) NOT NULL,
    commision_type character varying(1),
    commision_fee_type character varying(1),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8),
    return_rate_flag character varying(1) DEFAULT '0'::character varying,
    return_rate_org numeric(20,10) DEFAULT 0,
    return_rate numeric(20,10) DEFAULT 0,
    commision_value_type character varying(1) DEFAULT '1'::character varying,
    ggt_mid_cal_meth character varying(2) DEFAULT '1'::character varying,
    ggt_mid_tail_diff character varying(1) DEFAULT '0'::character varying
);
