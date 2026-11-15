-- SpinDuck 被测过程：purchase 核心『静态游标读 → 纯列投影 INSERT』套路。
-- 这是 PR#48 集合化可转换的六指令模式（无表达式/分支/异常/子调用/多写）。
-- OFF：解释器逐行 FETCH+INSERT（RBAR）；ON：整体下推为 INSERT...SELECT。
CREATE OR REPLACE PROCEDURE prc_copy_purchase_so IS
BEGIN
  FOR r IN SELECT gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl
           FROM mid_yjqs_detail_so LOOP
    INSERT INTO dat_fund_cjqs_so(gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl)
    VALUES (r.gddm, r.xwdm, r.scdm, r.zqdm, r.security_id, r.bs, r.trade_type, r.bcrq, r.cjsl);
  END LOOP;
END;
/