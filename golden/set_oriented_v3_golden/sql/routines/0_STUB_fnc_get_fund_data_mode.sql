-- 桩：资金数据模式（恒 '1'，全部纳入汇总）
CREATE OR REPLACE FUNCTION fnc_get_fund_data_mode(p_bcrq VARCHAR2, p_gddm VARCHAR2,
                                                  p_sub_gddm VARCHAR2, p_xwdm VARCHAR2)
RETURN VARCHAR2 IS
BEGIN
  RETURN '1';
END;
