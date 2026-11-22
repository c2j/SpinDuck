-- 桩：证券类型（QFII 段使用；恒 '0501'）
CREATE OR REPLACE FUNCTION fnc_get_stock_kind(p_zqdm VARCHAR2, p_i_date VARCHAR2,
                                              p_scdm VARCHAR2)
RETURN VARCHAR2 IS
BEGIN
  RETURN '0501';
END;
