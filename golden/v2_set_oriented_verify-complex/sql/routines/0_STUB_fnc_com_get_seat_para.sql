-- 桩：席位参数（commision_type + OUT 回写 return_rate 三元组）
CREATE OR REPLACE FUNCTION fnc_com_get_seat_para(p_i_date VARCHAR2, p_i_scdm VARCHAR2,
                                                 p_i_fund_code VARCHAR2, p_i_xwdm VARCHAR2,
                                                 p_i_stock_kind VARCHAR2,
                                                 p_o_flag OUT VARCHAR2,
                                                 p_o_org OUT NUMBER,
                                                 p_o_rate OUT NUMBER)
RETURN VARCHAR2 IS
  v_type  par_fund_seat_para.commision_type%TYPE;
  v_flag  par_fund_seat_para.return_rate_flag%TYPE;
  v_org   par_fund_seat_para.return_rate_org%TYPE;
  v_rate  par_fund_seat_para.return_rate%TYPE;
BEGIN
  SELECT t.commision_type, t.return_rate_flag, t.return_rate_org, t.return_rate
    INTO v_type, v_flag, v_org, v_rate
    FROM par_fund_seat_para t
   WHERE t.fund_code = p_i_fund_code
     AND t.market_code = p_i_scdm
     AND t.seat_code = p_i_xwdm
     AND t.stock_kind = p_i_stock_kind
     AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  p_o_flag  := v_flag;
  p_o_org   := v_org;
  p_o_rate  := v_rate;
  RETURN v_type;
EXCEPTION
  WHEN OTHERS THEN
    p_o_flag := '0';
    p_o_org  := 0;
    p_o_rate := 0;
    RETURN '00';
END;
