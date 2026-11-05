CREATE TABLE par_sys_purchase (
    security_id character varying(19) NOT NULL,
    purchase_days numeric(3,0),
    purchase_kind character varying(4),
    coin_code character varying(3),
    operator character varying(10),
    check_user character varying(10),
    inure_begin_date character varying(8) DEFAULT '19000101'::character varying,
    inure_end_date character varying(8) DEFAULT '99991231'::character varying,
    check_state character varying(1) DEFAULT '1'::character varying,
    check_action character varying(1),
    check_time character varying(20),
    if_capital character varying(1) DEFAULT '1'::character varying NOT NULL
);
