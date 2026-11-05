-- 被测例程：按基金+证券聚合成交，费率 0.0008，向下取整到分后入指令表。
-- 同时演示：DECLARE 初始化、显式游标、EXCEPTION OTHERS 兜底、函数调用。
CREATE OR REPLACE FUNCTION fnc_fee(p_amt IN NUMERIC) RETURN NUMERIC IS
BEGIN
  RETURN ROUND(p_amt * 0.0008, 2);
END;
/

CREATE OR REPLACE PROCEDURE prc_agg_fund(p_i_bcrq IN VARCHAR) IS
  v_fund  VARCHAR(10);
  v_scdm  VARCHAR(10);
  v_sl    NUMERIC(19,4);
  v_je    NUMERIC(19,2);
  v_n     NUMBER := 0;
  CURSOR c_data IS
    SELECT fund_code, scdm, SUM(cjsl) AS sl, SUM(cjsl * cjjg) AS je
      FROM mid_yjqs_detail
     WHERE bcrq = p_i_bcrq AND cjbz = 'Y'
     GROUP BY fund_code, scdm
     ORDER BY fund_code, scdm;
BEGIN
  DELETE FROM dat_inst_fund_cjqs WHERE bcrq = p_i_bcrq;
  OPEN c_data;
  LOOP
    FETCH c_data INTO v_fund, v_scdm, v_sl, v_je;
    EXIT WHEN c_data%NOTFOUND;
    INSERT INTO dat_inst_fund_cjqs(bcrq, fund_code, scdm, cjsl, cjje, fee)
    VALUES (p_i_bcrq, v_fund, v_scdm, v_sl, ROUND(v_je, 2), fnc_fee(v_je));
    v_n := v_n + 1;
  END LOOP;
  CLOSE c_data;
EXCEPTION
  WHEN OTHERS THEN
    INSERT INTO dat_inst_fund_cjqs(bcrq, fund_code, scdm, cjsl, cjje, fee)
    VALUES (p_i_bcrq, 'ERR', 'ERR', 0, 0, 0);
END;
/
