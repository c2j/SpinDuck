CREATE OR REPLACE FUNCTION FNC_COM_GET_MAIN_XWDM (p_i_fund_code IN VARCHAR2,
                                                 p_i_seat_code IN VARCHAR2,
                                                 p_i_date      IN VARCHAR2)
  RETURN VARCHAR2 IS

  RESULT par_fund_seat.seat_code%TYPE;

  v_count number;

begin

  begin

    SELECT nvl(count(1), 0)
      into v_count
      FROM par_fund_account_member t1,
           par_oper_account_member t2,
           par_fund_seat           t3
     WHERE p_i_date BETWEEN t1.inure_begin_date AND t1.inure_end_date
       AND p_i_date BETWEEN t2.inure_begin_date AND t2.inure_end_date
       AND t2.member_type = '1'
       AND t3.valid_flag = '1'
       AND t3.market_code = '001'
       AND t1.zjzh IS NOT NULL
       AND '20070924' BETWEEN t3.inure_begin_date AND t3.inure_end_date
       AND t1.member_code = t2.member_code
       AND t1.fund_code = t3.fund_code
       AND t3.Seat_Code = p_i_seat_code
       AND t1.fund_code = p_i_fund_code;
  exception
    when OTHERS then
      v_count := 0;
  end;

  begin

    if v_count = 0 then

      BEGIN

        SELECT nvl(t.father_seat, t.seat_code)
          INTO RESULT
          FROM PAR_FUND_seat t
         WHERE t.fund_code = p_i_fund_code
           AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date
           AND t.valid_flag = '1'
           AND t.seat_code = p_i_seat_code;
      EXCEPTION
        WHEN OTHERS THEN
          RESULT := 'xxxxxx';
      END;

    else

      RESULT := p_i_seat_code;

    end if;

    RETURN RESULT;

  end;

END ;
