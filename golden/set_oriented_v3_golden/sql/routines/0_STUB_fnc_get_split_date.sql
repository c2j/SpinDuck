-- 桩：分户切换日期（'qfii_dealer_date' → '20250101'，早于业务日 → QFII 段开启）
CREATE OR REPLACE FUNCTION fnc_get_split_date(p_key VARCHAR2)
RETURN VARCHAR2 IS
BEGIN
  RETURN '20250101';
END;
