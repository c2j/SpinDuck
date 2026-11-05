CREATE OR REPLACE PACKAGE PKG_SPLIT_TRADE_STEP3_SH IS

  PROCEDURE prc_trd_deal_purchase(p_i_date    IN VARCHAR2,
                                  p_o_succeed OUT  VARCHAR2);

  PROCEDURE prc_trd_hz_byfund(p_i_date    IN VARCHAR2,
                              p_o_succeed OUT  VARCHAR2);

END pkg_split_trade_step3_sh;
/
CREATE OR REPLACE PACKAGE BODY PKG_SPLIT_TRADE_STEP3_SH IS

  PROCEDURE prc_trd_hz_byfund(p_i_date    IN VARCHAR2,
                              p_o_succeed OUT  VARCHAR2
                              ) IS

    v_proc_name     db_log.proc_name%TYPE := '';
    v_step_no       db_log.step_no%TYPE := '';
    v_sql_txt       db_log.sql_txt%TYPE := '';
    v_fund_code     par_fund_info.fund_code%TYPE := '';
    v_fxj1          dat_xw_venture.qsje%TYPE := '';
    v_fxj2          dat_xw_venture.qsje%TYPE := '';
    v_xwdm          dat_xw_venture.xwdm%TYPE := '';
    v_trade_type    mid_yjqs_detail.trade_type%TYPE := '';
    v_venture_type  v_par_fund_check_para.venture_type%TYPE := '';
    v_venture_type2 v_par_fund_check_para.venture_type%TYPE := '';
    v_is_venture    v_par_fund_check_para.is_venture%TYPE := '';
    v_accrual       dat_fund_cjqs.accrual%TYPE;
    v_seat_id       par_fund_seat.seat_id%type;
    v_jsf           dat_fund_cjqs.jsf%TYPE;

    CURSOR c_get_venture_data IS
      SELECT a.stock_kind_str,
             a.operation_kind,
             a.market_code
        FROM par_oper_venture a
       WHERE a.market_code = '001';
    r_get_venture_data c_get_venture_data%ROWTYPE;
    TYPE fxj IS REF CURSOR;
    c_fxj fxj;

    CURSOR c_get_unknown IS
      SELECT s.gddm, s.xwdm
        FROM mid_yjqs_detail s
       WHERE s.bcrq = p_i_date
         AND s.scdm = '001'
         AND NOT (s.is_yysg = '1' AND s.ywlb IN ('ES', '77'))
         AND (s.fund_code = 'xxxxxx' OR s.main_xwdm = 'xxxxxx');
    r_get_unknown c_get_unknown%ROWTYPE;

    CURSOR c_get_data IS
      SELECT fund_code,
             a.xwdm,
             a.main_xwdm,
             a.security_id,
             a.bs,
             a.trade_type,
             nvl(SUM(a.cjsl), 0) cjsl,
             nvl(SUM(a.cjje), 0) cjje,
             nvl(SUM(a.yhs), 0) yhs,
             nvl(SUM(a.jsf), 0) jsf,
             nvl(SUM(a.jiesf), 0) jiesf,
             nvl(SUM(a.zgf), 0) zgf,
             nvl(SUM(a.ghf), 0) ghf,
             nvl(SUM(a.ghf_dealer), 0) ghf_dealer,
             nvl(round(SUM(a.fxj), 2), 0) fxj,
             nvl(SUM(a.accrual), 0) accrual,
             nvl(SUM(a.accrual2), 0) accrual2,
             nvl(round(SUM(a.yj01), 2), 0) yj12,
             nvl(round(SUM(a.yj02), 2), 0) yj11,
             nvl(SUM(a.yj03), 0) yj22,
             nvl(SUM(a.yj04), 0) yj21,
             nvl(SUM(a.yj05), 0) yj32,
             nvl(SUM(a.yj06), 0) yj31,
             nvl(round(SUM(a.yj07), 2), 0) yj13,
             nvl(SUM(a.yj08), 0) yj23,
             nvl(SUM(a.yj09), 0) yj33,
             nvl(round(SUM(a.yj10), 2), 0) yj14,
             nvl(SUM(a.yj11), 0) yj24,
             nvl(SUM(a.yj12), 0) yj34,
             nvl(round(SUM(a.yj13), 2), 0) yj15,
             nvl(SUM(a.yj14), 0) yj25,
             nvl(SUM(a.yj15), 0) yj35,
             nvl(SUM(a.yj16), 0) yj16,
             nvl(SUM(a.yj17), 0) yj26,
             nvl(SUM(a.yj18), 0) yj36,
             a.pay_type fffs,
             a.scdm,
             a.stock_kind,
             nvl(SUM(a.cjje), 0) +
             to_number(a.bs) * (nvl(SUM(a.yhs), 0) + nvl(SUM(a.jsf), 0) +
                                nvl(SUM(a.jiesf), 0) + nvl(SUM(a.zgf), 0) +
                                nvl(SUM(a.ghf), 0)) mrcb,
             a.gddm,
             a.sub_gddm gddmzm,
             a.check_type,
             a.date_source,
             a.cjbz,
             a.zqdm
        FROM mid_yjqs_detail a
       WHERE a.bcrq = p_i_date
         AND scdm = '001'
         AND NOT (a.is_yysg = '1' AND a.ywlb IN ('ES', '77'))
         AND (select fnc_get_fund_data_mode(a.bcrq, a.gddm, a.sub_gddm, a.xwdm)) = '1'

       GROUP BY a.fund_code,
                a.xwdm,
                a.bs,
                a.security_id,
                a.stock_kind,
                a.bcrq,
                a.scdm,
                a.trade_type,
                a.pay_type,
                a.main_xwdm,
                a.gddm,
                a.sub_gddm,
                a.check_type,
                a.date_source,
                a.cjbz,
                a.zqdm;

    r_get_data        c_get_data%ROWTYPE;
    v_yj              dat_fund_cjqs.yj%TYPE := '';
    v_commision_type  VARCHAR2(2) := '';
    v_temp_stock_kind par_sys_bond.bond_kind%TYPE := '';

    v_return_rate_flag par_fund_seat_para.return_rate_flag%TYPE := '';
    v_return_rate_org  par_fund_seat_para.return_rate_org%TYPE := '';
    v_return_rate      par_fund_seat_para.return_rate%TYPE := '';

    v_etf_type par_fund_etf.etf_type%TYPE := '';

    v_split_err_msg qfqscjhbrbb_temp.split_err_msg%TYPE;
  BEGIN

    v_step_no   := '1';
    v_proc_name := 'prc_trd_hz_byfund';
    p_o_succeed := '0';

    v_step_no := '2';

    BEGIN
      DELETE FROM dat_fund_cjqs
       WHERE bcrq = p_i_date
         AND scdm = '001';
      COMMIT;
    EXCEPTION
      WHEN OTHERS THEN
        p_o_succeed := '删除原有数据时出错！' || SQLERRM;
        RETURN;
    END;

    UPDATE mid_yjqs_detail t
       SET t.fund_code = fnc_com_get_fund_code(t.gddm,
                                               t.sub_gddm,
                                               p_i_date,
                                               t.xwdm)
     WHERE t.bcrq = p_i_date
       AND scdm = '001';
    COMMIT;

    UPDATE mid_yjqs_detail t
       SET t.main_xwdm = fnc_com_get_main_xwdm(t.fund_code, t.xwdm, p_i_date)
     WHERE t.bcrq = p_i_date
       AND scdm = '001';
    COMMIT;

    BEGIN
      DELETE FROM dat_fund_xw_venture
       WHERE bcrq = p_i_date
         AND scdm = '001';
      COMMIT;
    EXCEPTION
      WHEN OTHERS THEN
        p_o_succeed := '删除原有风险金数据时出错！' || SQLERRM;
        RETURN;
    END;

    OPEN c_get_data;
    LOOP
      <<error_row>>
      FETCH c_get_data
        INTO r_get_data;
      EXIT WHEN c_get_data%NOTFOUND;

      BEGIN

        v_commision_type := fnc_com_get_seat_para(p_i_date,
                                                  r_get_data.scdm,
                                                  r_get_data.fund_code,
                                                  r_get_data.xwdm,
                                                  r_get_data.stock_kind,
                                                  v_return_rate_flag,
                                                  v_return_rate_org,
                                                  v_return_rate);

        BEGIN
          SELECT v.is_venture
            INTO v_is_venture
            FROM v_par_fund_check_para v
           WHERE v.fund_code = r_get_data.fund_code
             AND p_i_date BETWEEN v.inure_begin_date AND v.inure_end_date;
        EXCEPTION
          WHEN OTHERS THEN
            v_is_venture := '0';
        END;
        SELECT CASE v_commision_type
                 WHEN '11' THEN
                  r_get_data.yj11
                 WHEN '12' THEN
                  r_get_data.yj12
                 WHEN '13' THEN
                  r_get_data.yj13
                 WHEN '14' THEN
                  r_get_data.yj14
                 WHEN '15' THEN
                  r_get_data.yj15
                 WHEN '16' THEN
                  r_get_data.yj16
                 WHEN '21' THEN
                  r_get_data.yj21
                 WHEN '22' THEN
                  r_get_data.yj22
                 WHEN '23' THEN
                  r_get_data.yj23
                 WHEN '24' THEN
                  r_get_data.yj24
                 WHEN '25' THEN
                  r_get_data.yj25
                 WHEN '26' THEN
                  r_get_data.yj26
                 WHEN '31' THEN
                  r_get_data.yj31
                 WHEN '32' THEN
                  r_get_data.yj32
                 WHEN '33' THEN
                  r_get_data.yj33
                 WHEN '34' THEN
                  r_get_data.yj34
                 WHEN '35' THEN
                  r_get_data.yj35
                 WHEN '36' THEN
                  r_get_data.yj36
                 ELSE
                  0
               END
          INTO v_yj
          FROM sys_dummy;
      EXCEPTION
        WHEN OTHERS THEN
          v_yj := '0';
      END;

      IF v_return_rate_flag = '1' THEN
        v_yj := r_get_data.cjje * v_return_rate_org -
                r_get_data.cjje * v_return_rate;
      END IF;

      IF r_get_data.stock_kind = '0100'  AND r_get_data.zqdm between '609100' and '609999' THEN
        v_jsf := 0;
        BEGIN
          select nvl(sum(nvl(t.jyfy,0)),0)
            INTO v_jsf
            from jyfy_temp t
           where t.zqdm = r_get_data.zqdm
             and t.gddm = r_get_data.gddm
             and t.data_date = p_i_date
             and t.bs = decode(r_get_data.bs,'1','B','-1','S')
             and t.jydy = r_get_data.xwdm
             and r_get_data.gddmzm='000000'
             and r_get_data.scdm='001';
        EXCEPTION
          WHEN OTHERS THEN
            v_jsf := 0;
        END;
        r_get_data.jsf := v_jsf;
      END IF;

      BEGIN
        SELECT t.bond_kind
          INTO v_temp_stock_kind
          FROM v_par_bond t
         WHERE security_id = r_get_data.security_id
           AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
      EXCEPTION
        WHEN no_data_found THEN
          p_o_succeed := '在债券信息表中查询内码为' || r_get_data.security_id ||
                         '的债券类型不存在，请在债券信息表中维护参数！';
WHEN too_many_rows THEN
          p_o_succeed := '在债券信息表中查询内码为' || r_get_data.security_id ||
                         '的债券类型存在多条重复数据，请在债券信息表中维护参数！';

        WHEN OTHERS THEN
          p_o_succeed := '在债券信息表中查询内码为' || r_get_data.security_id ||
                         '的债券类型出错，其他错误，请联系维护人员解决！' || SQLERRM;
      END;

      BEGIN
        INSERT INTO dat_fund_cjqs
          (fund_code,
           xwdm,
           main_xwdm,
           security_id,
           bs,
           trade_type,
           bcrq,
           cjsl,
           cjje,
           yhs,
           jsf,
           jiesf,
           zgf,
           ghf,
           ghf_dealer,
           yj,
           fxj,
           accrual,
           pay_type,
           scdm,
           stock_kind,
           mrcb,
           accural_tax,
           gddm,
           gddmzm,
           check_type,
           gh_type,
           cjbz)
        VALUES
          (r_get_data.fund_code,
           r_get_data.xwdm,
           r_get_data.main_xwdm,
           r_get_data.security_id,
           r_get_data.bs,
           r_get_data.trade_type,
           p_i_date,
           r_get_data.cjsl,
           r_get_data.cjje +
           decode(r_get_data.fffs, '0', r_get_data.accrual, 0),
           r_get_data.yhs,
           r_get_data.jsf,
           r_get_data.jiesf,
           r_get_data.zgf,
           r_get_data.ghf,
           r_get_data.ghf_dealer,
           v_yj,
           decode(r_get_data.scdm,
                  '001',
                  decode(v_is_venture, '0', 0, r_get_data.fxj),
                  r_get_data.fxj),
           decode(fnc_get_rate_info2(r_get_data.fund_code,
                                     p_i_date,
                                     '3',
                                     v_temp_stock_kind),
                  '0',
                  r_get_data.accrual2,
                  r_get_data.accrual),
           r_get_data.fffs,
           r_get_data.scdm,
           r_get_data.stock_kind,
           (r_get_data.mrcb +
           decode(r_get_data.fffs, '0', r_get_data.accrual, 0) +
           to_number(r_get_data.bs) * v_yj),
           abs(r_get_data.accrual - r_get_data.accrual2),
           r_get_data.gddm,
           r_get_data.gddmzm,
           r_get_data.check_type,
           r_get_data.date_source,
           r_get_data.cjbz);
        COMMIT;
      EXCEPTION
        WHEN OTHERS THEN
          pkg_split_trade_log.inst_log(p_i_date,
                                       '99999999',
                                       '按基金汇总成交金额业务',
                                        '将数据' || '席位代码:'    || r_get_data.XWDM||
                                                  ' ,证券内码:'    || r_get_data.SECURITY_ID||
                                                  ' ,买卖标记:'    || r_get_data.BS||
                                                  ' ,交易类型:'    || r_get_data.TRADE_TYPE||
                                                  ' ,成交日期:'    || p_i_date||
                                                  ' ,付费方式:'    || r_get_data.fffs||
                                                  ' ,基金代码:'    || r_get_data.fund_code||
                                                  ' ,证券类型:'    || r_get_data.STOCK_KIND||
                                                  ' ,市场代码:'    || r_get_data.SCDM||
                                                  ' ,主席位:'    || r_get_data.MAIN_XWDM||
                                                  ' ,股东代码:'    || r_get_data.GDDM||
                                                  ' ,股东代码子码:'|| r_get_data.GDDMZM||
                                                  ' ,核算类型:'	  || r_get_data.CHECK_TYPE||
                                                  ',汇总出错' ,
                                       '280',

                                       '1',
                                       'cf',
                                       p_o_succeed);
          GOTO error_row;

      END;
    END LOOP;
    CLOSE c_get_data;

    OPEN c_get_venture_data;
    LOOP
      <<error_row2>>
      FETCH c_get_venture_data
        INTO r_get_venture_data;
      EXIT WHEN c_get_venture_data%NOTFOUND;

      v_sql_txt := 'SELECT SUM(FXJ),ROUND(NVL(SUM(fxj2),0),2),FUND_CODE,XWDM,TRADE_TYPE,VENTURE_TYPE FROM (' ||
                   'SELECT SUM(NVL(ROUND(fxj,2),0)) FXJ,NVL(SUM(fxj),0) FXJ2,
                           fund_code, bs, xwdm,trade_type,venture_type
                    FROM
                    (select zqdm,fund_code,bs,xwdm,trade_type,SUM(NVL(fxj,0)) FXJ,
                            fnc_com_get_seat_fee_type(BCRQ,SCDM,FUND_CODE,XWDM,STOCK_KIND) VENTURE_TYPE
                     from mid_yjqs_detail
                     WHERE SCDM=''' ||
                   r_get_venture_data.market_code || ''' AND BCRQ = ''' ||
                   p_i_date || ''' and ' ||
                   r_get_venture_data.stock_kind_str ||
                   'AND not (is_yysg=''1'' and ywlb in (''ES'',''77''))' ||
                   'group by zqdm,bs,fund_code,xwdm,trade_type,
                     fnc_com_get_seat_fee_type(BCRQ,SCDM,FUND_CODE,XWDM,STOCK_KIND))' ||
                   ' GROUP BY fund_code, bs, XWDM,TRADE_TYPE,venture_type)' ||
                   ' GROUP BY fund_code, XWDM,TRADE_TYPE,venture_type ';

      BEGIN
        OPEN c_fxj FOR v_sql_txt;
      EXCEPTION
        WHEN OTHERS THEN
          pkg_split_trade_log.inst_log(p_i_date,
                                       '99999999',
                                       '按基金汇总风险金业务',
                                       '插入dat_xw_cjqs出错' || SQLERRM,
                                       '280',

                                       '1',
                                       'cf',
                                       p_o_succeed);
          GOTO error_row2;
      END;
      LOOP
        FETCH c_fxj
          INTO v_fxj1, v_fxj2, v_fund_code, v_xwdm, v_trade_type, v_venture_type2;
        EXIT WHEN c_fxj%NOTFOUND;

        BEGIN
          SELECT venture_type, is_venture
            INTO v_venture_type, v_is_venture
            FROM v_par_fund_check_para
           WHERE fund_code = v_fund_code
             AND p_i_date BETWEEN inure_begin_date AND inure_end_date;
        EXCEPTION
          WHEN OTHERS THEN
            v_venture_type := '0';
            v_is_venture   := '0';
        END;

        BEGIN
          INSERT INTO dat_fund_xw_venture
            (fund_code,
             xwdm,
             bcrq,
             qsje,
             scdm,
             operation_kind,
             trade_type,
             venture_type,
             qsje2)
          VALUES
            (v_fund_code,
             v_xwdm,
             p_i_date,
             decode(r_get_venture_data.market_code,
                    '001',
                    decode(v_is_venture,
                           '0',
                           0,
                           decode(v_venture_type, '1', v_fxj1, v_fxj2)),
                    decode(v_venture_type, '1', v_fxj1, v_fxj2)),
             r_get_venture_data.market_code,
             r_get_venture_data.operation_kind,
             v_trade_type,
             v_venture_type,
             decode(r_get_venture_data.market_code,
                    '001',
                    decode(v_is_venture,
                           '0',
                           0,
                           decode(v_venture_type2, '0', v_fxj2, 0)),
                    decode(v_venture_type2, '0', v_fxj2, 0)));
          COMMIT;
        EXCEPTION
          WHEN OTHERS THEN
            CLOSE c_fxj;
            pkg_split_trade_log.inst_log(p_i_date,
                                         '99999999',
                                         '按基金汇总风险金业务',
                                         '插入dat_xw_cjqs出错' || SQLERRM,
                                         '280',

                                         '1',
                                         'cf',
                                         p_o_succeed);
            GOTO error_row2;
        END;
      END LOOP;
      CLOSE c_fxj;
    END LOOP;
    CLOSE c_get_venture_data;

    OPEN c_get_unknown;
    LOOP
      FETCH c_get_unknown
        INTO r_get_unknown;
      EXIT WHEN c_get_unknown%NOTFOUND;
      pkg_split_trade_log.inst_log(p_i_date,
                                   '99999999',
                                   '未匹配股东代码或席位代码-成交清算汇总业务',
                                   '股东代码为' || r_get_unknown.gddm || '席位代码为' ||
                                   r_get_unknown.xwdm || ',请维护组合信息和席位信息',
                                   '280',

                                   '1',
                                   'cf',
                                   p_o_succeed);
    END LOOP;
    CLOSE c_get_unknown;

    IF p_i_date >= fnc_get_split_date('qfii_dealer_date') THEN
      FOR res IN (SELECT t.zqdm,
                         t.gsdm,
                         decode(t.bs, 'B', '1', 'S', '-1', 'G', '1', 'H', '-1', '0') bs,
                         t.bcrq,
                         SUM(nvl(t.cjsl, 0)) cjsl,
                         SUM(nvl(t.cjje, 0)) cjje,
                         SUM(nvl(t.yhs, 0)) yhs,
                         SUM(nvl(t.jsf, 0)) jsf,
                         SUM(nvl(t.qsf, 0)) qsf,
                         SUM(nvl(t.zgf, 0)) zgf,
                         SUM(nvl(t.ghf, 0)) ghf,
                         SUM(nvl(t.yj, 0)) yj,
                         SUM(nvl(t.fxjj, 0)) fxjj,
                         SUM(nvl(t.yjlx, 0)) yjlx,
                         fnc_get_stock_kind(t.zqdm,p_i_date,'001') stock_kind,
                         t.gddm,
                         nvl(t.gddmzm, '000000') gddmzm
                    FROM qfqscjhbrbb_temp t, dic_stock_kind_sub d
                   WHERE t.bcrq = p_i_date
                     AND d.market_code = '001'
                     and t.bs <> 'I'
                     AND t.zqdm between d.begin_code  and d.end_code
                     AND (EXISTS
                          (SELECT 1
                             FROM par_fund_partner a
                            WHERE a.partner_code = t.gddm
                              AND a.market_code = '001') OR NOT EXISTS
                          (SELECT 1
                             FROM par_fund_partner a
                            WHERE a.partner_code = t.gddm))
                   GROUP BY t.gddm,
                            nvl(t.gddmzm, '000000'),
                            t.bs,
                            t.zqdm,
                            t.bcrq,
                            t.gsdm,
                            d.stock_kind) LOOP
        v_split_err_msg := '';
        DECLARE
          v_fund_code   par_fund_info.fund_code%TYPE;
          v_security_id par_sys_securities.security_id%TYPE;
        BEGIN
          BEGIN
            SELECT t.fund_code
              INTO v_fund_code
              FROM par_fund_partner t
             WHERE t.partner_code = res.gddm
               AND (t.sub_partner_code = res.gddmzm OR
                   t.sub_partner_code = '000000')
               AND t.market_code = '001'
               AND res.bcrq BETWEEN t.inure_begin_date AND t.inure_end_date;
          EXCEPTION
            WHEN OTHERS THEN
              v_split_err_msg := 'QFII券商数据插入dat_fund_cjqs出错！' || '股东代码' ||
                                 res.gddm || '，股东代码子码' || res.gddmzm ||
                                 '未找到对应组合';
              pkg_split_trade_log.inst_log(p_i_date,
                                           '99999999',
                                           'QFII上海成交清算汇总业务',
                                           'QFII券商数据插入dat_fund_cjqs出错！' ||
                                           '股东代码' || res.gddm || '，股东代码子码' ||
                                           res.gddmzm || '未找到对应组合',
                                           '280',
                                           '1',
                                           'cf',
                                               p_o_succeed);
            UPDATE qfqscjhbrbb_temp t
               SET t.split_err_msg = v_split_err_msg
             WHERE t.gddm = res.gddm
               AND nvl(t.gddmzm, '000000') = res.gddmzm
               AND decode(t.bs, 'B', '1', 'S', '-1', 'G', '1', 'H', '-1', '0') = res.bs
               AND t.zqdm = res.zqdm
               AND t.bcrq = res.bcrq
               AND t.gsdm = res.gsdm;
            COMMIT;

              GOTO next_row;
          END;

          BEGIN
            SELECT t.security_id
              INTO v_security_id
              FROM par_sys_securities t
             WHERE t.main_market_code = '001'
               AND t.stock_status = '00'
               AND t.main_stock_code = res.zqdm
               AND res.bcrq BETWEEN t.inure_begin_date AND t.inure_end_date;
          EXCEPTION
            WHEN OTHERS THEN
              v_split_err_msg := 'QFII券商数据插入dat_fund_cjqs出错！' || '证券代码' ||
                                 res.zqdm || '未在系统中维护';
              pkg_split_trade_log.inst_log(p_i_date,
                                           '99999999',
                                           'QFII上海成交清算汇总业务',
                                           'QFII券商数据插入dat_fund_cjqs出错！' ||
                                           '证券代码' || res.zqdm || '未在系统中维护',
                                           '280',
                                           '1',
                                           'cf',
                                           p_o_succeed);
             UPDATE qfqscjhbrbb_temp t
               SET t.split_err_msg = v_split_err_msg
             WHERE t.gddm = res.gddm
               AND nvl(t.gddmzm, '000000') = res.gddmzm
               AND decode(t.bs, 'B', '1', 'S', '-1', 'G', '1', 'H', '-1', '0') = res.bs
               AND t.zqdm = res.zqdm
               AND t.bcrq = res.bcrq
               AND t.gsdm = res.gsdm;
            COMMIT;

             GOTO next_row;
          END;

          begin
            select seat_id
              into v_seat_id
              from par_fund_seat t
             where t.fund_code = v_fund_code
               and t.seat_code = res.gsdm
               and t.market_code = '001'
               and p_i_date between t.inure_begin_date and t.inure_end_date;
          exception
            when OTHERS then
              v_split_err_msg := 'QFII券商数据插入dat_fund_cjqs出错！股东代码' ||
                                 res.gddm || '，股东代码子码' || res.gddmzm || '，席位代码' ||
                                 res.gsdm || '未在系统中维护或者不正确！';
              pkg_split_trade_log.inst_log(p_i_date,
                                           '99999999',
                                           'QFII上海成交清算汇总业务',
                                           'QFII券商数据插入dat_fund_cjqs出错！股东代码' ||
                                 res.gddm || '，股东代码子码' || res.gddmzm || '，席位代码' || res.gsdm || '未在系统中维护或者不正确',
                                           '280',
                                           '1',
                                           'cf',
                                           p_o_succeed);
            UPDATE qfqscjhbrbb_temp t
               SET t.split_err_msg = v_split_err_msg
             WHERE t.gddm = res.gddm
               AND nvl(t.gddmzm, '000000') = res.gddmzm
               AND decode(t.bs, 'B', '1', 'S', '-1', 'G', '1', 'H', '-1', '0') = res.bs
               AND t.zqdm = res.zqdm
               AND t.bcrq = res.bcrq
               AND t.gsdm = res.gsdm;
            COMMIT;
              goto next_row;
          end;

          BEGIN
            v_accrual := round(res.cjsl *
                               fnc_get_bond_yjlx(v_security_id,
                                                 p_i_date,
                                                 0,
                                                 '-1'),
                               2);
          EXCEPTION
            WHEN OTHERS THEN
              v_accrual := res.yjlx;
          END;

          INSERT INTO dat_fund_cjqs
            (fund_code,
             xwdm,
             main_xwdm,
             security_id,
             bs,
             trade_type,
             bcrq,
             cjsl,
             cjje,
             yhs,
             jsf,
             jiesf,
             zgf,
             ghf,
             ghf_dealer,
             yj,
             fxj,
             accrual,
             accural_tax,
             pay_type,
             mrcb,
             scdm,
             stock_kind,
             gddm,
             gddmzm)
          VALUES
            (v_fund_code,
             res.gsdm,
             res.gsdm,
             v_security_id,
             res.bs,
             '0',
             nvl(res.bcrq, 0),
             nvl(res.cjsl, 0),
             nvl(res.cjje + res.yjlx, 0),
             nvl(res.yhs, 0),
             nvl(res.jsf, 0),
             nvl(res.qsf, 0),
             nvl(res.zgf, 0),
             nvl(res.ghf, 0),
             0,
             nvl(res.yj, 0),
             nvl(res.fxjj, 0),
             v_accrual,
             nvl(res.yjlx - v_accrual, 0),
             '1',
             nvl(res.cjje + res.yjlx, 0) +
             to_number(res.bs) *
             (nvl(res.yhs, 0) + nvl(res.jsf, 0) + nvl(res.qsf, 0) +
              nvl(res.zgf, 0) + nvl(res.ghf, 0) + nvl(res.yj, 0)),
             '001',
             res.stock_kind,
             res.gddm,
             res.gddmzm);

       	UPDATE qfqscjhbrbb_temp t
           SET t.split_err_msg = '成功',t.split_flag='0'
         WHERE t.gddm = res.gddm
           AND nvl(t.gddmzm, '000000') = res.gddmzm
           AND decode(t.bs, 'B', '1', 'S', '-1', 'G', '1', 'H', '-1', '0') = res.bs
           AND t.zqdm = res.zqdm
           AND t.bcrq = res.bcrq
           AND t.gsdm = res.gsdm;
        COMMIT;

        EXCEPTION
          WHEN OTHERS THEN
            v_split_err_msg := 'QFII券商数据插入dat_fund_cjqs出错！' || SQLERRM;
            pkg_split_trade_log.inst_log(p_i_date,
                                         '99999999',
                                         'QFII上海成交清算汇总业务',
                                         'QFII券商数据插入dat_fund_cjqs出错' ||
                                         SQLERRM,
                                         '280',
                                         '1',
                                         'cf',
                                         p_o_succeed);
        END;

        <<next_row>>
        NULL;

      END LOOP;
    END IF;
  END ;

  PROCEDURE prc_trd_deal_purchase(p_i_date    IN VARCHAR2,
                                  p_o_succeed OUT  VARCHAR2)
   IS

    v_proc_name db_log.proc_name%TYPE := '';
    v_step_no   db_log.step_no%TYPE := '';

    v_repurchase_date    dat_trd_repurchase.purchase_date%TYPE := '';
    v_purchase_no        dat_trd_repurchase.purchase_no%TYPE := '';
    v_tradedate_flag     NUMBER(1) := 0;
    v_fund_code          par_fund_info.fund_code%TYPE := '';
    v_purchase_days      par_sys_purchase.purchase_days%TYPE := '';
    v_purchase_js_days      par_sys_purchase.purchase_days%TYPE := NULL;
    v_commision_fee_type varchar2(2) := '';

    v_return_rate_flag par_fund_seat_para.return_rate_flag%TYPE := NULL;
    v_return_rate_org  par_fund_seat_para.return_rate_org%TYPE := NULL;
    v_return_rate      par_fund_seat_para.return_rate%TYPE := NULL;
    v_yj               dat_fund_cjqs.yj%TYPE := NULL;
    v_kind             varchar2(2);

    CURSOR c_get_purchase IS
      SELECT qs.gddm,
             qs.scdm market_code,
             qs.gddm partner_code,
             qs.sub_gddm sub_partner_code,
             qs.xwdm seat_code,
             qs.security_id security_id,
             qs.cjbh cjbh,
             qs.jsf purchase_fee,
             qs.fxj purchase_fxj,
             qs.yj06 purchase_yj06,
             qs.yj05 purchase_yj05,
             qs.yj12 purchase_yj12,
             qs.yj18 purchase_yj18,
             qs.yj04 purchase_yj04,
             qs.yj03 purchase_yj03,
             qs.yj11 purchase_yj11,
             qs.yj17 purchase_yj17,
             qs.yj02 purchase_yj02,
             qs.yj01 purchase_yj01,
             qs.yj10 purchase_yj10,
             qs.yj16 purchase_yj16,
             qs.cjje purchase_money,
             qs.cjsl purchase_quantity,
             qs.bcrq purchase_date,

             qs.bs         buy_sale,
             qs.stock_kind stock_kind,
             qs.ghcjje     repurchase_money,
             qs.ghcjje2     repurchase_money_js,
             qs.stock_kind purchase_kind
        FROM mid_yjqs_detail qs
       WHERE substr(qs.stock_kind, 1, 2) = '05'
         and qs.stock_kind <> '0509'
         AND NOT (qs.is_yysg = '1' AND qs.ywlb IN ('ES', '77'))
         AND qs.cjsl > 0
         AND qs.scdm = '001'
         AND qs.bcrq = p_i_date;

    r_get_purchase c_get_purchase%ROWTYPE;

  BEGIN
    v_proc_name := 'PRC_TRD_DEAL_PURCHASE';

    p_o_succeed := '0';

    BEGIN
      DELETE FROM dat_trd_repurchase
       WHERE purchase_date = p_i_date
         AND market_code = '001';
      COMMIT;
    EXCEPTION
      WHEN OTHERS THEN
        p_o_succeed := '清除dat_trd_repurchase数据出错';
        RETURN;
    END;

    OPEN c_get_purchase;
    LOOP
      <<error_row>>
      FETCH c_get_purchase
        INTO r_get_purchase;
      EXIT WHEN c_get_purchase%NOTFOUND;

      BEGIN
        SELECT t.fund_code
          INTO v_fund_code
          FROM par_fund_partner t
         WHERE p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date
           AND t.partner_code = r_get_purchase.gddm
           AND t.sub_partner_code = r_get_purchase.sub_partner_code
           AND t.market_code = r_get_purchase.market_code
           AND t.fund_code IN (SELECT p.fund_code
                                 FROM par_fund_seat p
                                WHERE p.seat_code = r_get_purchase.seat_code
                                  AND p_i_date BETWEEN p.inure_begin_date AND
                                      p.inure_end_date);
      EXCEPTION
        WHEN OTHERS THEN
          pkg_split_trade_log.inst_log(p_i_date,
                                       '99999999',
                                       '回购业务业务拆分',
                                       '股东代码为' || r_get_purchase.gddm ||
                                       ',席位代码为' || r_get_purchase.seat_code ||
                                       '的信息没有在par_fund_parter表中维护',
                                       '210',

                                       '1',
                                       'cf',
                                       p_o_succeed);
          GOTO error_row;
      END;

      BEGIN
        SELECT nvl(t.purchase_days, 1)
          INTO v_purchase_days
          FROM par_sys_purchase t
         WHERE p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date
           AND t.security_id = r_get_purchase.security_id;
      EXCEPTION
        WHEN OTHERS THEN
          pkg_split_trade_log.inst_log(p_i_date,
                                       '99999999',
                                       '回购业务业务拆分',
                                       '取' || r_get_purchase.security_id ||
                                       '回购类型时出错！' || SQLERRM,
                                       '210',

                                       '1',
                                       'cf',
                                       p_o_succeed);
          GOTO error_row;
      END;

      v_purchase_js_days :=FNC_GET_PURCHASE_JS_DAYS(r_get_purchase.purchase_date,r_get_purchase.security_id);

      v_step_no         := '2.2';
      v_purchase_no     := r_get_purchase.purchase_date ||
                           r_get_purchase.cjbh;
      v_repurchase_date := to_char(to_date(r_get_purchase.purchase_date,
                                           'YYYYMMDD') + v_purchase_days,
                                   'YYYYMMDD');

      v_tradedate_flag := fnc_com_if_deal_date(r_get_purchase.market_code,
                                               v_repurchase_date);
      IF v_tradedate_flag = 1 THEN
        v_repurchase_date := fnc_com_getday(r_get_purchase.market_code,
                                            v_repurchase_date,
                                            1);
      END IF;

      BEGIN
       select kind_id
         into v_kind
         from swh_all_kind
        where operation_kind = 'COMMISSION_SWITCH';
      EXCEPTION
       WHEN OTHERS THEN
       v_kind := '0';
      END;

      BEGIN
      IF v_kind = '1' THEN
        v_commision_fee_type := fnc_com_get_seat_para(p_i_date,
                                                  r_get_purchase.market_code,
                                                  v_fund_code,
                                                  r_get_purchase.seat_code,
                                                  r_get_purchase.stock_kind,
                                                  v_return_rate_flag,
                                                  v_return_rate_org,
                                                  v_return_rate);

      SELECT CASE v_commision_fee_type
                 WHEN '11' THEN
                  r_get_purchase.purchase_yj02
                 WHEN '12' THEN
                  r_get_purchase.purchase_yj01
                 WHEN '14' THEN
                  r_get_purchase.purchase_yj10
                 WHEN '16' THEN
                  r_get_purchase.purchase_yj16
                 WHEN '21' THEN
                  r_get_purchase.purchase_yj04
                 WHEN '22' THEN
                  r_get_purchase.purchase_yj03
                 WHEN '24' THEN
                  r_get_purchase.purchase_yj11
                 WHEN '26' THEN
                  r_get_purchase.purchase_yj17
                 WHEN '31' THEN
                  r_get_purchase.purchase_yj06
                 WHEN '32' THEN
                  r_get_purchase.purchase_yj05
                 WHEN '34' THEN
                  r_get_purchase.purchase_yj12
                 WHEN '36' THEN
                  r_get_purchase.purchase_yj18
                 ELSE
                  r_get_purchase.purchase_yj05
               END
          INTO v_yj
          FROM sys_dummy;
      ELSE
        SELECT a.commision_type
          INTO v_commision_fee_type
          FROM (SELECT s.commision_fee_type commision_type
                  FROM par_fund_seat_para s
                 WHERE s.fund_code = v_fund_code
                   AND s.market_code = r_get_purchase.market_code
                   AND s.seat_code IN (r_get_purchase.seat_code, '000000')
                   AND s.stock_kind IN
                       (r_get_purchase.stock_kind,
                        substr(r_get_purchase.stock_kind, 1, 2) || '00',
                        '0000')
                   AND p_i_date BETWEEN s.inure_begin_date AND
                       s.inure_end_date
                 ORDER BY seat_code DESC, stock_kind DESC) a
         WHERE rownum = 1;

               SELECT CASE v_commision_fee_type
                 WHEN '1' THEN
                  r_get_purchase.purchase_yj06
                 WHEN '4' THEN
                  r_get_purchase.purchase_yj12
                 WHEN '6' THEN
                  r_get_purchase.purchase_yj18
                 ELSE
                  r_get_purchase.purchase_yj05
               END
          INTO v_yj
          FROM sys_dummy;
      END IF;
      EXCEPTION
        WHEN OTHERS THEN
          v_yj := '0';
      END;

      v_step_no := '2.4';
      BEGIN
        INSERT INTO dat_trd_repurchase
          (fund_code,
           market_code,
           partner_code,
           security_id,
           purchase_no,
           purchase_fee,
           purchase_fxj,
           purchase_yj,
           purchase_money,
           purchase_quantity,
           purchase_date,
           buy_sale,
           seat_code,
           repurchase_date,
           repurchase_money,
           repurchase_money_js,
           accrual_daily,
           accrual_daily_js,
           purchase_days,
           purchase_days_js,
           purchase_kind,
           gddmzm)
        VALUES
          (v_fund_code,
           r_get_purchase.market_code,
           r_get_purchase.partner_code,
           r_get_purchase.security_id,
           v_purchase_no,
           r_get_purchase.purchase_fee,
           r_get_purchase.purchase_fxj,
           v_yj,
           r_get_purchase.purchase_money,
           abs(r_get_purchase.purchase_quantity),
           r_get_purchase.purchase_date,
           r_get_purchase.buy_sale,
           r_get_purchase.seat_code,
           v_repurchase_date,
           r_get_purchase.repurchase_money,
           r_get_purchase.repurchase_money_js,
           round((r_get_purchase.repurchase_money -
                 r_get_purchase.purchase_money) / v_purchase_days,
                 2),
           round((r_get_purchase.repurchase_money_js -
                 r_get_purchase.purchase_money) / v_purchase_js_days,
                 2),
           v_purchase_days,
           v_purchase_js_days,
           r_get_purchase.purchase_kind,
           r_get_purchase.sub_partner_code);

        COMMIT;
      EXCEPTION
        WHEN OTHERS THEN
          pkg_split_trade_log.inst_log(p_i_date,
                                       '99999999',
                                       '回购业务业务拆分',
                                       '基金代码为' || v_fund_code || '市场代码为' ||
                                       r_get_purchase.market_code ||
                                       '证券内码为' ||
                                       r_get_purchase.security_id || '流水号为' ||
                                       v_purchase_no ||
                                       '插入dat_trd_repurchase时出错',
                                       '210',

                                       '1',
                                       'cf',
                                       p_o_succeed);
          GOTO error_row;
      END;

    END LOOP;
    CLOSE c_get_purchase;
  END ;

END pkg_split_trade_step3_sh;
