CREATE TABLE db_log (
    id character varying(20),
    proc_name character varying(100),
    info character varying(4000),
    log_level character varying(10),
    time_stamp character varying(23),
    call_stack character varying(4000),
    err_stack character varying(4000),
    step_no character varying(20),
    sql_txt character varying(4000),
    sql_param character varying(4000),
    log_date character varying(8)
);
