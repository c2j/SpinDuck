CREATE TABLE par_fund_seat (
    seat_id character varying(6) NOT NULL,
    fund_code character varying(8) NOT NULL,
    father_seat character varying(6),
    seat_code character varying(7) NOT NULL,
    market_code character varying(3) NOT NULL,
    dealer_code character varying(10),
    index_code character varying(6),
    seat_attribute character varying(1),
    operator character varying(10),
    check_user character varying(10),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8) DEFAULT '99991231'::character varying,
    valid_flag character varying(1),
    member_code character varying(6),
    relation_side character varying(1) DEFAULT '0'::character varying,
    check_time timestamp(6) without time zone,
    oper_time timestamp(6) without time zone
);
