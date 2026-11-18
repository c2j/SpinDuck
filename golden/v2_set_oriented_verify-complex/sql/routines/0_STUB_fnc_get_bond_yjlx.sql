-- 桩：债券应计利息（QFII 段使用；恒 0）
CREATE OR REPLACE FUNCTION fnc_get_bond_yjlx(p_security_id VARCHAR2, p_i_date VARCHAR2,
                                             p_num NUMBER, p_flag VARCHAR2)
RETURN NUMBER IS
BEGIN
  RETURN 0;
END;
