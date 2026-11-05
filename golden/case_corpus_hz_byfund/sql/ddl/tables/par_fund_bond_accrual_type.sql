CREATE TABLE par_fund_bond_accrual_type (
    fund_code character varying(10) NOT NULL,
    security_id character varying(19) NOT NULL,
    bond_accrual_type character varying(2) NOT NULL,
    pre_bond_accrual_type character varying(2) NOT NULL,
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8) NOT NULL,
    operator character varying(10),
    checker character varying(10),
    check_time character varying(20),
    check_action character varying(1),
    check_state character varying(1),
    tm character varying(17) DEFAULT to_char("sysdate"(), 'yyyymmdd hh24:mi:ss'::text)
);
