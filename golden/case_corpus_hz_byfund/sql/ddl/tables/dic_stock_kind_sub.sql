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
