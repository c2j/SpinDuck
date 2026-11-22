-- 桩：席位参数（commision_type + OUT 回写 return_rate 三元组）
-- 结构对齐真实现（直接 INTO OUT 形参 + 嵌套异常块 + 单一 RETURN），
-- 使摘要走 PureExpr 内联查找路径而非 LookupTuple。
CREATE OR REPLACE FUNCTION fnc_com_get_seat_para(p_i_date VARCHAR2, p_i_scdm VARCHAR2,
                                                 p_i_fund_code VARCHAR2, p_i_xwdm VARCHAR2,
                                                 p_i_stock_kind VARCHAR2,
                                                 p_o_flag OUT VARCHAR2,
                                                 p_o_org OUT NUMBER,
                                                 p_o_rate OUT NUMBER)
RETURN VARCHAR2 IS
  v_type  par_fund_seat_para.commision_type%TYPE;
BEGIN
  BEGIN
    SELECT t.commision_type, t.return_rate_flag, t.return_rate_org, t.return_rate
      INTO v_type, p_o_flag, p_o_org, p_o_rate
      FROM par_fund_seat_para t
     WHERE t.fund_code = p_i_fund_code
       AND t.market_code = p_i_scdm
       AND t.seat_code = p_i_xwdm
       AND t.stock_kind = p_i_stock_kind
       AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  EXCEPTION
    WHEN OTHERS THEN
      v_type   := '00';
      p_o_flag := '0';
      p_o_org  := 0;
      p_o_rate := 0;
  END;
  RETURN v_type;
END;
