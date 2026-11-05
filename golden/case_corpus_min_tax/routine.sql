CREATE OR REPLACE FUNCTION FNC_COM_GET_MIN_TAX(p_i_market_code IN VARCHAR2,
                                               p_i_par_code    IN VARCHAR2,
                                               p_i_stock_kind  IN VARCHAR2,
                                               p_i_work_date   IN VARCHAR2,
                                               p_i_gddm        IN VARCHAR2 DEFAULT NULL,
                                               p_i_sub_gddm    IN VARCHAR2 DEFAULT NULL)
  RETURN NUMBER IS
  RESULT           par_sys_marketrate.min_tax%TYPE := 0;
  v_all_stock_kind par_sys_marketrate.stock_kind%TYPE;
BEGIN
  v_all_stock_kind := substr(p_i_stock_kind, 1, 2) || '00';

  BEGIN
    SELECT min_tax
      INTO RESULT
      FROM (SELECT min_tax
              FROM par_sys_marketrate
             WHERE market_code = p_i_market_code
               AND par_code = p_i_par_code
               AND (stock_kind = p_i_stock_kind OR
                    stock_kind = v_all_stock_kind OR
                    stock_kind = '0000')
               AND p_i_work_date BETWEEN inure_begin_date AND inure_end_date
             ORDER BY stock_kind DESC)
     WHERE rownum = 1;
  EXCEPTION
    WHEN OTHERS THEN
      RESULT := 0;
  END;

  RETURN nvl(RESULT, 0);
END;
