-- 桩：主席位代码（直接回传席位代码，确定性）
CREATE OR REPLACE FUNCTION fnc_com_get_main_xwdm(p_fund_code VARCHAR2, p_xwdm VARCHAR2,
                                                 p_i_date VARCHAR2)
RETURN VARCHAR2 IS
BEGIN
  RETURN p_xwdm;
END;
