-- 桩：费率信息（bond_kind '0201' → '0' 走 accrual2，其余 → '1' 走 accrual）
CREATE OR REPLACE FUNCTION fnc_get_rate_info2(p_fund_code VARCHAR2, p_i_date VARCHAR2,
                                              p_i_type VARCHAR2, p_bond_kind VARCHAR2)
RETURN VARCHAR2 IS
BEGIN
  IF p_bond_kind = '0201' THEN
    RETURN '0';
  END IF;
  RETURN '1';
END;
