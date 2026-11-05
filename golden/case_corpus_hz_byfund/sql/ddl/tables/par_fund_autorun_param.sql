CREATE TABLE par_fund_autorun_param (
    fund_code character varying(8) NOT NULL,
    is_autorun character varying(1) NOT NULL,
    is_do_dayend character varying(1) NOT NULL,
    is_do_checkdata character varying(1) NOT NULL,
    custom_run_time character varying(2),
    custom_run_time2 character varying(2),
    custom_run_time3 character varying(2),
    operator character varying(10),
    inure_begin_date character varying(8) NOT NULL,
    inure_end_date character varying(8) DEFAULT '99991231'::character varying,
    is_do_monitor character varying(1) DEFAULT '1'::character varying NOT NULL,
    is_monitor character varying(1) DEFAULT '1'::character varying,
    estimate_days character varying(2) DEFAULT '11'::character varying
);
