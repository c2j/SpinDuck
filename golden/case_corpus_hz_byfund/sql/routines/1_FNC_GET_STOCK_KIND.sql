CREATE OR REPLACE FUNCTION FNC_GET_STOCK_KIND (p_i_stock_code  IN VARCHAR2,
                                              p_i_date        IN VARCHAR2,
                                              p_i_market_code IN VARCHAR2)
  RETURN VARCHAR2 IS
  stock_kind    par_sys_stock.stock_kind%TYPE;
  v_security_id par_sys_securities.security_id%TYPE;
  v_stock_kind  par_sys_securities.stock_kind%TYPE;

BEGIN

    BEGIN
      SELECT t.stock_kind
        INTO stock_kind
        FROM dic_stock_kind_sub t
       WHERE p_i_stock_code BETWEEN t.begin_code AND t.end_code
         AND t.market_code = p_i_market_code

         AND rownum = 1;
    EXCEPTION
      WHEN OTHERS THEN
        stock_kind := 'AAAA';
    END;

  RETURN stock_kind;
END ;
