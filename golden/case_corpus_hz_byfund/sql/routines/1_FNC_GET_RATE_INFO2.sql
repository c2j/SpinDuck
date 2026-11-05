CREATE OR REPLACE FUNCTION FNC_GET_RATE_INFO2 (p_i_fund_code  VARCHAR2,
                                              p_i_date       VARCHAR2,
                                              p_i_oper_type  VARCHAR2,
                                              p_i_stock_kind VARCHAR2 DEFAULT '0200')
  RETURN VARCHAR2 IS

  v_rate_flag      v_par_fund_rate_info.rate_flag%TYPE;
  v_all_stock_kind v_par_bond.bond_kind%TYPE:='0200';
BEGIN

  BEGIN
     select t.rate_flag
       INTO v_rate_flag
       from (SELECT nn.rate_flag, nn.stock_kind, rownum rn
               FROM (SELECT distinct t.rate_flag, t.stock_kind
                       FROM v_par_fund_rate_info t
                      WHERE t.fund_code = p_i_fund_code
                        AND t.stock_kind IN (v_all_stock_kind, p_i_stock_kind)
                        AND t.oper_type = p_i_oper_type
                        AND p_i_date BETWEEN t.inure_begin_date AND
                            t.inure_end_date
                      ORDER BY t.stock_kind DESC) nn) t
      WHERE t.rn = 1;
  EXCEPTION
    WHEN OTHERS THEN
      v_rate_flag := '1';
  END;
  RETURN v_rate_flag;
END ;
