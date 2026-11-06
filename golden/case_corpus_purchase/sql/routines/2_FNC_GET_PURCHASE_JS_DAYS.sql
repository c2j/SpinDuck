CREATE OR REPLACE FUNCTION FNC_GET_PURCHASE_JS_DAYS (p_i_date        VARCHAR2,
                                                    p_i_security_id VARCHAR2)
  RETURN par_sys_purchase.purchase_days%TYPE IS

  RESULT               par_sys_purchase.purchase_days%TYPE;
  v_purchase_days      par_sys_purchase.purchase_days%TYPE;
  v_repurchase_date    dat_trd_repurchase.purchase_date%TYPE := NULL;
  v_purchase_js_date   dat_trd_repurchase.purchase_date%TYPE := NULL;
  v_repurchase_js_date dat_trd_repurchase.purchase_date%TYPE := NULL;
  v_tradedate_flag     NUMBER(1) := NULL;

BEGIN

  BEGIN
    SELECT t.purchase_days
      INTO v_purchase_days
      FROM par_sys_purchase t
     WHERE t.security_id = p_i_security_id
       AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  EXCEPTION
    WHEN OTHERS THEN
      v_purchase_days := '0';
  END;

  v_repurchase_date := to_char(to_date(p_i_date, 'YYYYMMDD') +
                               v_purchase_days,
                               'YYYYMMDD');

  v_tradedate_flag := fnc_com_if_deal_date(substr(p_i_security_id, 3, 3),
                                           v_repurchase_date);
  IF v_tradedate_flag = 1 THEN
    v_repurchase_date := fnc_com_getday(substr(p_i_security_id, 3, 3),
                                        v_repurchase_date,
                                        1);
  END IF;

  v_purchase_js_date := fnc_com_getday(substr(p_i_security_id, 3, 3),
                                       p_i_date,
                                       1);

  v_repurchase_js_date := fnc_com_getday(substr(p_i_security_id, 3, 3),
                                         v_repurchase_date,
                                         1);

  RESULT := INTERVALTONUM(to_date(v_repurchase_js_date, 'YYYYMMDD') -
            to_date(v_purchase_js_date, 'YYYYMMDD'));

  RETURN RESULT;
END ;
