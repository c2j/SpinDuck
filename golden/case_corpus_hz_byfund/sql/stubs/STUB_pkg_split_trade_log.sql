CREATE OR REPLACE PACKAGE pkg_split_trade_log AS
  PROCEDURE inst_log(p_date varchar2, p_user varchar2, p_biz varchar2, p_msg varchar2,
                     p_code varchar2, p_split varchar2, p_flag varchar2, p_succeed IN OUT varchar2);
END pkg_split_trade_log;
/
CREATE OR REPLACE PACKAGE BODY pkg_split_trade_log AS
  PROCEDURE inst_log(p_date varchar2, p_user varchar2, p_biz varchar2, p_msg varchar2,
                     p_code varchar2, p_split varchar2, p_flag varchar2, p_succeed IN OUT varchar2) AS
  BEGIN NULL; END;
END pkg_split_trade_log;
