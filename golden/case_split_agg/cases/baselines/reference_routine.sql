-- 参照侧（openGauss / GaussDB 内核血统）等价实现。
-- 语法差异说明：openGauss 5.0.0 的 CREATE PROCEDURE 不接受 PG 风格 $$ 体，
-- 且 DECLARE 中的游标声明仅支持 FOR 形式之外的用法受限；因此参照侧用
-- FUNCTION + FOR 隐式游标表达同一逻辑，被测侧（routine.sql）用
-- PROCEDURE + 显式游标。两侧语义对齐由结果表逐行比对保证。
CREATE OR REPLACE FUNCTION fnc_fee(p_amt NUMERIC) RETURNS NUMERIC AS $$
BEGIN
  RETURN ROUND(p_amt * 0.0008, 2);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION fn_agg_fund(p_i_bcrq IN VARCHAR) RETURNS INT AS $$
DECLARE
  v_n INT := 0;
  v_sl NUMERIC(19,4);
  v_je NUMERIC(19,2);
BEGIN
  DELETE FROM dat_inst_fund_cjqs WHERE bcrq = p_i_bcrq;
  FOR r IN
    SELECT fund_code, scdm, SUM(cjsl) AS sl, SUM(cjsl * cjjg) AS je
      FROM mid_yjqs_detail
     WHERE bcrq = p_i_bcrq AND cjbz = 'Y'
     GROUP BY fund_code, scdm
     ORDER BY fund_code, scdm
  LOOP
    v_sl := r.sl;
    v_je := ROUND(r.je, 2);
    INSERT INTO dat_inst_fund_cjqs(bcrq, fund_code, scdm, cjsl, cjje, fee)
    VALUES (p_i_bcrq, r.fund_code, r.scdm, v_sl, v_je, fnc_fee(r.je));
    v_n := v_n + 1;
  END LOOP;
  RETURN v_n;
EXCEPTION
  WHEN OTHERS THEN
    RETURN -1;
END;
$$ LANGUAGE plpgsql;
