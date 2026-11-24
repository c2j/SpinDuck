-- N 行 mid_yjqs_detail：全部命中 c_get_data 过滤（bcrq/scdm/非 ES,77/fund_data_mode='1'）
-- 分组键用互质乘子打散 → c_get_data 分组数 ≈ 行数，压测逐组循环体
INSERT INTO mid_yjqs_detail (
  gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,
  cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,ghf_dealer,
  yj01,yj02,yj03,yj04,yj05,yj06,yj07,yj08,yj09,yj10,yj11,yj12,yj13,yj14,yj15,yj16,yj17,yj18,
  fxj,accrual,accrual2,pay_type,ghcjje,cjbh,stock_kind,sub_gddm,
  is_yysg,ywlb,etf_flag,check_type,date_source,cjbz,ghcjje2)
SELECT
  'GD' || ((g * 3) % 200 + 1),
  'X' || ((g * 7) % 10 + 1),
  '001',
  CASE WHEN (g * 11) % 10 = 9 THEN '609' || (100 + (g * 37) % 800)
       ELSE '60' || g END,
  'SEC' || ((g * 13) % 100 + 1),
  CASE WHEN (g * 3) % 2 = 0 THEN '1' ELSE '-1' END,
  CASE WHEN (g * 5) % 3 = 0 THEN '1' ELSE '0' END,
  '20250715',
  100 + (g % 50),
  10000 + g,
  1 + (g % 7) * 0.5, 2 + (g % 5) * 0.25, 0.5, 0.5, 0.5, (g % 7) * 0.1,
  11 + (g % 3) * 0.5, 10.5 + (g % 4), 0.5 + (g % 3), 1.5, 2.5,
  0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5,
  8.5, 9.5, 10.5, 0.25, 0.75,
  3 + (g % 5) * 0.5,
  0.5 + (g % 4) * 0.1,
  0.4 + (g % 3) * 0.1,
  CASE WHEN (g * 7) % 2 = 0 THEN '0' ELSE '1' END,
  10500 + g,
  100000 + g,
  CASE (g * 11) % 10 WHEN 9 THEN '0100' WHEN 8 THEN '0502' ELSE '0501' END,
  CASE WHEN (g * 11) % 10 = 9 THEN '000000' ELSE 'S' || ((g * 11) % 5 + 1) END,
  '0','00','0','0','0',
  CASE WHEN (g * 13) % 4 = 0 THEN '1' ELSE '0' END,
  10560 + g
FROM generate_series(1, __N__) AS t(g);
