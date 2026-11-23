-- purchase 驱动数据：N 行 mid_yjqs_detail。
-- 主游标 c_get_data 是逐行 SELECT（无 GROUP BY）⇒ N 行 = N 次循环体，
-- 每行都命中谓词（bcrq/scdm/stock_kind/cjsl），与存过侧同口径。
-- :N 由 sd_bench.py 替换为实际行数。
INSERT INTO mid_yjqs_detail (gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,yj01,fxj,accrual,pay_type,ghcjje,cjbh,yj02,yj03,yj04,stock_kind,yj06,yj05,sub_gddm,yj07,yj08,yj09,is_yysg,ywlb,etf_flag,check_type,date_source,ghcjje2)
SELECT 'A0001','X001','001','600000','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,100000+g,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000
FROM generate_series(1,:N) AS t(g);
