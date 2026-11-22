-- byfund 驱动数据：造 N 个 GROUP BY 分组。
-- 主游标 c_get_data 的分组键含 security_id ⇒ 每行一个 distinct security_id
-- 即 N 个分组 = N 次循环体；gddm/sub_gddm 在 A0001|S0001 与 A0002|S0002 之间
-- 交替，保证每组的席位/伙伴查找都命中真实配置行。
-- 第二段把每张证券写进 par_sys_bond（债券主数据已维护）——否则业务分支
-- 报「债券类型不存在」，且集合化入口守卫会因 top1 歧义探测退回
-- Skipped(AmbiguousTop1Tie)，收益消失。见 REPORT §限制。
-- :N 由 sd_bench.py 替换为实际分组数。
INSERT INTO mid_yjqs_detail (gddm,xwdm,scdm,zqdm,security_id,bs,trade_type,bcrq,cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,yj01,fxj,accrual,pay_type,ghcjje,cjbh,yj02,yj03,yj04,stock_kind,yj06,yj05,sub_gddm,yj07,yj08,yj09,is_yysg,ywlb,etf_flag,check_type,date_source,ghcjje2)
SELECT CASE g%2 WHEN 0 THEN 'A0001' ELSE 'A0002' END,
       CASE g%2 WHEN 0 THEN 'X001' ELSE 'X002' END,
       '001','01'||lpad(g::VARCHAR,10,'0'),'013000'||lpad(g::VARCHAR,7,'0'),
       '1','0','20250715',100+g,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',
       10500.000,200000+g,10.50,0,0,'0101',0,0,
       CASE g%2 WHEN 0 THEN 'S0001' ELSE 'S0002' END,0,0,0,'0','00','0','0','0',10560.000
FROM generate_series(1,:N) AS t(g);
INSERT INTO par_sys_bond (security_id,bond_kind,inure_begin_date,inure_end_date)
SELECT '013000'||lpad(g::VARCHAR,7,'0'),'0201','20200101','20991231'
FROM generate_series(1,:N) AS t(g);
