-- 桩：席位费型（动态 SQL 内 venture_type，恒 '1'）
CREATE OR REPLACE FUNCTION fnc_com_get_seat_fee_type(p_bcrq VARCHAR2, p_scdm VARCHAR2,
                                                     p_fund_code VARCHAR2, p_xwdm VARCHAR2,
                                                     p_stock_kind VARCHAR2)
RETURN VARCHAR2 IS
BEGIN
  RETURN '1';
END;
