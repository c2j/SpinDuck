CREATE OR REPLACE PROCEDURE prc_com_get_jxdays(
  p_accrual_type varchar2, p_duixi_mode varchar2, p_date varchar2,
  p_start_date varchar2, p_end_date varchar2, p_first_term_start varchar2,
  p_term_first_start varchar2, p_o_jxdays OUT number, p_o_actdays OUT number) AS
BEGIN
  p_o_jxdays := 1; p_o_actdays := 1;   -- 桩：计息天数简化
END;
