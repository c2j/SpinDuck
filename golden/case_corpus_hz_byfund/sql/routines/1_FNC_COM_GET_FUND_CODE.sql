CREATE OR REPLACE FUNCTION FNC_COM_GET_FUND_CODE (p_i_gddm       IN VARCHAR2,
                                                 p_i_subpartner IN VARCHAR2 DEFAULT '000000',
                                                 p_i_date       IN VARCHAR2,
                                                 p_i_seatcode   IN VARCHAR2)
  RETURN VARCHAR2 IS

  RESULT par_fund_partner.fund_code%TYPE := NULL;

BEGIN
  BEGIN

    SELECT distinct a.fund_code
      INTO RESULT
      FROM PAR_FUND_partner a
     WHERE a.partner_code = p_i_gddm
       AND a.sub_partner_code = p_i_subpartner
       AND p_i_date BETWEEN a.inure_begin_date AND a.inure_end_date
       AND a.fund_code in (select t.fund_code
                           from par_fund_seat t
                          where t.seat_code = p_i_seatcode);
  EXCEPTION
    WHEN OTHERS THEN
      RESULT := 'xxxxxxxx';
  END;
  RETURN RESULT;
END ;
