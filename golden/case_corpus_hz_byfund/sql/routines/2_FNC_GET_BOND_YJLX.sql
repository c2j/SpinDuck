CREATE OR REPLACE FUNCTION FNC_GET_BOND_YJLX (p_i_security_id VARCHAR2,
                                             p_i_date        VARCHAR2,
                                             p_i_flag        VARCHAR2 DEFAULT '1',
                                             p_i_change_date VARCHAR2 DEFAULT '0',
                                             p_i_is_qdii VARCHAR2 DEFAULT '0',
                                             p_i_fund_code   VARCHAR2 DEFAULT ''
                                             )
  RETURN NUMBER IS

  r_bond_attr         v_par_bond%ROWTYPE;
  r_bond_section_attr v_par_bond_section%ROWTYPE;
  v_yjlx              NUMBER(19, 12) := 0;
  v_settle_day        par_sys_market_settleday.settle_day%TYPE;
  v_section_id        v_par_bond.section_sum%TYPE;
  v_start_date        VARCHAR2(8) := '';
  v_decimal_digits    v_par_bond.decimal_digits%TYPE;
  v_flag              NUMBER(38) := 0;
  v_next_jxdays       NUMBER(8) := 0;
  v_jxdays            NUMBER(8) := 0;
  v_actdays           NUMBER(8) := 0;
  v_pmll              v_par_bond.pmll%TYPE;
  v_pmll_original     v_par_bond.pmll%TYPE;
  v_alldays           NUMBER(8) := 0;
  v_first_term_start_date        VARCHAR2(8) := '';
  v_term_first_start_date        VARCHAR2(8) := '';
  v_fund_bond_accrual_type      par_fund_bond_accrual_type.bond_accrual_type%TYPE;
  v_fund_pre_bond_accrual_type   par_fund_bond_accrual_type.pre_bond_accrual_type%TYPE;

BEGIN

  IF p_i_change_date = '0' or p_i_change_date = '-1' THEN

     BEGIN
    SELECT *
      INTO r_bond_attr
      FROM v_par_bond t
     WHERE t.security_id = p_i_security_id
     and p_i_date between t.inure_begin_date and t.inure_end_date ;
    EXCEPTION
      WHEN OTHERS THEN
        RETURN 0;
    END;

  ELSE

     BEGIN
    SELECT *
      INTO r_bond_attr
      FROM v_par_bond t
     WHERE t.security_id = p_i_security_id
     and p_i_change_date between t.inure_begin_date and t.inure_end_date ;
    EXCEPTION
      WHEN OTHERS THEN
        RETURN 0;
    END;

  END IF;

  BEGIN
    SELECT t.bond_accrual_type,
           t.pre_bond_accrual_type
      INTO v_fund_bond_accrual_type,
           v_fund_pre_bond_accrual_type
      FROM par_fund_bond_accrual_type t
     WHERE t.fund_code = p_i_fund_code
       AND t.security_id = substr(p_i_security_id, 0, 17) || '00'
       AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  EXCEPTION
    WHEN OTHERS THEN
      v_fund_bond_accrual_type     := '';
      v_fund_pre_bond_accrual_type := '';
  END;

  IF v_fund_bond_accrual_type IS NOT NULL THEN
    r_bond_attr.bond_accrual_type := v_fund_bond_accrual_type ;
  END IF;

  IF v_fund_pre_bond_accrual_type IS NOT NULL THEN
    r_bond_attr.pre_bond_accrual_type := v_fund_pre_bond_accrual_type;
  END IF;

  BEGIN
    SELECT t2.settle_day
      INTO v_settle_day
      FROM par_sys_securities t, par_sys_market_settleday t2
     WHERE t.security_id = p_i_security_id
       AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date
       AND t.main_market_code = t2.market_code;
  EXCEPTION
    WHEN OTHERS THEN
      v_settle_day := 0;
  END;

  BEGIN
    SELECT MIN(a.start_date)
      INTO v_start_date
      FROM (SELECT MAX(t.start_date) over(PARTITION BY section_num ORDER BY seq_no DESC) start_date
              FROM v_par_bond_section t
             WHERE t.security_id = p_i_security_id
               AND ((p_i_date BETWEEN t.start_date AND t.end_date)
                   OR (p_i_date BETWEEN t.start_date AND t.act_cqdate))) a;
  EXCEPTION
    WHEN OTHERS THEN

      RETURN 0;
  END;

  BEGIN
    SELECT MIN(a.start_date)
      INTO v_term_first_start_date
      FROM (SELECT MIN(t.start_date) over(PARTITION BY section_num ORDER BY seq_no) start_date
              FROM v_par_bond_section t
             WHERE t.security_id = p_i_security_id
               AND ((p_i_date BETWEEN t.start_date AND t.end_date)
                   OR (p_i_date BETWEEN t.start_date AND t.act_cqdate))) a;
  EXCEPTION
    WHEN OTHERS THEN

      RETURN 0;
  END;

  BEGIN
    SELECT t.*
      INTO r_bond_section_attr
      FROM v_par_bond_section t
     WHERE t.security_id = p_i_security_id
       AND t.start_date = v_start_date
       AND ((p_i_date BETWEEN t.start_date AND t.act_cqdate) OR
           (p_i_date BETWEEN t.start_date AND t.end_date));
  EXCEPTION
    WHEN OTHERS THEN
      RETURN 0;
  END;

  v_section_id     := r_bond_section_attr.section_num;
  v_decimal_digits := nvl(r_bond_attr.decimal_digits, 8);

  IF p_i_date = r_bond_section_attr.end_date THEN

    v_flag := 1;
  ELSIF p_i_date > r_bond_section_attr.end_date THEN
    v_flag := 2;
  ELSE

    IF p_i_date >= r_bond_section_attr.act_cqdate THEN
      v_flag := 3;
    ELSE

      v_flag := 0;
    END IF;
  END IF;

  IF p_i_flag = '0' THEN

    v_pmll          := r_bond_section_attr.pmll;
    v_pmll_original := r_bond_section_attr.pmll;
  ELSif p_i_flag ='1' then

    v_pmll          := r_bond_section_attr.pre_pmll;
    v_pmll_original := r_bond_section_attr.pre_pmll;
  ELSIF p_i_flag ='2' THEN

    v_pmll          := r_bond_section_attr.pre_pmll;
    v_pmll_original := r_bond_section_attr.pre_pmll;
  else
    v_pmll          :=r_bond_section_attr.pre_pmll*(1-to_number(p_i_flag));
    v_pmll_original :=r_bond_section_attr.pre_pmll*(1-to_number(p_i_flag));
  END IF;

  BEGIN
    IF (r_bond_attr.bond_accrual_type='2' AND
       ( r_bond_attr.duixi_mode = '1' OR r_bond_attr.duixi_mode = '2' OR
         r_bond_attr.duixi_mode = '5' OR r_bond_attr.duixi_mode = '4' )) OR r_bond_attr.bond_accrual_type = '9' THEN
      select t.start_date
      into v_first_term_start_date
      FROM v_par_bond_section t
      WHERE t.security_id = p_i_security_id
        AND t.section_num = '1'
        AND t.seq_no = '1';
    ELSE
      v_first_term_start_date := '0';
    END IF;
  EXCEPTION
    WHEN OTHERS THEN
      RETURN 0;
  END;
  IF p_i_is_qdii ='1' THEN
     prc_com_get_jxdays_qdii(r_bond_attr.bond_accrual_type,
                     r_bond_attr.duixi_mode,
                     p_i_date,
                     r_bond_section_attr.start_date,
                     r_bond_section_attr.end_date,
                     v_first_term_start_date,
                     v_term_first_start_date,
                     p_i_is_qdii,
                     v_jxdays,
                     v_actdays);
  ELSE

  if(r_bond_attr.bond_kind = '0601' and v_flag = '1')
  then
     prc_com_get_jxdays(r_bond_attr.PRE_bond_accrual_type,
                     r_bond_attr.duixi_mode,
                     p_i_date,
                     r_bond_section_attr.start_date,
                     r_bond_section_attr.end_date,
                     v_first_term_start_date,
                     v_term_first_start_date,
                     v_jxdays,
                     v_actdays);
  else
  prc_com_get_jxdays(r_bond_attr.BOND_ACCRUAL_TYPE,
                     r_bond_attr.duixi_mode,
                     p_i_date,
                     r_bond_section_attr.start_date,
                     r_bond_section_attr.end_date,
                     v_first_term_start_date,
                     v_term_first_start_date,
                     v_jxdays,
                     v_actdays);
  end if;

  END IF;

	IF p_i_flag = '2' THEN
	  v_actdays := 360;
	END IF;

	IF r_bond_attr.bond_type = '0' THEN
    IF r_bond_attr.bond_accrual_type = '4' OR r_bond_attr.bond_accrual_type = '10' THEN

      v_pmll := round(v_pmll_original * (r_bond_attr.divident_section /
                      r_bond_attr.section_sum) / 12,
                      12);
    END IF;

  END IF;
  IF r_bond_section_attr.seq_no = 1 THEN

    IF (v_flag = 3 OR
       (v_flag = 1 AND NOT (r_bond_attr.divident_section < 12 AND
        r_bond_attr.section_sum = 1) AND
       (r_bond_attr.divident_section / r_bond_attr.section_sum) < 12)) AND (r_bond_attr.PRE_BOND_ACCRUAL_TYPE = '4' OR r_bond_attr.PRE_BOND_ACCRUAL_TYPE = '10') AND p_i_change_date<>'-1' THEN

        v_yjlx := round(v_pmll_original * (r_bond_attr.divident_section /
                        r_bond_attr.section_sum) / 12 * NVL(r_bond_section_attr.factor, 1),
                        12);
    ELSE
      IF v_flag = 2 THEN

        v_next_jxdays := fnc_com_get_between_days(TO_CHAR(to_date(r_bond_section_attr.end_date, 'YYYYMMDD') + 1, 'YYYYMMDD'),
                                                  p_i_date);

        v_yjlx := round(v_pmll_original * (r_bond_attr.divident_section /
                        r_bond_attr.section_sum) / 12 * NVL(r_bond_section_attr.factor, 1),
                        12) +
                  round(nvl(v_pmll * v_next_jxdays / v_actdays * NVL(r_bond_section_attr.factor, 1), 0),
                        v_decimal_digits);
      ELSE

        v_yjlx := round(nvl(v_pmll * v_jxdays * NVL(r_bond_section_attr.factor, 1) / v_actdays, 0),
                        v_decimal_digits);

      END IF;
    END IF;
  ELSE

    BEGIN
      SELECT SUM(round(nvl(decode(r_bond_attr.bond_type,
                                  '0',
                                  decode(p_i_flag,
                                         '0',
                                         nvl(t.pmll, 0),
                                         nvl(t.pre_pmll, 0)),
                                  '1',
                                  round(nvl(decode(p_i_flag,
                                                   '0',
                                                   nvl(t.pmll, 0),
                                                   nvl(t.pre_pmll, 0)) *
                                            v_actdays /
                                            fnc_com_get_between_jxdays(t.start_date,
                                                                       t.end_date,
                                                                       r_bond_attr.bond_accrual_type),
                                            0),
                                        nvl(r_bond_attr.decimal_digits, 8)),
                                  t.pmll) *
                           fnc_com_get_between_jxdays(t.start_date,
                                                      t.end_date,
                                                      r_bond_attr.bond_accrual_type) /
                           v_actdays,
                           0),
                       nvl(r_bond_attr.decimal_digits, 8)))

        INTO v_yjlx
        FROM v_par_bond_section t
       WHERE t.security_id = p_i_security_id
         AND t.section_num = v_section_id
         AND t.seq_no < r_bond_section_attr.seq_no;
    EXCEPTION
      WHEN OTHERS THEN
        v_yjlx := 0;
    END;

    v_yjlx := v_yjlx * NVL(r_bond_section_attr.factor, 1)  +
              round(nvl(v_pmll * v_jxdays * NVL(r_bond_section_attr.factor, 1)  / v_actdays, 0), v_decimal_digits);

  END IF;

  RETURN v_yjlx;
END ;
