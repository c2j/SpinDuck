-- 桩：股东代码 → 基金代码（经 par_fund_partner 映射；查无 → 'FUND_ZZZ'）
CREATE OR REPLACE FUNCTION fnc_com_get_fund_code(p_gddm VARCHAR2, p_sub_gddm VARCHAR2,
                                                 p_i_date VARCHAR2, p_xwdm VARCHAR2)
RETURN VARCHAR2 IS
  v_fund_code par_fund_partner.fund_code%TYPE;
BEGIN
  SELECT t.fund_code
    INTO v_fund_code
    FROM par_fund_partner t
   WHERE t.partner_code = p_gddm
     AND t.market_code = '001'
     AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  RETURN v_fund_code;
EXCEPTION
  WHEN OTHERS THEN
    RETURN 'FUND_ZZZ';
END;
