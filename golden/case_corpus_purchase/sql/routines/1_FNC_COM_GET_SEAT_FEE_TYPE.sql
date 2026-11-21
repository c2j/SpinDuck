CREATE OR REPLACE FUNCTION FNC_COM_GET_SEAT_FEE_TYPE (p_i_date        VARCHAR2,
                                                  p_i_market_code VARCHAR2,
                                                  p_i_fund_code   VARCHAR2,
                                                  p_i_seat_code   VARCHAR2,
                                                  p_i_stock_kind  VARCHAR2)
  RETURN VARCHAR IS

  v_commision_type        par_fund_seat_para.commision_type%type:='';
  v_commision_fee_type    par_fund_seat_para.commision_fee_type%type:='';
  v_all_stock_kind        par_sys_marketrate.stock_kind%TYPE:='';
  v_venture_type          varchar2(1):='';

BEGIN
  v_all_stock_kind := substr(p_i_stock_kind, 1, 2) || '00';

  BEGIN

    SELECT '1'
      INTO v_venture_type
      FROM (
    SELECT commision_type,
           commision_fee_type
     FROM  par_fund_seat_para
     WHERE fund_code = p_i_fund_code
       AND market_code = p_i_market_code
       AND seat_code = p_i_seat_code
       AND commision_fee_type ='4'
       AND (stock_kind = p_i_stock_kind OR
            stock_kind = v_all_stock_kind OR stock_kind = '0000')
       AND p_i_date BETWEEN inure_begin_date AND inure_end_date
       ORDER BY stock_kind DESC)
     WHERE rownum = 1;
  EXCEPTION
    WHEN OTHERS THEN
      BEGIN
    SELECT '1'
      INTO v_venture_type
      FROM par_fund_seat_para
     WHERE fund_code = p_i_fund_code
       AND market_code = p_i_market_code
       AND seat_code = '000000'
       AND stock_kind = '0000'
       AND commision_fee_type ='4'
       AND p_i_date BETWEEN inure_begin_date AND inure_end_date
       AND rownum = 1;
  EXCEPTION
    WHEN OTHERS THEN
      v_venture_type := '0';
  END;
  END;

  RETURN v_venture_type;

END ;
