-- 参照侧（openGauss 5.0.0 A 模式）等价实现：原生 plpgsql FOR 隐式游标逐行 INSERT（RBAR）。
-- 与被测过程同一逻辑（纯列投影搬运），作为『存过原生执行』基准。
-- openGauss 的 CREATE PROCEDURE 不接受 PG $$ 体（V10/V13），故用 FUNCTION + FOR 隐式游标。
DROP TABLE IF EXISTS mid_yjqs_detail_so;
DROP TABLE IF EXISTS dat_fund_cjqs_so;
CREATE TABLE mid_yjqs_detail_so(gddm VARCHAR, xwdm VARCHAR, scdm VARCHAR, zqdm VARCHAR, security_id VARCHAR, bs VARCHAR, trade_type VARCHAR, bcrq VARCHAR, cjsl BIGINT);
CREATE TABLE dat_fund_cjqs_so(gddm VARCHAR, xwdm VARCHAR, scdm VARCHAR, zqdm VARCHAR, security_id VARCHAR, bs VARCHAR, trade_type VARCHAR, bcrq VARCHAR, cjsl BIGINT);
CREATE OR REPLACE FUNCTION fnc_copy_purchase_so() RETURNS BIGINT AS $$
DECLARE n BIGINT := 0;
BEGIN
  FOR r IN SELECT gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl
           FROM mid_yjqs_detail_so LOOP
    INSERT INTO dat_fund_cjqs_so(gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl)
    VALUES (r.gddm, r.xwdm, r.scdm, r.zqdm, r.security_id, r.bs, r.trade_type, r.bcrq, r.cjsl);
    n := n + 1;
  END LOOP;
  RETURN n;
END;
$$ LANGUAGE plpgsql;