CREATE TABLE par_sys_bond_section (
    security_id character varying(19) NOT NULL,
    section_num numeric(4,0) NOT NULL,
    seq_no numeric(2,0) DEFAULT 1 NOT NULL,
    start_date character varying(8),
    end_date character varying(8),
    act_cqdate character varying(8),
    pmll numeric(18,15),
    pre_pmll numeric(18,15),
    operator character varying(10),
    check_user character varying(10),
    factor numeric(18,15) DEFAULT 1,
    jcll numeric(18,15) DEFAULT 0,
    check_state character varying(1) DEFAULT '1'::character varying,
    check_action character varying(1),
    check_time character varying(20)
);
