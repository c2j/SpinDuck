-- 桩：费率信息（bond_kind '0201' → '0' 走 accrual2，其余 → '1' 走 accrual）
-- 结构对齐可折叠形态（赋值 + IF-THEN 单赋值），使摘要走 PureExpr 内联。
CREATE OR REPLACE FUNCTION fnc_get_rate_info2(p_fund_code VARCHAR2, p_i_date VARCHAR2,
                                              p_i_type VARCHAR2, p_bond_kind VARCHAR2)
RETURN VARCHAR2 IS
  v_rate_flag VARCHAR2;
BEGIN
  v_rate_flag := '1';
  IF p_bond_kind = '0201' THEN
    v_rate_flag := '0';
  END IF;
  RETURN v_rate_flag;
END;
