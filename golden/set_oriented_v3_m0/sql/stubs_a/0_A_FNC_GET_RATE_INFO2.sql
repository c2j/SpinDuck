-- M0 剖面验证桩：正文逐字取自 docs/spinduck-set-oriented-v3-design.md 附录 A。
CREATE OR REPLACE FUNCTION fnc_get_rate_info2(
  p_fund_code VARCHAR2, p_i_date VARCHAR2, p_i_type VARCHAR2, p_bond_kind VARCHAR2
) RETURN VARCHAR2 IS
BEGIN
  IF p_bond_kind = '0201' THEN RETURN '0'; END IF;
  RETURN '1';
END;
