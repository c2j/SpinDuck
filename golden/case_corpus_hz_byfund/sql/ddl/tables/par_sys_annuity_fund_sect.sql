CREATE TABLE par_sys_annuity_fund_sect (
    section_num numeric(4,0) NOT NULL,
    seq_no numeric(2,0) NOT NULL,
    start_date character varying(8),
    end_date character varying(8),
    act_cqdate character varying(8),
    pmll numeric(18,15),
    operator character varying(20),
    check_user character varying(20),
    check_state character varying(1),
    check_action character varying(1),
    check_time character varying(20),
    security_id character varying(100) NOT NULL
);
