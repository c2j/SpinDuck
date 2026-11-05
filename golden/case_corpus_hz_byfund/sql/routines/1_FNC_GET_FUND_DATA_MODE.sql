CREATE OR REPLACE FUNCTION FNC_GET_FUND_DATA_MODE (p_i_date       IN VARCHAR2,
                                                  p_i_gddm       IN VARCHAR2,
                                                  p_i_subpartner IN VARCHAR2 DEFAULT '000000',
                                                  p_i_seatcode   IN VARCHAR2)
  RETURN VARCHAR2 IS

  RESULT par_fund_info.data_mode%TYPE := NULL;

BEGIN
  BEGIN
    SELECT nvl(t.data_mode, '1')
      INTO RESULT
      FROM par_fund_info t, par_fund_partner a
     WHERE a.partner_code = p_i_gddm
       AND a.sub_partner_code = p_i_subpartner
       AND p_i_date BETWEEN a.inure_begin_date AND a.inure_end_date
       AND  EXISTS
           (SELECT 1
              FROM par_fund_seat t
             WHERE t.seat_code = p_i_seatcode
             AND t.fund_code = a.fund_code)
       AND t.fund_code = a.fund_code
       AND t.fund_status =  '1';
  EXCEPTION
    WHEN OTHERS THEN
      RESULT := '1';
  END;
  RETURN RESULT;
END ;
