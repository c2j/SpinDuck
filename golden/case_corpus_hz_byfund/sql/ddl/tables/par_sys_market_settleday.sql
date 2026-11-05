CREATE TABLE par_sys_market_settleday (
    market_code character varying(3) NOT NULL,
    settle_day character varying(1) DEFAULT '0'::character varying,
    operator character varying(10),
    check_user character varying(10),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8),
    check_state character varying(1) DEFAULT '1'::character varying,
    check_action character varying(1),
    check_time character varying(20)
);
