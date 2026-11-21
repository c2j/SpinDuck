-- M0 剖面验证桩：正文逐字取自 docs/spinduck-set-oriented-v3-design.md 附录 A。
CREATE OR REPLACE FUNCTION fnc_get_fund_data_mode(
  p_bcrq VARCHAR2, p_gddm VARCHAR2, p_sub_gddm VARCHAR2, p_xwdm VARCHAR2
) RETURN VARCHAR2 IS
BEGIN
  RETURN '1';
END;
